//! `BuildCatalog` adapter for Snow Crows (raid/strike meta).
//!
//! Source: <https://snowcrows.com>
//!
//! Snow Crows publishes builds as HTML pages (no public API). We:
//! - **List**: scrape `/builds/<category>` index pages and extract each
//!   build's URL + title. Results are cached in-memory per-category with
//!   a TTL (default 6h). The default category (when no `gamemode` filter
//!   is supplied) is `raids` — Snow Crows' canonical content. Other
//!   categories (`strikes`, `fractals`, `open-world`, `pvp`) are fetched
//!   only when explicitly requested via the filter. Limiting cold-start
//!   to a single request avoids tripping Cloudflare's burst rate-limit.
//! - **Fetch**: scrape a single `/builds/<category>/<profession>/<slug>`
//!   on demand.
//! - **Graceful degradation**: non-success status codes (Cloudflare 403,
//!   5xx, etc.) log a warning and return empty, rather than failing the
//!   whole MCP tool call. 404 caches as empty for TTL.
//!
//! ## On the Cloudflare content signals
//!
//! Snow Crows' `robots.txt` declares `search=yes, ai-train=no`. Per their
//! own definitions:
//!
//! - `ai-train` — "training or fine-tuning AI models." Set to **no**.
//! - `ai-input` — "inputting content into one or more AI models (e.g.,
//!   retrieval augmented generation, grounding, or other real-time taking
//!   of content for generative AI search answers)." **Not set.**
//! - `search` — "building a search index and providing search results
//!   (e.g., returning hyperlinks and short excerpts from your website's
//!   contents). Search does not include providing AI-generated search
//!   summaries." Set to **yes**.
//!
//! This adapter is `ai-input` (read pages on demand to ground a single
//! user's question), not `ai-train`. The unset `ai-input` signal "neither
//! grants nor restricts" per Snow Crows' own preamble. Their User-Agent
//! blocklist targets training crawlers (`ClaudeBot`, `GPTBot`, `CCBot`,
//! `Google-Extended`, etc.); our User-Agent is honest (`gw2-mcp/X.Y.Z
//! (+repo URL)`) and not in that list.
//!
//! Every response includes a `source_url` for attribution. The TTL cache
//! keeps our request volume low.
//!
//! ## Slug shape
//!
//! `slug` is the URL path tail: `<category>/<profession>/<build-slug>`,
//! e.g. `raids/elementalist/power-tempest-spear`. The leading `/builds/`
//! is implicit.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use scraper::{Html, Selector};
use tokio::sync::Mutex;

use crate::domain::BuildSlug;
use crate::ports::{BuildCatalog, BuildDetail, BuildSummary, CatalogError, CatalogFilter};

const SOURCE_NAME: &str = "snowcrows";
const DEFAULT_BASE: &str = "https://snowcrows.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp; contact: github issues)"
);
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 6);

/// Known top-level category pages on snowcrows.com. Verified live:
/// `strikes` and `fractals` *used to* exist but now 302 to an invalid
/// path that 403s, so they're excluded. `accessibuilds` is intentionally
/// dropped — it's an accessibility-specific tier, not meta. Unknown
/// gamemode filters return empty; a 404 caches as empty for the full
/// TTL.
const CATEGORIES: &[&str] = &["raids", "open-world", "pvp", "wvw"];

/// Category returned when no `gamemode` filter is supplied. Snow Crows is
/// primarily a raids site; fetching all five categories on every cold
/// start risks tripping Cloudflare's per-IP burst rate-limit (observed
/// 403s in the wild). Callers who want strikes/fractals/pvp/open-world
/// should pass an explicit `gamemode` filter — those still work via
/// [`SnowCrowsCatalog::list_category`].
const DEFAULT_CATEGORY: &str = "raids";

struct CachedListing {
    fetched_at: Instant,
    builds: Vec<BuildSummary>,
}

pub struct SnowCrowsCatalog {
    client: Client,
    base_url: String,
    cache: Mutex<HashMap<String, CachedListing>>,
    cache_ttl: Duration,
}

impl SnowCrowsCatalog {
    pub fn new() -> Result<Self, CatalogError> {
        Self::with_base_url(DEFAULT_BASE.to_owned())
    }

    pub fn with_base_url(base_url: String) -> Result<Self, CatalogError> {
        Self::with_options(base_url, DEFAULT_CACHE_TTL)
    }

    /// Construct with an explicit cache TTL. Used by tests to verify both
    /// cache-hit (long TTL) and cache-miss (zero TTL) behavior.
    pub fn with_options(base_url: String, cache_ttl: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: e.to_string(),
            })?;
        Ok(Self {
            client,
            base_url,
            cache: Mutex::new(HashMap::new()),
            cache_ttl,
        })
    }

    /// Return the listing for one category, fetching upstream only when
    /// the cache entry is missing or older than `cache_ttl`. On 404 we
    /// cache an empty listing — Snow Crows occasionally renames or drops
    /// categories and we don't want to retry every call.
    async fn list_category(&self, category: &str) -> Result<Vec<BuildSummary>, CatalogError> {
        {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(category)
                && entry.fetched_at.elapsed() < self.cache_ttl
            {
                return Ok(entry.builds.clone());
            }
        }

        let url = format!("{}/builds/{category}", self.base_url);
        let resp = self.client.get(&url).send().await.map_err(transport)?;
        let status = resp.status();

        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND {
                // 404 is durable: category was renamed or removed upstream.
                // Cache empty for the full TTL so we don't refetch in a loop.
                let mut cache = self.cache.lock().await;
                cache.insert(
                    category.to_owned(),
                    CachedListing {
                        fetched_at: Instant::now(),
                        builds: Vec::new(),
                    },
                );
            } else {
                // 403/5xx is likely transient (Cloudflare burst limit,
                // upstream blip). Return empty + warning but *don't*
                // cache — the next call retries, and the burst window
                // typically clears in seconds.
                tracing::warn!(
                    category,
                    status = %status,
                    "snowcrows: non-success fetching category index; returning empty (not caching)"
                );
            }
            return Ok(Vec::new());
        }

        let html = resp.text().await.map_err(transport)?;
        let builds = parse_listing(&html, category, &self.base_url);
        let mut cache = self.cache.lock().await;
        cache.insert(
            category.to_owned(),
            CachedListing {
                fetched_at: Instant::now(),
                builds: builds.clone(),
            },
        );
        Ok(builds)
    }
}

#[async_trait]
impl BuildCatalog for SnowCrowsCatalog {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn list(&self, filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError> {
        let categories: Vec<&'static str> = match filter.gamemode.as_deref() {
            Some(want) => {
                let want = normalize_gamemode(want);
                CATEGORIES
                    .iter()
                    .copied()
                    .filter(|c| normalize_gamemode(c) == want)
                    .collect()
            }
            // No filter → only the canonical `raids` category. Snow Crows
            // is primarily a raids site, and limiting the cold-start fan-out
            // to a single request keeps things snappy. Other categories
            // (`open-world`, `pvp`, `wvw`) are still reachable by passing
            // an explicit gamemode.
            None => vec![DEFAULT_CATEGORY],
        };

        let mut all = Vec::new();
        for category in categories {
            all.extend(self.list_category(category).await?);
        }

        if let Some(want) = &filter.profession {
            all.retain(|b| b.profession.eq_ignore_ascii_case(want));
        }
        if let Some(limit) = filter.limit {
            let n = usize::try_from(limit).unwrap_or(usize::MAX);
            all.truncate(n);
        }
        Ok(all)
    }

    async fn fetch(&self, slug: &BuildSlug) -> Result<BuildDetail, CatalogError> {
        let slug_str = slug.as_str();
        let parts: Vec<&str> = slug_str.splitn(3, '/').collect();
        if parts.len() != 3 {
            return Err(CatalogError::Parse {
                source_name: SOURCE_NAME.to_owned(),
                message: format!(
                    "snowcrows slug must be `<category>/<profession>/<build-slug>` (got: \
                     {slug_str})"
                ),
            });
        }
        let url = format!("{}/builds/{slug_str}", self.base_url);
        let resp = self.client.get(&url).send().await.map_err(transport)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(CatalogError::NotFound {
                source_name: SOURCE_NAME.to_owned(),
                slug: slug_str.to_owned(),
            });
        }
        if !resp.status().is_success() {
            return Err(CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: format!("status {}", resp.status()),
            });
        }
        let html = resp.text().await.map_err(transport)?;

        let parsed = Html::parse_document(&html);
        let title = select_text(&parsed, "h1").unwrap_or_else(|| parts[2].to_owned());
        let body_text = extract_main_text(&parsed);

        let summary = BuildSummary {
            slug: slug_str.to_owned(),
            title,
            profession: capitalise(parts[1]),
            elite_spec: None,
            role: String::new(),
            gamemode: parts[0].to_owned(),
            rating: Some("Meta".to_owned()),
            source: SOURCE_NAME.to_owned(),
            source_url: url,
        };
        Ok(BuildDetail {
            summary,
            details: serde_json::Value::Null,
            description: body_text,
            chat_code: None,
        })
    }
}

/// Pure parser for a `/builds/<category>` index page. Extracts each
/// `<a href="/builds/<cat>/<prof>/<slug>">` anchor with its first `<h2>`
/// as the title. Anchors with fewer than three path segments
/// (profession-filter pages) or pointing to a different category are
/// skipped. Duplicates by slug are deduped.
fn parse_listing(html: &str, category: &str, base_url: &str) -> Vec<BuildSummary> {
    // .unwrap() is fine here: literals are valid selectors.
    let link_sel = Selector::parse("a").expect("static selector");
    let h2_sel = Selector::parse("h2").expect("static selector");

    let doc = Html::parse_document(html);
    let mut out: Vec<BuildSummary> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for a in doc.select(&link_sel) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        let Some(rest) = href.strip_prefix("/builds/") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 {
            continue;
        }
        let (cat, profession, slug_part) = (parts[0], parts[1], parts[2]);
        if !cat.eq_ignore_ascii_case(category) {
            continue;
        }
        let slug = format!("{cat}/{profession}/{slug_part}");
        if !seen.insert(slug.clone()) {
            continue;
        }
        let title = a
            .select(&h2_sel)
            .next()
            .map(|el| collapse_whitespace(&el.text().collect::<Vec<_>>().join(" ")))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| slug_part.replace('-', " "));
        out.push(BuildSummary {
            slug,
            title,
            profession: capitalise(profession),
            elite_spec: None,
            role: String::new(),
            gamemode: cat.to_owned(),
            rating: Some("Meta".to_owned()),
            source: SOURCE_NAME.to_owned(),
            source_url: format!("{base_url}/builds/{cat}/{profession}/{slug_part}"),
        });
    }
    out
}

fn select_text(doc: &Html, css: &str) -> Option<String> {
    let sel = Selector::parse(css).ok()?;
    let el = doc.select(&sel).next()?;
    let s = el.text().collect::<String>().trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn extract_main_text(doc: &Html) -> String {
    // Snow Crows pages have multiple <article> elements (one per build variant)
    // plus header/nav junk; the first <article> is often nav, while the full
    // build text only shows up if you sum them or fall back to <body>. We try
    // each candidate selector and pick the largest text payload — empirically
    // robust against minor markup tweaks.
    let mut best = String::new();
    for css in ["main", "article", "body"] {
        if let Ok(sel) = Selector::parse(css) {
            for el in doc.select(&sel) {
                let text = collapse_whitespace(&el.text().collect::<Vec<_>>().join(" "));
                if text.len() > best.len() {
                    best = text;
                }
            }
        }
    }
    best
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_owned()
}

/// Normalize a gamemode string for case- and separator-insensitive matching
/// against [`CATEGORIES`]. The MCP tool schema's gamemode enum uses
/// underscores (`open_world`), our URL paths use hyphens (`open-world`),
/// and humans inconsistently mix case — collapse all three so any spelling
/// resolves to the same category.
fn normalize_gamemode(s: &str) -> String {
    s.to_ascii_lowercase().replace('-', "_")
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(chars).collect(),
    }
}

fn transport(e: reqwest::Error) -> CatalogError {
    CatalogError::Transport {
        source_name: SOURCE_NAME.to_owned(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING_FIXTURE: &str = include_str!("../../tests/fixtures/snowcrows_listing.html");

    #[test]
    fn collapse_whitespace_works() {
        assert_eq!(collapse_whitespace("  a  \n\n b\tc  "), "a b c");
    }

    #[test]
    fn select_h1_returns_text() {
        let doc = Html::parse_document("<html><body><h1>Foo Bar</h1></body></html>");
        assert_eq!(select_text(&doc, "h1").as_deref(), Some("Foo Bar"));
    }

    #[test]
    fn select_text_returns_none_when_missing() {
        let doc = Html::parse_document("<html><body></body></html>");
        assert!(select_text(&doc, "h1").is_none());
    }

    #[test]
    fn parse_listing_skips_wrong_category_and_short_paths() {
        let out = parse_listing(LISTING_FIXTURE, "raids", "https://snowcrows.com");
        // The fixture has 4 distinct raid builds (+ one duplicate href + one
        // h2-less anchor = 5 distinct raids slugs total). Strikes anchors,
        // 2-segment profession-filter anchors, and off-site links must all
        // be skipped.
        let slugs: Vec<&str> = out.iter().map(|b| b.slug.as_str()).collect();
        assert!(
            slugs.contains(&"raids/elementalist/power-tempest-spear"),
            "expected ele build; got {slugs:?}"
        );
        assert!(slugs.contains(&"raids/guardian/power-dragonhunter-longbow"));
        assert!(slugs.contains(&"raids/necromancer/condition-scourge-pistol-torch"));
        assert!(
            !slugs.iter().any(|s| s.starts_with("strikes/")),
            "strikes builds must not appear when filtering for raids"
        );
        assert!(
            !slugs
                .iter()
                .any(|s| s == &"raids/elementalist" || s == &"raids/guardian"),
            "profession-filter pages (2 segments) must be dropped"
        );
    }

    #[test]
    fn parse_listing_dedupes_by_slug() {
        let out = parse_listing(LISTING_FIXTURE, "raids", "https://snowcrows.com");
        let count = out
            .iter()
            .filter(|b| b.slug == "raids/elementalist/power-tempest-spear")
            .count();
        assert_eq!(count, 1, "duplicate hrefs must dedupe");
    }

    #[test]
    fn parse_listing_falls_back_to_slug_when_no_h2() {
        let out = parse_listing(LISTING_FIXTURE, "raids", "https://snowcrows.com");
        let entry = out
            .iter()
            .find(|b| b.slug == "raids/necromancer/condition-scourge-pistol-torch")
            .expect("h2-less build present");
        assert_eq!(entry.title, "condition scourge pistol torch");
    }

    #[test]
    fn normalize_gamemode_handles_case_and_separators() {
        assert_eq!(normalize_gamemode("open-world"), "open_world");
        assert_eq!(normalize_gamemode("open_world"), "open_world");
        assert_eq!(normalize_gamemode("Open-World"), "open_world");
        assert_eq!(normalize_gamemode("RAIDS"), "raids");
    }

    #[test]
    fn parse_listing_extracts_title_and_attribution() {
        let out = parse_listing(LISTING_FIXTURE, "raids", "https://snowcrows.com");
        let entry = out
            .iter()
            .find(|b| b.slug == "raids/elementalist/power-tempest-spear")
            .expect("power tempest present");
        assert_eq!(entry.title, "Power Tempest");
        assert_eq!(entry.profession, "Elementalist");
        assert_eq!(entry.gamemode, "raids");
        assert_eq!(entry.rating.as_deref(), Some("Meta"));
        assert_eq!(
            entry.source_url,
            "https://snowcrows.com/builds/raids/elementalist/power-tempest-spear"
        );
    }
}
