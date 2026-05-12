//! Per-zone wiki dump for LLM consumption.
//!
//! Walks `Category:Zones` + `Category:Cities` on `wiki.guildwars2.com`
//! and, for every page that resolves to a `/v2/maps` id, emits a
//! markdown section with:
//!
//! - The map id + name
//! - The `Location infobox` structured fields (`type`, `id`,
//!   `requires`, `within`, `connections`)
//! - The intro paragraph (the lead prose before any `==` heading)
//! - Any sub-section whose H2/H3 heading matches `(getting there |
//!   location | access | portal | entrance)` (case-insensitive)
//!
//! Output is a single `data/wiki_zone_access.md` file by default —
//! ~150 entries, sorted by map id, ready to paste into an LLM context.
//!
//! Run from the repo root:
//!
//! ```text
//! cargo run -p scrape-wiki-access --release -- --out data/wiki_zone_access.md
//! ```
//!
//! The goal is NOT to make `get_map_neighbors` smarter; the goal is to
//! give an operator (or an LLM) enough context to draft
//! `data/map_neighbors_overrides.yaml` patches when the wiki has info
//! the infobox alone doesn't capture (story-gated portals, festival
//! access, passkey-gated lounges).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;

const GW2_API_BASE: &str = "https://api.guildwars2.com/v2";
const WIKI_API_BASE: &str = "https://wiki.guildwars2.com/api.php";
const USER_AGENT: &str = concat!(
    "gw2-mcp-wiki-access-scraper/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);

/// Section-heading filter — case-insensitive substring match against
/// the heading text (stripped of `==` markers). Tuned narrow: most
/// zone pages have a "Getting there" section; broader matches (like
/// "Locations") would drag in the wiki's POI tables which add a lot
/// of bytes without helping routing decisions.
const ACCESS_SECTION_PATTERNS: &[&str] = &[
    "getting there",
    "how to get",
    "access",
    "portal",
    "asura gate",
    "entrance",
];

#[derive(Parser, Debug)]
#[command(
    name = "scrape-wiki-access",
    about = "Dump every zone page's access-related prose into a single LLM-readable markdown file."
)]
struct Args {
    /// Output markdown path.
    #[arg(long, default_value = "data/wiki_zone_access.md")]
    out: std::path::PathBuf,
    /// Limit to the first N zone pages (debugging / sampling).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GwMap {
    id: u32,
    name: String,
    #[serde(default)]
    region_name: Option<String>,
    #[serde(default)]
    min_level: Option<u32>,
    #[serde(default)]
    max_level: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;

    eprintln!("[1/3] Fetching /v2/maps?ids=all …");
    let maps = fetch_all_maps(&client).await?;
    eprintln!("    {} maps loaded.", maps.len());
    let name_to_id = build_name_index(&maps);

    eprintln!("[2/3] Enumerating Category:Zones + Category:Cities …");
    let mut pages = list_category_members(&client, "Category:Zones").await?;
    let city_pages = list_category_members(&client, "Category:Cities").await?;
    for c in &city_pages {
        if !pages.contains(c) {
            pages.push(c.clone());
        }
    }
    pages.sort();
    pages.dedup();
    if let Some(n) = args.limit {
        pages.truncate(n);
        eprintln!("    Limited to first {n} for this run.");
    }
    eprintln!("    {} pages total.", pages.len());

    eprintln!("[3/3] Fetching + parsing each page's wikitext …");
    // Keyed by map id so the output is sorted deterministically.
    let mut entries: BTreeMap<u32, MarkdownEntry> = BTreeMap::new();
    let mut skipped: usize = 0;
    let total = pages.len();
    for (i, page) in pages.iter().enumerate() {
        if i % 20 == 0 {
            eprintln!("    [{}/{}] {}", i + 1, total, page);
        }
        match parse_page(&client, page, &maps, &name_to_id).await {
            Ok(Some(e)) => {
                entries.insert(e.map_id, e);
            }
            Ok(None) => skipped += 1,
            Err(e) => {
                eprintln!("    ! page {page} failed: {e}");
                skipped += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    eprintln!(
        "    {} entries kept, {} skipped (no infobox / no id / WvW / non-public)",
        entries.len(),
        skipped,
    );

    eprintln!("[done] Writing {} …", args.out.display());
    let body = render_markdown(&entries);
    std::fs::write(&args.out, body).with_context(|| format!("writing {}", args.out.display()))?;
    eprintln!("    wrote {} entries.", entries.len());
    Ok(())
}

#[derive(Debug)]
struct MarkdownEntry {
    map_id: u32,
    name: String,
    region_name: Option<String>,
    min_level: Option<u32>,
    max_level: Option<u32>,
    infobox: BTreeMap<String, String>,
    intro: String,
    /// Pairs of `(heading, body)` for every section that matched the
    /// access-prose patterns. Body is wikitext lightly cleaned for
    /// reading.
    sections: Vec<(String, String)>,
}

async fn fetch_all_maps(client: &reqwest::Client) -> Result<BTreeMap<u32, GwMap>> {
    let url = format!("{GW2_API_BASE}/maps?ids=all");
    let resp = client.get(&url).send().await?.error_for_status()?;
    let raw: Vec<GwMap> = resp.json().await?;
    let mut out = BTreeMap::new();
    for m in raw {
        out.insert(m.id, m);
    }
    Ok(out)
}

fn build_name_index(maps: &BTreeMap<u32, GwMap>) -> BTreeMap<String, u32> {
    let mut idx = BTreeMap::new();
    for (id, m) in maps {
        idx.insert(m.name.trim().to_lowercase(), *id);
    }
    idx
}

async fn list_category_members(client: &reqwest::Client, category: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cont: Option<String> = None;
    loop {
        let mut q = vec![
            ("action", "query".to_owned()),
            ("list", "categorymembers".to_owned()),
            ("cmtitle", category.to_owned()),
            ("cmlimit", "500".to_owned()),
            ("cmtype", "page".to_owned()),
            ("format", "json".to_owned()),
            ("formatversion", "2".to_owned()),
        ];
        if let Some(c) = cont.clone() {
            q.push(("cmcontinue", c));
        }
        let resp = client
            .get(WIKI_API_BASE)
            .query(&q)
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = resp.json().await?;
        if let Some(members) = body["query"]["categorymembers"].as_array() {
            for m in members {
                if let Some(title) = m["title"].as_str() {
                    out.push(title.to_owned());
                }
            }
        }
        match body["continue"]["cmcontinue"].as_str() {
            Some(next) => cont = Some(next.to_owned()),
            None => break,
        }
    }
    Ok(out)
}

async fn fetch_wikitext(client: &reqwest::Client, page: &str) -> Result<String> {
    let q = [
        ("action", "parse"),
        ("page", page),
        ("prop", "wikitext"),
        ("format", "json"),
        ("formatversion", "2"),
    ];
    let resp = client
        .get(WIKI_API_BASE)
        .query(&q)
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    let text = body["parse"]["wikitext"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no wikitext for {page}"))?
        .to_owned();
    Ok(text)
}

async fn parse_page(
    client: &reqwest::Client,
    page: &str,
    maps: &BTreeMap<u32, GwMap>,
    _name_to_id: &BTreeMap<String, u32>,
) -> Result<Option<MarkdownEntry>> {
    let wikitext = fetch_wikitext(client, page).await?;
    let Some(infobox_body) = extract_location_infobox(&wikitext) else {
        return Ok(None);
    };
    let params = parse_infobox_params(&infobox_body);

    // Same filters the map-neighbors scraper applies — keep the two
    // outputs aligned on what "in scope" means.
    let kind = params.get("type").map(|s| s.to_lowercase());
    match kind.as_deref() {
        Some("zone") | Some("city") => {}
        _ => return Ok(None),
    }
    let Some(map_id) = params.get("id").and_then(|s| s.trim().parse::<u32>().ok()) else {
        return Ok(None);
    };
    if !maps.contains_key(&map_id) {
        return Ok(None);
    }
    if let Some(within) = params.get("within") {
        let lower = within.to_lowercase();
        if lower.contains("world vs. world") || lower.contains("world vs world") {
            return Ok(None);
        }
    }

    let intro = extract_intro(&wikitext);
    let sections = extract_access_sections(&wikitext);

    let gw2_meta = maps.get(&map_id);
    Ok(Some(MarkdownEntry {
        map_id,
        name: gw2_meta
            .map(|m| m.name.clone())
            .unwrap_or_else(|| page.to_owned()),
        region_name: gw2_meta.and_then(|m| m.region_name.clone()),
        min_level: gw2_meta.and_then(|m| m.min_level),
        max_level: gw2_meta.and_then(|m| m.max_level),
        infobox: params,
        intro,
        sections,
    }))
}

/// Pull the body of `{{Location infobox …}}` out of the wikitext.
fn extract_location_infobox(wikitext: &str) -> Option<String> {
    let lower = wikitext.to_lowercase();
    let start_marker = "{{location infobox";
    let mut start = lower.find(start_marker)?;
    start += start_marker.len();
    let bytes = wikitext.as_bytes();
    let mut depth = 1;
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(wikitext[start..i].to_owned());
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    None
}

fn parse_infobox_params(infobox: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut buf = String::new();
    let mut depth = 0i32;
    let mut link_depth = 0i32;
    let push = |out: &mut BTreeMap<String, String>, key: &Option<String>, buf: &str| {
        if let Some(k) = key {
            out.insert(k.trim().to_lowercase(), buf.trim().to_owned());
        }
    };
    let bytes = infobox.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            depth += 1;
            buf.push_str("{{");
            i += 2;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'}' && bytes[i + 1] == b'}' {
            depth -= 1;
            buf.push_str("}}");
            i += 2;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b'[' {
            link_depth += 1;
            buf.push_str("[[");
            i += 2;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b']' && bytes[i + 1] == b']' {
            link_depth -= 1;
            buf.push_str("]]");
            i += 2;
            continue;
        }
        if bytes[i] == b'|' && depth == 0 && link_depth == 0 {
            push(&mut out, &current_key, &buf);
            buf.clear();
            current_key = None;
            i += 1;
            let mut key = String::new();
            while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b'|' {
                if bytes[i] == b'{' || bytes[i] == b'}' {
                    break;
                }
                key.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'=' {
                current_key = Some(key);
                i += 1;
            }
            continue;
        }
        buf.push(bytes[i] as char);
        i += 1;
    }
    push(&mut out, &current_key, &buf);
    out
}

/// Everything between the closing `}}` of the Location infobox and the
/// first `==` heading is the lead / intro.
fn extract_intro(wikitext: &str) -> String {
    let lower = wikitext.to_lowercase();
    let Some(infobox_start) = lower.find("{{location infobox") else {
        return String::new();
    };
    // Skip past the infobox's closing `}}`.
    let bytes = wikitext.as_bytes();
    let mut depth = 0i32;
    let mut i = infobox_start;
    let mut after_infobox = None;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                after_infobox = Some(i);
                break;
            }
            continue;
        }
        i += 1;
    }
    let Some(start) = after_infobox else {
        return String::new();
    };
    let rest = &wikitext[start..];
    let end = rest.find("\n==").unwrap_or(rest.len());
    clean_wiki_markup(rest[..end].trim()).trim().to_owned()
}

/// Walk every H2/H3 section in the wikitext and keep the ones whose
/// heading matches an access-related pattern.
fn extract_access_sections(wikitext: &str) -> Vec<(String, String)> {
    let header_re =
        Regex::new(r"(?m)^(={2,4})\s*([^=]+?)\s*={2,4}\s*$").expect("static regex compiles");
    let lower_patterns: Vec<&str> = ACCESS_SECTION_PATTERNS.to_vec();
    let mut headers: Vec<(usize, usize, String)> = Vec::new();
    for c in header_re.captures_iter(wikitext) {
        let m = c.get(0).unwrap();
        let heading = c.get(2).map_or("", |m| m.as_str()).trim().to_owned();
        headers.push((m.start(), m.end(), heading));
    }
    let mut out = Vec::new();
    for (i, (start, body_start, heading)) in headers.iter().enumerate() {
        let lower = heading.to_lowercase();
        if !lower_patterns.iter().any(|p| lower.contains(p)) {
            continue;
        }
        let body_end = headers
            .get(i + 1)
            .map_or(wikitext.len(), |(next_start, _, _)| *next_start);
        let body = &wikitext[*body_start..body_end];
        let cleaned = clean_wiki_markup(body).trim().to_owned();
        if cleaned.is_empty() {
            continue;
        }
        out.push((heading.clone(), cleaned));
        // Silence unused-variable lint without renaming the binding
        // (clearer this way for the heading-table format).
        let _ = start;
    }
    out
}

/// Light wikitext → readable-text pass. Keeps the prose recognisable
/// without dragging in a full wikitext parser.
fn clean_wiki_markup(raw: &str) -> String {
    // Strip HTML comments first.
    let re_comment = Regex::new(r"<!--.*?-->").expect("static regex");
    let s = re_comment.replace_all(raw, "");
    // Strip ref tags.
    let re_ref = Regex::new(r"(?is)<ref[^>]*>.*?</ref>").expect("static regex");
    let s = re_ref.replace_all(&s, "");
    let re_ref_self = Regex::new(r"(?i)<ref[^>]*/>").expect("static regex");
    let s = re_ref_self.replace_all(&s, "");
    // <br> → newline.
    let re_br = Regex::new(r"(?i)<br\s*/?>").expect("static regex");
    let s = re_br.replace_all(&s, "\n");
    // Files / images — drop entirely.
    let re_file = Regex::new(r"\[\[(?:File|Image):[^\]]*\]\]").expect("static regex");
    let s = re_file.replace_all(&s, "");
    // [[Page|Display]] → Display
    let re_link_disp = Regex::new(r"\[\[([^\]|]+)\|([^\]]+)\]\]").expect("static regex");
    let s = re_link_disp.replace_all(&s, "$2");
    // [[Page]] → Page
    let re_link = Regex::new(r"\[\[([^\]]+)\]\]").expect("static regex");
    let s = re_link.replace_all(&s, "$1");
    // '''bold''' → **bold**
    let re_bold = Regex::new(r"'''(.*?)'''").expect("static regex");
    let s = re_bold.replace_all(&s, "**$1**");
    // ''italic'' → *italic*
    let re_italic = Regex::new(r"''(.*?)''").expect("static regex");
    let s = re_italic.replace_all(&s, "*$1*");
    // Strip any leftover Template:Foo invocations that survived the
    // infobox extraction (they show up in body text occasionally as
    // {{api|x}} etc.). Keep this conservative — drop the wrapper but
    // keep the first argument's text.
    let re_tmpl = Regex::new(r"\{\{([^{}|]+)\|([^{}]*)\}\}").expect("static regex");
    let s = re_tmpl.replace_all(&s, "$2");
    // Empty {{Template}} — drop.
    let re_tmpl_empty = Regex::new(r"\{\{[^{}]*\}\}").expect("static regex");
    let s = re_tmpl_empty.replace_all(&s, "");
    s.to_string()
}

fn render_markdown(entries: &BTreeMap<u32, MarkdownEntry>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Guild Wars 2 wiki access-prose dump");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Generated by `scrape-wiki-access` from <https://wiki.guildwars2.com>. \
         Each entry below corresponds to one map id in the curated adjacency \
         table (`data/map_neighbors.yaml`). Use this dump as context when \
         deciding what to add to `data/map_neighbors_overrides.yaml` — \
         it captures the prose the structured infobox can't (story-gated \
         portals, lounge-passkey entries, festival access, secret entrances)."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "Entries: **{}**", entries.len());
    let _ = writeln!(out);
    let _ = writeln!(out, "---");
    let _ = writeln!(out);
    for (id, e) in entries {
        let _ = writeln!(out, "## {} (map {id})", e.name);
        let _ = writeln!(out);
        if let Some(region) = &e.region_name {
            let _ = writeln!(out, "**Region**: {region}");
        }
        if let (Some(min), Some(max)) = (e.min_level, e.max_level) {
            let _ = writeln!(out, "**Levels**: {min}–{max}");
        }
        let _ = writeln!(
            out,
            "**Wiki**: <https://wiki.guildwars2.com/wiki/{}>",
            url_path(&e.name)
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "### Infobox");
        let _ = writeln!(out);
        for key in &["type", "id", "requires", "within", "connections"] {
            if let Some(v) = e.infobox.get(*key) {
                let cleaned = clean_wiki_markup(v);
                let one_line = cleaned.replace('\n', " / ");
                let _ = writeln!(out, "- `{key}`: {}", one_line.trim());
            }
        }
        let _ = writeln!(out);
        if !e.intro.is_empty() {
            let _ = writeln!(out, "### Intro");
            let _ = writeln!(out);
            let _ = writeln!(out, "{}", e.intro);
            let _ = writeln!(out);
        }
        for (heading, body) in &e.sections {
            let _ = writeln!(out, "### {heading}");
            let _ = writeln!(out);
            let _ = writeln!(out, "{body}");
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "---");
        let _ = writeln!(out);
    }
    out
}

fn url_path(name: &str) -> String {
    name.replace(' ', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_location_infobox_matches_simple_template() {
        let txt = r#"
== History ==
{{Location infobox
| name = Foo
| type = Zone
| id = 42
}}
Body.
"#;
        let body = extract_location_infobox(txt).expect("found");
        assert!(body.contains("Zone"));
        assert!(!body.contains("Body"));
    }

    #[test]
    fn parse_infobox_params_picks_typed_id() {
        let body = r"
| name = Foo
| type = Zone
| id = 42
| connections = [[Bar]] (NW)
";
        let p = parse_infobox_params(body);
        assert_eq!(p.get("type").map(|s| s.as_str()), Some("Zone"));
        assert_eq!(p.get("id").map(|s| s.as_str()), Some("42"));
    }

    #[test]
    fn extract_intro_takes_text_after_infobox_and_before_first_heading() {
        let txt = r"
{{Location infobox
| type = Zone
| id = 42
}}
This is the intro paragraph about Foo.

Second paragraph still intro.

== Locations ==
Body.
";
        let intro = extract_intro(txt);
        assert!(intro.contains("This is the intro"), "got: {intro}");
        assert!(intro.contains("Second paragraph"), "got: {intro}");
        assert!(!intro.contains("Locations"), "should stop at heading");
    }

    #[test]
    fn extract_access_sections_picks_getting_there_drops_trivia_and_locations() {
        let txt = r#"
{{Location infobox
| type = Zone
| id = 42
}}
Intro.

== Locations ==
Inside the zone — POI tables here aren't useful for routing.

== Getting there ==
Take the asura gate from Lion's Arch.

== Asura gate ==
Located near Trader's Forum.

== Trivia ==
Unrelated.
"#;
        let sections = extract_access_sections(txt);
        let headings: Vec<&str> = sections.iter().map(|(h, _)| h.as_str()).collect();
        assert!(headings.contains(&"Getting there"), "got: {headings:?}");
        assert!(headings.contains(&"Asura gate"), "got: {headings:?}");
        assert!(
            !headings.contains(&"Trivia"),
            "unrelated section must be filtered: {headings:?}"
        );
        assert!(
            !headings.contains(&"Locations"),
            "noisy POI table must be filtered: {headings:?}"
        );
    }

    #[test]
    fn clean_wiki_markup_pipes_through_to_display_text() {
        let raw = "Go to [[Lion's Arch|the city]] via [[Asura gate]].";
        let out = clean_wiki_markup(raw);
        assert!(out.contains("the city"), "got: {out}");
        assert!(out.contains("Asura gate"), "got: {out}");
        assert!(!out.contains("[["));
        assert!(!out.contains("]]"));
    }

    #[test]
    fn clean_wiki_markup_strips_html_comments_and_files() {
        let raw = "[[File:foo.png|thumb]] Hello <!-- internal note -->world.";
        let out = clean_wiki_markup(raw);
        assert!(out.contains("Hello"));
        assert!(out.contains("world"));
        assert!(!out.contains("File:"));
        assert!(!out.contains("internal note"));
    }

    #[test]
    fn clean_wiki_markup_handles_br_and_emphasis() {
        let raw = "First.<br>Second.<br />Third '''bold''' and ''italic''.";
        let out = clean_wiki_markup(raw);
        assert!(out.contains("First."));
        assert!(out.contains("Second."));
        assert!(out.contains("**bold**"));
        assert!(out.contains("*italic*"));
        assert!(!out.contains("<br"));
    }
}
