//! One-shot scraper that walks the GW2 wiki + GW2 API to build the
//! curated map-adjacency YAML the gw2-mcp server ships in
//! `data/map_neighbors.yaml`.
//!
//! Run from the repo root:
//!
//! ```text
//! cargo run -p scrape-map-neighbors -- --out data/map_neighbors.yaml
//! ```
//!
//! What it does (per the planning spec):
//!
//! 1. Build a name→id index of every public map by hitting
//!    `https://api.guildwars2.com/v2/maps?ids=all`.
//! 2. Enumerate every `Category:Zones` page on the wiki via the
//!    MediaWiki action API with pagination (`cmcontinue`).
//! 3. For each page, fetch wikitext (`action=parse&prop=wikitext`).
//! 4. Parse the `{{Location infobox … | type = Zone | id = <map id> |
//!    connections = … }}` infobox. Drop anything where
//!    `type != "Zone"` or `id` isn't a known `/v2/maps` id.
//! 5. Per connection segment, regex-extract the target page name +
//!    optional direction (e.g. `[[Brisban Wildlands]] (NW)`). Resolve
//!    the target to a map id via the name→id index from step 1.
//! 6. Emit YAML keyed by map id. Unresolved target page names get
//!    reported to stderr for hand-review (cities, instance
//!    entrances, disambiguation hops).
//!
//! The output is deterministic per `/v2/maps` + wiki snapshot — same
//! input, same YAML. Hand-review is expected before the YAML lands
//! in `data/`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use serde::{Deserialize, Serialize};

const GW2_API_BASE: &str = "https://api.guildwars2.com/v2";
const WIKI_API_BASE: &str = "https://wiki.guildwars2.com/api.php";
const USER_AGENT: &str = concat!(
    "gw2-mcp-scraper/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);

#[derive(Parser, Debug)]
#[command(
    name = "scrape-map-neighbors",
    about = "Scrape the GW2 wiki for map adjacency data and emit YAML for the gw2-mcp server."
)]
struct Args {
    /// Output YAML path.
    #[arg(long, default_value = "data/map_neighbors.yaml")]
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
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
struct YamlOutput {
    /// Keyed by source map id. BTreeMap so output is deterministic.
    maps: BTreeMap<u32, YamlMapEntry>,
}

#[derive(Debug, Serialize)]
struct YamlMapEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_name: Option<String>,
    neighbors: Vec<YamlNeighbor>,
}

#[derive(Debug, Serialize)]
struct YamlNeighbor {
    map_id: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;

    eprintln!("[1/4] Fetching /v2/maps?ids=all …");
    let maps = fetch_all_maps(&client).await?;
    eprintln!("    {} maps loaded from GW2 API.", maps.len());
    let name_to_id = build_name_index(&maps);

    eprintln!("[2/4] Enumerating Category:Zones on the wiki …");
    let mut zone_pages = list_category_members(&client, "Category:Zones").await?;
    eprintln!("    {} zone pages found.", zone_pages.len());
    if let Some(n) = args.limit {
        zone_pages.truncate(n);
        eprintln!("    Limited to first {n} for this run.");
    }

    eprintln!("[3/4] Fetching + parsing each page's wikitext …");
    let mut output = YamlOutput {
        maps: BTreeMap::new(),
    };
    let mut unresolved: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut skipped_not_zone = 0usize;
    let mut skipped_unknown_id = 0usize;

    let total = zone_pages.len();
    for (i, page) in zone_pages.iter().enumerate() {
        if i % 20 == 0 {
            eprintln!("    [{}/{}] {}", i + 1, total, page);
        }
        match parse_zone_page(&client, page, &maps).await {
            Ok(Some(entry)) => {
                let source_map_id = entry.source_id;
                let mut neighbors = Vec::new();
                for raw in entry.raw_connections {
                    if let Some(id) = name_to_id.get(&normalise(&raw.page)) {
                        let neighbor_name = maps
                            .get(id)
                            .map(|m| m.name.clone())
                            .unwrap_or_else(|| raw.page.clone());
                        neighbors.push(YamlNeighbor {
                            map_id: *id,
                            name: neighbor_name,
                            direction: raw.direction,
                        });
                    } else {
                        unresolved
                            .entry(raw.page.clone())
                            .or_default()
                            .push(entry.source_name.clone());
                    }
                }
                // Sort neighbors by name so YAML diffs are minimal across reruns.
                neighbors.sort_by(|a, b| a.name.cmp(&b.name));
                output.maps.insert(
                    source_map_id,
                    YamlMapEntry {
                        name: entry.source_name,
                        region_name: maps.get(&source_map_id).and_then(|m| m.region_name.clone()),
                        neighbors,
                    },
                );
            }
            Ok(None) => {
                // Tracked via the counters below; reason already printed.
            }
            Err(e) => {
                eprintln!("    ! page {page} failed: {e}");
            }
        }
        // The wiki MediaWiki action API is more tolerant than the GW2
        // API; still, a brief pause keeps us friendly.
        tokio::time::sleep(Duration::from_millis(80)).await;
        _ = (&mut skipped_not_zone, &mut skipped_unknown_id);
    }

    eprintln!("[4/4] Writing YAML to {} …", args.out.display());
    let yaml = serde_yaml_bw::to_string(&output)?;
    std::fs::write(&args.out, yaml).with_context(|| format!("writing {}", args.out.display()))?;
    eprintln!("    wrote {} entries.", output.maps.len());

    if !unresolved.is_empty() {
        eprintln!();
        eprintln!(
            "Unresolved neighbor page names ({} distinct). These wiki pages didn't map to a \
             public /v2/maps id. Review and either add them by hand to the YAML, retag them, or \
             leave them out as not-walkable transitions (cities, instance entrances, fractals, \
             story instances):",
            unresolved.len()
        );
        for (page, sources) in &unresolved {
            eprintln!("  - {page}  (cited from: {})", sources.join(", "));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ZoneEntry {
    source_id: u32,
    source_name: String,
    raw_connections: Vec<RawConnection>,
}

#[derive(Debug)]
struct RawConnection {
    page: String,
    direction: Option<String>,
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
        idx.insert(normalise(&m.name), *id);
    }
    idx
}

fn normalise(name: &str) -> String {
    // Wiki page names use spaces; both sides preserve case but our
    // index is case-insensitive to forgive infobox typos.
    name.trim().to_lowercase()
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
        .ok_or_else(|| anyhow::anyhow!("no wikitext in response for {page}"))?
        .to_owned();
    Ok(text)
}

async fn parse_zone_page(
    client: &reqwest::Client,
    page: &str,
    maps: &BTreeMap<u32, GwMap>,
) -> Result<Option<ZoneEntry>> {
    let wikitext = fetch_wikitext(client, page).await?;
    let Some(infobox) = extract_location_infobox(&wikitext) else {
        eprintln!("    skipped {page}: no Location infobox");
        return Ok(None);
    };
    let params = parse_infobox_params(&infobox);

    let type_value = params.get("type").map(|s| s.to_lowercase());
    if type_value.as_deref() != Some("zone") {
        // Region pages, story instance pages, etc. land here.
        return Ok(None);
    }

    let source_id: u32 = match params.get("id").and_then(|s| s.trim().parse().ok()) {
        Some(id) => id,
        None => {
            eprintln!("    skipped {page}: no integer | id =");
            return Ok(None);
        }
    };
    if !maps.contains_key(&source_id) {
        eprintln!("    skipped {page}: id {source_id} not in /v2/maps (probably WvW/PvP/instance)");
        return Ok(None);
    }
    let source_name = maps
        .get(&source_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| page.to_owned());

    let raw_connections = params
        .get("connections")
        .map(|s| parse_connections(s))
        .unwrap_or_default();

    Ok(Some(ZoneEntry {
        source_id,
        source_name,
        raw_connections,
    }))
}

/// Pull the body of `{{Location infobox …}}` out of the wikitext.
/// Returns the inside of the braces (without the leading `Location
/// infobox` template name).
fn extract_location_infobox(wikitext: &str) -> Option<String> {
    // Case-insensitive locate.
    let lower = wikitext.to_lowercase();
    let start_marker = "{{location infobox";
    let mut start = lower.find(start_marker)?;
    // Advance past the marker.
    start += start_marker.len();
    // Now find the matching `}}` accounting for nested templates.
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

/// Parse `| key = value` parameters from the inside of a template.
/// Values can span newlines but the next `| ` at column 0 (after a
/// newline) marks the next parameter.
fn parse_infobox_params(infobox: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut buf = String::new();
    let mut depth = 0i32; // track nested {{…}} so a `|` inside one doesn't split us
    let mut link_depth = 0i32; // track nested [[…]]

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
            // boundary
            push(&mut out, &current_key, &buf);
            buf.clear();
            current_key = None;
            // Read the key up to '='.
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

/// Parse a `| connections =` value into structured `(page, direction)`
/// segments. Per the verified contract:
///
/// - segments are separated by `<br>` (tolerant of whitespace + case)
/// - each segment matches `[[Target Page]] (DIRECTION?)`
fn parse_connections(value: &str) -> Vec<RawConnection> {
    // Lowercase comparison strips off variations like `<BR>`, `<br/>`,
    // `<br />`. Split on the regex.
    let br = Regex::new(r"(?i)<br\s*/?>").expect("static regex compiles");
    let seg_re = Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]\s*(?:\(([^)]+)\))?")
        .expect("static regex compiles");
    let mut out = Vec::new();
    for raw in br.split(value) {
        // Strip wiki comments, leading bullets, etc.
        let trimmed = strip_wiki_chrome(raw);
        if let Some(caps) = seg_re.captures(&trimmed) {
            let page = caps
                .get(1)
                .map(|m| m.as_str().trim().to_owned())
                .unwrap_or_default();
            let direction = caps
                .get(2)
                .map(|m| m.as_str().trim().to_owned())
                .filter(|s| !s.is_empty());
            if !page.is_empty() {
                out.push(RawConnection { page, direction });
            }
        }
    }
    out
}

fn strip_wiki_chrome(s: &str) -> String {
    let re = Regex::new(r"<!--.*?-->").expect("static regex compiles");
    re.replace_all(s, "").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connections_extracts_page_and_direction() {
        let raw = "[[Brisban Wildlands]] (NW)<br>[[Metrica Province]] (W)<br>[[Lion's Arch]]";
        let out = parse_connections(raw);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].page, "Brisban Wildlands");
        assert_eq!(out[0].direction.as_deref(), Some("NW"));
        assert_eq!(out[2].page, "Lion's Arch");
        assert_eq!(out[2].direction, None);
    }

    #[test]
    fn parse_connections_handles_multi_direction_and_self_close_br() {
        let raw = "[[Gendarran Fields]] (E)<br />[[Kessex Hills]] (SW, S)";
        let out = parse_connections(raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].direction.as_deref(), Some("SW, S"));
    }

    #[test]
    fn parse_connections_strips_piped_display_text() {
        // [[Page|displayed text]] should resolve to "Page".
        let raw = "[[The Grove|Grove]] (S)";
        let out = parse_connections(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].page, "The Grove");
    }

    #[test]
    fn extract_location_infobox_handles_nested_templates() {
        let wikitext = r#"
== History ==
Whatever
{{Location infobox
| name = Caledon Forest
| type = Zone
| id = 34
| connections = [[Brisban Wildlands]] (NW)<br>[[Metrica Province]] (W)
| within = {{Region|Maguuma Jungle}}
}}
Body text below.
"#;
        let body = extract_location_infobox(wikitext).expect("found");
        assert!(body.contains("connections"));
        assert!(body.contains("Brisban Wildlands"));
        assert!(body.contains("Body text") == false);
    }

    #[test]
    fn parse_infobox_params_extracts_typed_and_id() {
        let body = r#"
| name = Caledon Forest
| type = Zone
| id = 34
| connections = [[Brisban Wildlands]] (NW)<br>[[Metrica Province]] (W)
| within = {{Region|Maguuma Jungle}}
"#;
        let p = parse_infobox_params(body);
        assert_eq!(p.get("type").map(|s| s.as_str()), Some("Zone"));
        assert_eq!(p.get("id").map(|s| s.as_str()), Some("34"));
        assert!(
            p.get("connections")
                .map(|s| s.contains("Brisban Wildlands"))
                .unwrap_or(false)
        );
    }
}
