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
//!    `https://api.guildwars2.com/v2/maps?ids=all`. The same response
//!    carries `min_level` / `max_level` per map, which we mirror into
//!    the YAML so the LLM doesn't have to follow up with
//!    `list_maps_in_region` to answer "is this level-appropriate?".
//! 2. Enumerate every `Category:Zones` page on the wiki via the
//!    MediaWiki action API with pagination (`cmcontinue`).
//! 3. For each page, fetch wikitext (`action=parse&prop=wikitext`),
//!    parse the `{{Location infobox … | type = Zone | id = <map id> |
//!    connections = … }}` block, and pull out the `| requires = …`
//!    field so we can tag the source map with its expansion. The
//!    wiki uses tag values like `hot`, `pof`, `lws3`, `eod`, `soto`,
//!    `jw`, `voe`; core-Tyria maps omit the field entirely. When a
//!    map carries multiple tags (e.g. `requires = hot, lws3` for
//!    LWS3 zones that need HoT access), the *latest* expansion wins.
//!    Drop anything where `type != "Zone"`, `id` isn't a known
//!    `/v2/maps` id, or `within` contains "World vs. World" (WvW
//!    is excluded from PvE travel planning).
//! 4. Per connection segment, regex-extract the target page name +
//!    optional direction (e.g. `[[Brisban Wildlands]] (NW)`). Resolve
//!    the target to a map id via the name→id index from step 1.
//! 5. Apply the `connection` heuristic: if the segment carried an
//!    explicit direction, mark it `physical`; otherwise mark it
//!    `asura_gate`. This is the "data convention" the round-1 YAML
//!    already used implicitly (empty direction = gate) — promoting it
//!    to an explicit field makes the distinction unmissable to the LLM.
//!    Hand-curators can override specific edges to `story_gate`,
//!    `instance_portal`, or `guild_hall` after the scrape.
//! 6. Emit YAML keyed by map id, sorted by name for stable diffs.
//!    Unresolved target page names get reported to stderr for
//!    hand-review (cities, instance entrances, disambiguation hops).
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
    #[serde(default)]
    min_level: Option<u32>,
    #[serde(default)]
    max_level: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    min_level: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_level: Option<u32>,
    /// Snake-case enum value matching `domain::Expansion`. Plain
    /// string here because this crate doesn't depend on gw2-mcp.
    #[serde(skip_serializing_if = "Option::is_none")]
    expansion: Option<String>,
    neighbors: Vec<YamlNeighbor>,
}

#[derive(Debug, Serialize)]
struct YamlNeighbor {
    map_id: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<String>,
    /// Snake-case enum value matching `domain::ConnectionType`.
    /// "physical" if the source page gave a direction; "asura_gate"
    /// otherwise. Hand-curators can override to "story_gate",
    /// "instance_portal", or "guild_hall".
    #[serde(skip_serializing_if = "Option::is_none")]
    connection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_level: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_level: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expansion: Option<String>,
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

    eprintln!("[2/4] Enumerating Category:Zones + Category:Cities on the wiki …");
    let mut zone_pages = list_category_members(&client, "Category:Zones").await?;
    let city_pages = list_category_members(&client, "Category:Cities").await?;
    let zone_count = zone_pages.len();
    // Cities are queryable as sources of asura-gate edges, so we
    // include their pages alongside zones. The infobox parser accepts
    // both `type = Zone` and `type = City`.
    for c in &city_pages {
        if !zone_pages.contains(c) {
            zone_pages.push(c.clone());
        }
    }
    eprintln!(
        "    {} zone pages + {} city pages ({} total after dedupe).",
        zone_count,
        city_pages.len(),
        zone_pages.len()
    );
    if let Some(n) = args.limit {
        zone_pages.truncate(n);
        eprintln!("    Limited to first {n} for this run.");
    }

    eprintln!("[3/4] Fetching + parsing each page's wikitext …");
    let mut output = YamlOutput {
        maps: BTreeMap::new(),
    };
    // Pass-1 scrape result: source_id → (release, raw_connections).
    // We accumulate before emitting so neighbor-side level/expansion can
    // pull from the same in-memory map.
    let mut scraped: BTreeMap<u32, ScrapedZone> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let total = zone_pages.len();
    for (i, page) in zone_pages.iter().enumerate() {
        if i % 20 == 0 {
            eprintln!("    [{}/{}] {}", i + 1, total, page);
        }
        match parse_zone_page(&client, page, &maps).await {
            Ok(Some(entry)) => {
                scraped.insert(
                    entry.source_id,
                    ScrapedZone {
                        source_name: entry.source_name,
                        requires_raw: entry.requires_raw,
                        raw_connections: entry.raw_connections,
                    },
                );
            }
            Ok(None) => {
                // Tracked in stderr already.
            }
            Err(e) => {
                eprintln!("    ! page {page} failed: {e}");
            }
        }
        // Keep the request rate polite — the wiki MediaWiki action API
        // is tolerant but courtesy costs us little.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    eprintln!("[3.5/4] Resolving neighbors + applying enrichments …");
    for (source_id, zone) in &scraped {
        let mut neighbors = Vec::new();
        for raw in &zone.raw_connections {
            if let Some(id) = name_to_id.get(&normalise(&raw.page)) {
                let target_map = maps.get(id);
                let neighbor_name = target_map
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| raw.page.clone());
                let target_expansion = scraped
                    .get(id)
                    .and_then(|z| expansion_with_core_default(z.requires_raw.as_deref()));
                neighbors.push(YamlNeighbor {
                    map_id: *id,
                    name: neighbor_name,
                    connection: Some(infer_connection_type(raw.direction.as_deref()).to_owned()),
                    direction: raw.direction.clone(),
                    min_level: target_map.and_then(|m| m.min_level),
                    max_level: target_map.and_then(|m| m.max_level),
                    expansion: target_expansion,
                });
            } else {
                unresolved
                    .entry(raw.page.clone())
                    .or_default()
                    .push(zone.source_name.clone());
            }
        }
        // Sort neighbors by name so YAML diffs are minimal across reruns.
        neighbors.sort_by(|a, b| a.name.cmp(&b.name));
        let source_map = maps.get(source_id);
        output.maps.insert(
            *source_id,
            YamlMapEntry {
                name: zone.source_name.clone(),
                region_name: source_map.and_then(|m| m.region_name.clone()),
                min_level: source_map.and_then(|m| m.min_level),
                max_level: source_map.and_then(|m| m.max_level),
                expansion: expansion_with_core_default(zone.requires_raw.as_deref()),
                neighbors,
            },
        );
    }

    eprintln!("[3.75/4] Normalizing symmetry — adding reverse edges …");
    let added = symmetrize(&mut output);
    eprintln!(
        "    added {} reverse edges (skipped one-way + unmodelled endpoints).",
        added
    );

    eprintln!("[3.85/4] Reconciling direction asymmetry on bidirectional edges …");
    let reconciled = reconcile_directions(&mut output);
    eprintln!(
        "    rewrote {reconciled} non-canonical directions (lower-id side is authoritative)."
    );

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

/// One zone's scraped data before neighbor resolution. We accumulate
/// these in pass-1 so pass-2 can cross-reference target maps for level
/// range / expansion without re-fetching them.
#[derive(Debug)]
struct ScrapedZone {
    source_name: String,
    /// Raw value of `| requires =` from the infobox, if present. A
    /// comma-separated list of expansion tags (`hot`, `pof`, `lws3`,
    /// `lws4`, `lws5`, `eod`, `soto`, `jw`, `voe`). Absence means a
    /// core-Tyria map. Mapped to a single canonical expansion via
    /// [`pick_expansion`].
    requires_raw: Option<String>,
    raw_connections: Vec<RawConnection>,
}

#[derive(Debug)]
struct ZoneEntry {
    source_id: u32,
    source_name: String,
    requires_raw: Option<String>,
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

/// Pass-1 connection-type heuristic. The round-1 YAML already used
/// "empty direction = gate" implicitly; we promote that to an explicit
/// `connection` field so the LLM can rely on the signal without
/// inferring from absence.
fn infer_connection_type(direction: Option<&str>) -> &'static str {
    if direction.is_some_and(|d| !d.trim().is_empty()) {
        "physical"
    } else {
        "asura_gate"
    }
}

/// Pick the canonical expansion for a map from its `| requires =`
/// value. `requires` is a comma-separated list of access-prereq tags;
/// when a map needs more than one (e.g. LWS3 zones require both HoT
/// and the LWS3 episode itself, so `requires = hot, lws3`), we report
/// the *latest* — that's the one a player most needs to own.
///
/// Returns the snake_case enum string that `domain::Expansion`
/// deserialises. `None` means the value didn't match any known tag.
/// Callers should default to `"core"` for maps that omit the field
/// entirely (those are core-Tyria release).
fn pick_expansion(requires_raw: Option<&str>) -> Option<String> {
    // Order roughly corresponds to release chronology so "later" wins
    // when a map carries multiple tags. Earliest at index 0.
    const PRIORITY: &[(&str, &str)] = &[
        ("hot", "heart_of_thorns"),
        ("lws3", "living_world_season3"),
        ("pof", "path_of_fire"),
        ("lws4", "living_world_season4"),
        // The wiki uses `lws5` for what was officially renamed the
        // "Icebrood Saga"; map both to the same enum value.
        ("lws5", "icebrood_saga"),
        ("ibs", "icebrood_saga"),
        ("eod", "end_of_dragons"),
        ("soto", "secrets_of_the_obscure"),
        ("jw", "janthir_wilds"),
        // Visions of Eternity / Castora.
        ("voe", "castora"),
        ("castora", "castora"),
    ];
    let raw = requires_raw?.trim().to_lowercase();
    if raw.is_empty() {
        return None;
    }
    let tags: Vec<&str> = raw.split(',').map(str::trim).collect();
    // Iterate priority in reverse so the *latest* match wins.
    for (tag, enum_value) in PRIORITY.iter().rev() {
        if tags.contains(tag) {
            return Some((*enum_value).to_owned());
        }
    }
    None
}

/// Wrap [`pick_expansion`] so absence defaults to `"core"`.
/// Core-Tyria maps don't carry a `requires` field on the wiki.
fn expansion_with_core_default(requires_raw: Option<&str>) -> Option<String> {
    match requires_raw {
        Some(s) if !s.trim().is_empty() => pick_expansion(Some(s)),
        _ => Some("core".to_owned()),
    }
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
    match type_value.as_deref() {
        Some("zone") | Some("city") => {}
        _ => {
            // Region pages, story instance pages, etc. land here.
            return Ok(None);
        }
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
    // Explicit WvW exclusion. The infobox `within` field on every WvW
    // zone reads "World vs. World"; rejecting it here drops Eternal
    // Battlegrounds, the three borderlands, Edge of the Mists, and the
    // Mists Rift sub-zones from the YAML in one place. PvE travel
    // planning shouldn't surface these — they share a continent id
    // with Janthir/Castora, so the GW2 API alone doesn't disambiguate.
    if let Some(within) = params.get("within") {
        let lower = within.to_lowercase();
        if lower.contains("world vs. world") || lower.contains("world vs world") {
            eprintln!("    skipped {page}: WvW map (within = {within})");
            return Ok(None);
        }
    }
    let source_name = maps
        .get(&source_id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| page.to_owned());

    let raw_connections = params
        .get("connections")
        .map(|s| parse_connections(s))
        .unwrap_or_default();

    let requires_raw = params
        .get("requires")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    Ok(Some(ZoneEntry {
        source_id,
        source_name,
        requires_raw,
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

/// Round-2-feedback symmetry pass. For every neighbor edge `src → dst`
/// in `output`, ensure a matching `dst → src` exists. This closes the
/// asymmetry where the wiki only lists one direction (typically city
/// pages listing every zone the city has a gate to, but the zone
/// pages omitting the city back-reference).
///
/// Skips edges flagged `one_way: true` (none seeded today; reserved
/// for legitimately unidirectional portals like dungeon entrances
/// that hand-curators may add later).
///
/// Reverse-edge metadata: direction is computed via compass reversal
/// (`NE → SW`, `SSW → NNE`, etc.); connection / min_level / max_level
/// / expansion are copied from the source side. gate_location and
/// note are left blank — the original source page's hint doesn't
/// translate to the new direction without a second scrape.
///
/// Returns the count of edges added so the caller can log it.
fn symmetrize(output: &mut YamlOutput) -> usize {
    use std::collections::{BTreeMap, BTreeSet};

    // Snapshot every modelled source so the reverse can be added.
    let modelled: BTreeSet<u32> = output.maps.keys().copied().collect();

    // Existing edge set; we don't want to add a reverse if it's
    // already there with potentially different metadata (hand-curated
    // entries win).
    let mut existing: BTreeSet<(u32, u32)> = BTreeSet::new();
    for (src, entry) in &output.maps {
        for n in &entry.neighbors {
            existing.insert((*src, n.map_id));
        }
    }

    // (target_id, reverse_edge) pairs to insert. We can't mutate
    // `output.maps` while iterating it, so accumulate then apply.
    let mut to_add: BTreeMap<u32, Vec<YamlNeighbor>> = BTreeMap::new();
    for (src_id, src_entry) in &output.maps {
        for n in &src_entry.neighbors {
            if !modelled.contains(&n.map_id) {
                continue;
            }
            if existing.contains(&(n.map_id, *src_id)) {
                continue;
            }
            let reverse = YamlNeighbor {
                map_id: *src_id,
                name: src_entry.name.clone(),
                direction: n.direction.as_deref().and_then(reverse_direction),
                connection: n.connection.clone(),
                min_level: src_entry.min_level,
                max_level: src_entry.max_level,
                expansion: src_entry.expansion.clone(),
            };
            to_add.entry(n.map_id).or_default().push(reverse);
        }
    }

    let mut added = 0;
    for (target_id, reverses) in to_add {
        if let Some(entry) = output.maps.get_mut(&target_id) {
            for r in reverses {
                added += 1;
                entry.neighbors.push(r);
            }
            // Keep YAML diffs stable.
            entry.neighbors.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }
    added
}

/// Round-3-feedback: when the wiki disagrees with itself, normalize on
/// ingest. Walk every unordered pair `(a, b)` where both `a → b` and
/// `b → a` exist with non-empty directions, pick the lower-id side as
/// canonical, and overwrite the other side's direction with the
/// compass-inverse of canonical's.
///
/// "Lower id wins" is a deterministic, re-run-stable rule — we can't
/// ground-truth wiki disagreements without manual play-testing, so the
/// rule only needs to be consistent.
///
/// Skips pairs where either side's direction is `None` / empty: those
/// asymmetries are handled by [`symmetrize`], which derives the missing
/// side from the present one.
///
/// Returns the count of directions rewritten.
fn reconcile_directions(output: &mut YamlOutput) -> usize {
    // Snapshot the canonical (lower-id) side's direction for every
    // bidirectional pair. We must not mutate `output.maps` while
    // iterating it, so collect first then apply.
    let mut canonical_dirs: BTreeMap<(u32, u32), String> = BTreeMap::new();
    for (src_id, entry) in &output.maps {
        for n in &entry.neighbors {
            let Some(dir) = n.direction.as_deref() else {
                continue;
            };
            if dir.trim().is_empty() {
                continue;
            }
            let lo = (*src_id).min(n.map_id);
            let hi = (*src_id).max(n.map_id);
            // Only record the lower-id-as-source view.
            if *src_id == lo {
                canonical_dirs.insert((lo, hi), dir.to_owned());
            }
        }
    }

    let mut rewritten = 0;
    for ((lo, hi), canonical) in &canonical_dirs {
        let Some(want) = reverse_direction(canonical) else {
            // Canonical side has a direction that doesn't compass-
            // reverse cleanly (rare; e.g. "C"). Leave as-is.
            continue;
        };
        // Is the higher-id side also a bidirectional edge with a
        // non-empty direction? If so, check whether it matches.
        let Some(hi_entry) = output.maps.get_mut(hi) else {
            continue;
        };
        for n in &mut hi_entry.neighbors {
            if n.map_id != *lo {
                continue;
            }
            let Some(current) = n.direction.as_deref() else {
                break;
            };
            if current.trim().is_empty() {
                break;
            }
            if current != want {
                n.direction = Some(want.clone());
                rewritten += 1;
            }
            break;
        }
    }
    rewritten
}

/// Reverse a compass-direction string. Handles 16-point bearings
/// (N/NE/ENE/etc.) and comma-separated multi-bearings ("NW, N" →
/// "SE, S"). Returns `None` if any token doesn't reverse cleanly.
fn reverse_direction(d: &str) -> Option<String> {
    let parts: Vec<&str> = d
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        out.push(reverse_bearing(p)?);
    }
    Some(out.join(", "))
}

/// Reverse a single 16-point compass bearing.
fn reverse_bearing(b: &str) -> Option<String> {
    match b.to_uppercase().as_str() {
        "N" => Some("S".to_owned()),
        "NNE" => Some("SSW".to_owned()),
        "NE" => Some("SW".to_owned()),
        "ENE" => Some("WSW".to_owned()),
        "E" => Some("W".to_owned()),
        "ESE" => Some("WNW".to_owned()),
        "SE" => Some("NW".to_owned()),
        "SSE" => Some("NNW".to_owned()),
        "S" => Some("N".to_owned()),
        "SSW" => Some("NNE".to_owned()),
        "SW" => Some("NE".to_owned()),
        "WSW" => Some("ENE".to_owned()),
        "W" => Some("E".to_owned()),
        "WNW" => Some("ESE".to_owned()),
        "NW" => Some("SE".to_owned()),
        "NNW" => Some("SSE".to_owned()),
        // "C" (contained) is sometimes used by guild halls etc; the
        // semantic doesn't have a directional reverse, so just echo.
        "C" => Some("C".to_owned()),
        _ => None,
    }
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
        assert!(!body.contains("Body text"));
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

    #[test]
    fn infer_connection_type_uses_direction_signal() {
        assert_eq!(infer_connection_type(Some("NW")), "physical");
        assert_eq!(infer_connection_type(Some("SW, S")), "physical");
        assert_eq!(infer_connection_type(None), "asura_gate");
        assert_eq!(infer_connection_type(Some("   ")), "asura_gate");
    }

    #[test]
    fn pick_expansion_handles_single_tag() {
        assert_eq!(
            pick_expansion(Some("hot")).as_deref(),
            Some("heart_of_thorns")
        );
        assert_eq!(pick_expansion(Some("pof")).as_deref(), Some("path_of_fire"));
        assert_eq!(
            pick_expansion(Some("eod")).as_deref(),
            Some("end_of_dragons")
        );
        assert_eq!(
            pick_expansion(Some("soto")).as_deref(),
            Some("secrets_of_the_obscure")
        );
        assert_eq!(pick_expansion(Some("jw")).as_deref(), Some("janthir_wilds"));
        assert_eq!(pick_expansion(Some("voe")).as_deref(), Some("castora"));
        assert_eq!(
            pick_expansion(Some("lws5")).as_deref(),
            Some("icebrood_saga")
        );
        assert_eq!(
            pick_expansion(Some("ibs")).as_deref(),
            Some("icebrood_saga")
        );
    }

    #[test]
    fn pick_expansion_returns_latest_tag_when_multiple() {
        // Bitterfrost Frontier: `requires = hot, lws3` — LWS3 is later
        // than HoT, so the player needs LWS3 access (which already
        // implies HoT).
        assert_eq!(
            pick_expansion(Some("hot, lws3")).as_deref(),
            Some("living_world_season3")
        );
        // Domain of Istan: `requires = pof, lws4`
        assert_eq!(
            pick_expansion(Some("pof, lws4")).as_deref(),
            Some("living_world_season4")
        );
        // Hypothetical late expansion combo
        assert_eq!(
            pick_expansion(Some("soto, jw")).as_deref(),
            Some("janthir_wilds")
        );
    }

    #[test]
    fn pick_expansion_ignores_unknown_tags() {
        assert_eq!(pick_expansion(Some("nonsense")).as_deref(), None);
        assert_eq!(pick_expansion(Some("")).as_deref(), None);
        assert_eq!(pick_expansion(None).as_deref(), None);
    }

    #[test]
    fn reverse_bearing_round_trips_8_point_compass() {
        for (b, expected) in [
            ("N", "S"),
            ("NE", "SW"),
            ("E", "W"),
            ("SE", "NW"),
            ("S", "N"),
            ("SW", "NE"),
            ("W", "E"),
            ("NW", "SE"),
        ] {
            assert_eq!(reverse_bearing(b).as_deref(), Some(expected));
            // Lowercase tolerance.
            assert_eq!(
                reverse_bearing(&b.to_lowercase()).as_deref(),
                Some(expected)
            );
            // Round-trip via reverse_direction.
            assert_eq!(reverse_direction(b).as_deref(), Some(expected));
        }
    }

    #[test]
    fn reverse_bearing_handles_16_point_compass() {
        for (b, expected) in [
            ("NNE", "SSW"),
            ("ENE", "WSW"),
            ("ESE", "WNW"),
            ("SSE", "NNW"),
            ("SSW", "NNE"),
            ("WSW", "ENE"),
            ("WNW", "ESE"),
            ("NNW", "SSE"),
        ] {
            assert_eq!(reverse_bearing(b).as_deref(), Some(expected));
        }
    }

    #[test]
    fn reverse_direction_handles_comma_separated() {
        // From the round-1 YAML: Queensdale points to Kessex as "SW, S".
        // The reverse from Kessex should be "NE, N".
        assert_eq!(reverse_direction("SW, S").as_deref(), Some("NE, N"));
        assert_eq!(reverse_direction("NW, N").as_deref(), Some("SE, S"));
    }

    #[test]
    fn reverse_direction_returns_none_on_garbage() {
        assert_eq!(reverse_direction("not a bearing").as_deref(), None);
        assert_eq!(reverse_direction("").as_deref(), None);
    }

    #[test]
    fn symmetrize_adds_missing_reverse_with_flipped_direction() {
        let mut out = YamlOutput {
            maps: BTreeMap::new(),
        };
        out.maps.insert(
            1,
            YamlMapEntry {
                name: "Alpha".into(),
                region_name: None,
                min_level: Some(1),
                max_level: Some(15),
                expansion: Some("core".into()),
                neighbors: vec![YamlNeighbor {
                    map_id: 2,
                    name: "Beta".into(),
                    direction: Some("NE".into()),
                    connection: Some("physical".into()),
                    min_level: Some(20),
                    max_level: Some(30),
                    expansion: Some("core".into()),
                }],
            },
        );
        // Beta exists but doesn't list Alpha yet — symmetrize should
        // insert the reverse.
        out.maps.insert(
            2,
            YamlMapEntry {
                name: "Beta".into(),
                region_name: None,
                min_level: Some(20),
                max_level: Some(30),
                expansion: Some("core".into()),
                neighbors: vec![],
            },
        );

        let added = symmetrize(&mut out);
        assert_eq!(added, 1);
        let beta = out.maps.get(&2).unwrap();
        assert_eq!(beta.neighbors.len(), 1);
        let reverse = &beta.neighbors[0];
        assert_eq!(reverse.map_id, 1);
        assert_eq!(reverse.name, "Alpha");
        assert_eq!(reverse.direction.as_deref(), Some("SW"));
        assert_eq!(reverse.connection.as_deref(), Some("physical"));
        assert_eq!(reverse.min_level, Some(1));
    }

    #[test]
    fn symmetrize_skips_unmodelled_endpoint() {
        // Alpha points to map id 999 which isn't in the table; the
        // reverse can't be inserted (we have nothing to insert it
        // into).
        let mut out = YamlOutput {
            maps: BTreeMap::new(),
        };
        out.maps.insert(
            1,
            YamlMapEntry {
                name: "Alpha".into(),
                region_name: None,
                min_level: None,
                max_level: None,
                expansion: None,
                neighbors: vec![YamlNeighbor {
                    map_id: 999,
                    name: "Unmodelled".into(),
                    direction: Some("N".into()),
                    connection: Some("physical".into()),
                    min_level: None,
                    max_level: None,
                    expansion: None,
                }],
            },
        );
        let added = symmetrize(&mut out);
        assert_eq!(added, 0);
    }

    #[test]
    fn symmetrize_preserves_existing_reverse() {
        // Both directions already exist; no double-add.
        let mut out = YamlOutput {
            maps: BTreeMap::new(),
        };
        let make =
            |_id: u32, name: &str, neighbor_id: u32, neighbor_name: &str, dir: &str| YamlMapEntry {
                name: name.into(),
                region_name: None,
                min_level: None,
                max_level: None,
                expansion: None,
                neighbors: vec![YamlNeighbor {
                    map_id: neighbor_id,
                    name: neighbor_name.into(),
                    direction: Some(dir.into()),
                    connection: Some("physical".into()),
                    min_level: None,
                    max_level: None,
                    expansion: None,
                }],
            };
        out.maps.insert(1, make(1, "Alpha", 2, "Beta", "N"));
        out.maps.insert(2, make(2, "Beta", 1, "Alpha", "S"));
        let added = symmetrize(&mut out);
        assert_eq!(added, 0);
    }

    /// Helper for `reconcile_directions` tests: build a pair where
    /// map `lo` lists map `hi` with `lo_dir`, and `hi` lists `lo`
    /// with `hi_dir`. Both sides modelled, both with directions.
    fn build_pair(lo: u32, hi: u32, lo_dir: Option<&str>, hi_dir: Option<&str>) -> YamlOutput {
        let mk_neighbor = |map_id: u32, name: &str, direction: Option<&str>| YamlNeighbor {
            map_id,
            name: name.into(),
            direction: direction.map(str::to_owned),
            connection: Some("physical".into()),
            min_level: None,
            max_level: None,
            expansion: None,
        };
        let mut out = YamlOutput {
            maps: BTreeMap::new(),
        };
        out.maps.insert(
            lo,
            YamlMapEntry {
                name: format!("Map{lo}"),
                region_name: None,
                min_level: None,
                max_level: None,
                expansion: None,
                neighbors: vec![mk_neighbor(hi, &format!("Map{hi}"), lo_dir)],
            },
        );
        out.maps.insert(
            hi,
            YamlMapEntry {
                name: format!("Map{hi}"),
                region_name: None,
                min_level: None,
                max_level: None,
                expansion: None,
                neighbors: vec![mk_neighbor(lo, &format!("Map{lo}"), hi_dir)],
            },
        );
        out
    }

    #[test]
    fn reconcile_directions_noop_on_matched_pair() {
        // Map 1 → Map 2 = "E"; Map 2 → Map 1 = "W" (the correct inverse).
        let mut out = build_pair(1, 2, Some("E"), Some("W"));
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 0);
        assert_eq!(out.maps[&2].neighbors[0].direction.as_deref(), Some("W"));
    }

    #[test]
    fn reconcile_directions_rewrites_higher_id_side_on_mismatch() {
        // Queensdale-style: Map 1 says Map 2 is "E", Map 2 says Map 1
        // is "NW". Canonical (Map 1) wins, so Map 2's edge becomes "W".
        let mut out = build_pair(1, 2, Some("E"), Some("NW"));
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 1);
        assert_eq!(out.maps[&1].neighbors[0].direction.as_deref(), Some("E"));
        assert_eq!(out.maps[&2].neighbors[0].direction.as_deref(), Some("W"));
    }

    #[test]
    fn reconcile_directions_skips_one_sided_missing_direction() {
        // Map 2's edge has no direction yet — symmetrize handles that
        // case; reconcile_directions must not touch it.
        let mut out = build_pair(1, 2, Some("E"), None);
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 0);
        assert_eq!(out.maps[&2].neighbors[0].direction, None);
    }

    #[test]
    fn reconcile_directions_rewrites_multi_segment_direction() {
        // Queensdale-Kessex style: Map 1 says Map 2 is "SW, S"; Map 2
        // says Map 1 is "NW, N". The compass-inverse of "SW, S" is
        // "NE, N", so Map 2's edge gets rewritten.
        let mut out = build_pair(1, 2, Some("SW, S"), Some("NW, N"));
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 1);
        assert_eq!(
            out.maps[&2].neighbors[0].direction.as_deref(),
            Some("NE, N"),
        );
    }

    #[test]
    fn reconcile_directions_leaves_uninvertable_canonical_alone() {
        // "C" (contained / guild hall) echoes itself in reverse_bearing,
        // so the inverse of "C" is "C" — matched, no rewrite.
        let mut out = build_pair(1, 2, Some("C"), Some("C"));
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 0);
    }

    #[test]
    fn reconcile_directions_handles_purely_one_way_data() {
        // Only Map 1 → Map 2 exists in the table (no reverse). Nothing
        // to reconcile against. (symmetrize is the one that fills the
        // missing side, but it runs before us anyway in main().)
        let mk_neighbor = |map_id: u32, name: &str, direction: Option<&str>| YamlNeighbor {
            map_id,
            name: name.into(),
            direction: direction.map(str::to_owned),
            connection: Some("physical".into()),
            min_level: None,
            max_level: None,
            expansion: None,
        };
        let mut out = YamlOutput {
            maps: BTreeMap::new(),
        };
        out.maps.insert(
            1,
            YamlMapEntry {
                name: "Alpha".into(),
                region_name: None,
                min_level: None,
                max_level: None,
                expansion: None,
                neighbors: vec![mk_neighbor(2, "Beta", Some("E"))],
            },
        );
        let n = reconcile_directions(&mut out);
        assert_eq!(n, 0);
    }

    #[test]
    fn expansion_with_core_default_falls_back() {
        // Core maps don't carry `requires` at all on the wiki.
        assert_eq!(expansion_with_core_default(None).as_deref(), Some("core"));
        assert_eq!(
            expansion_with_core_default(Some("")).as_deref(),
            Some("core")
        );
        // Present-but-unknown values keep us honest about uncertainty.
        assert_eq!(
            expansion_with_core_default(Some("nonsense_tag")).as_deref(),
            None
        );
        // Known values work end-to-end.
        assert_eq!(
            expansion_with_core_default(Some("hot")).as_deref(),
            Some("heart_of_thorns")
        );
    }
}
