//! `BuildCatalog` adapter for Snow Crows: raids, open-world, `PvP`, `WvW`.
//!
//! Source: <https://snowcrows.com>
//!
//! Snow Crows publishes builds as HTML pages (no public API). We:
//! - **List**: scrape `/builds/<category>/<profession>` index pages.
//!   Each page returns one profession's builds; the per-category landing
//!   (`/builds/<category>`) only renders one default profession, so we
//!   must fetch per `(category, profession)` to get full coverage.
//!   Results are cached in-memory per `(category, profession)` pair with
//!   a TTL (default 6h). With no `gamemode` filter we fan out across all
//!   nine professions in `raids` only — Snow Crows' canonical content.
//!   Other categories (`open-world`, `pvp`, `wvw`) are fetched only when
//!   explicitly requested.
//! - **Fetch**: scrape a single `/builds/<category>/<profession>/<slug>`
//!   on demand.
//! - **Graceful degradation**: 404 caches empty for the full TTL
//!   (category was renamed/removed upstream). 403 / 5xx log a warning
//!   and cache empty for a short cooldown window (default 30s) — bounded
//!   retries without locking the user out for hours.
//! - **Stampede prevention**: the cache mutex is held across the HTTP
//!   fetch, so concurrent callers for the same `(category, profession)`
//!   serialize behind one in-flight request.
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
use scraper::{ElementRef, Html, Node, Selector};
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

/// Short TTL for transient-error empties (403, 5xx). Long enough to avoid
/// hammering a temporarily-hostile upstream (Cloudflare burst limits
/// typically clear in 5–10s); short enough that a real outage doesn't
/// lock the user out for hours.
const DEFAULT_ERROR_COOLDOWN: Duration = Duration::from_secs(30);

/// Upper bound on the plain-text description we return per build. Snow
/// Crows pages are ~200 KB of HTML and a naive whole-`<body>` text dump
/// yields ~50 KB of plaintext laden with cookie-banner JS, ad-block
/// detection scripts inlined as text, and full sidebar nav. 16 KiB is
/// plenty for a build's rotation + traits + gear notes and keeps the
/// MCP response within sane limits.
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

/// Known top-level category pages on snowcrows.com. Verified live:
/// `strikes` and `fractals` *used to* exist but now 302 to an invalid
/// path that 403s, so they're excluded. `accessibuilds` is intentionally
/// dropped — it's an accessibility-specific tier, not meta. Unknown
/// gamemode filters return empty; a 404 caches as empty for the full
/// TTL.
const CATEGORIES: &[&str] = &["raids", "open-world", "pvp", "wvw"];

/// The nine GW2 professions, in the lower-cased form Snow Crows uses in
/// its URL paths. `/builds/<category>` redirects to the page for one
/// specific profession (typically `elementalist`) — to get every
/// profession's builds we must fetch `/builds/<category>/<profession>`
/// once per profession.
const PROFESSIONS: &[&str] = &[
    "elementalist",
    "engineer",
    "guardian",
    "mesmer",
    "necromancer",
    "ranger",
    "revenant",
    "thief",
    "warrior",
];

/// Category returned when no `gamemode` filter is supplied. Snow Crows is
/// primarily a raids site; we still fan out across all nine professions
/// in that one category, but we don't also iterate `open-world` / `pvp` /
/// `wvw` (that would be 36 cold-start requests). Callers who want those
/// categories must pass an explicit `gamemode`.
const DEFAULT_CATEGORY: &str = "raids";

struct CachedListing {
    fetched_at: Instant,
    builds: Vec<BuildSummary>,
    /// `true` if this entry was cached because of a transient HTTP error
    /// (403, 5xx); use `error_cooldown` for its freshness window instead
    /// of `cache_ttl`. `false` for successful fetches (including durable
    /// 404s, which represent a real "category doesn't exist" answer).
    is_error: bool,
}

pub struct SnowCrowsCatalog {
    client: Client,
    base_url: String,
    cache: Mutex<HashMap<String, CachedListing>>,
    cache_ttl: Duration,
    error_cooldown: Duration,
}

impl SnowCrowsCatalog {
    pub fn new() -> Result<Self, CatalogError> {
        Self::with_base_url(DEFAULT_BASE.to_owned())
    }

    pub fn with_base_url(base_url: String) -> Result<Self, CatalogError> {
        Self::with_options(base_url, DEFAULT_CACHE_TTL)
    }

    /// Construct with an explicit cache TTL. Used by tests to verify both
    /// cache-hit (long TTL) and cache-miss (zero TTL) behavior. The
    /// transient-error cooldown is left at the default; tests that need
    /// to override it can chain [`SnowCrowsCatalog::with_error_cooldown`].
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
            error_cooldown: DEFAULT_ERROR_COOLDOWN,
        })
    }

    /// Override the transient-error cooldown TTL. Useful in tests to
    /// force immediate retries (set to `Duration::ZERO`) or to bound
    /// retries tightly (e.g. 5s) when running against a flaky CI
    /// network.
    #[must_use]
    pub fn with_error_cooldown(mut self, cooldown: Duration) -> Self {
        self.error_cooldown = cooldown;
        self
    }

    /// Return one profession's listing inside one category, fetching
    /// `/builds/<category>/<profession>` upstream only when the cache
    /// entry is missing or stale. Snow Crows' per-category landing
    /// pages (`/builds/<category>`) only render one profession's grid
    /// — to get every profession we must fetch each
    /// `/builds/<category>/<profession>` separately. Cache key is
    /// `"<category>/<profession>"`.
    ///
    /// On 404 we cache empty for the full TTL. On 403/5xx we log a
    /// warning and return empty *without* caching, so the next call
    /// retries (Cloudflare burst windows clear in seconds).
    ///
    /// ## Stampede prevention
    ///
    /// The cache mutex is held across the HTTP fetch, so two concurrent
    /// callers asking for the same (category, profession) will not both
    /// hit upstream: the first acquires the lock, fetches, and writes;
    /// the second waits, then sees the fresh entry. The cost is that
    /// concurrent callers asking for *different* keys also serialize
    /// behind one in-flight fetch — fine for an MCP server with at most
    /// a handful of concurrent tool calls, and a feature rather than a
    /// bug when fanning out cold-start fetches against a rate-limited
    /// upstream.
    async fn list_profession_in_category(
        &self,
        category: &str,
        profession: &str,
    ) -> Result<Vec<BuildSummary>, CatalogError> {
        let cache_key = format!("{category}/{profession}");
        let mut cache = self.cache.lock().await;

        if let Some(entry) = cache.get(&cache_key) {
            let ttl = if entry.is_error {
                self.error_cooldown
            } else {
                self.cache_ttl
            };
            if entry.fetched_at.elapsed() < ttl {
                return Ok(entry.builds.clone());
            }
        }

        let url = format!("{}/builds/{category}/{profession}", self.base_url);
        let resp = self.client.get(&url).send().await.map_err(transport)?;
        let status = resp.status();

        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND {
                // 404 is durable: category was renamed or removed
                // upstream. Cache empty for the full TTL.
                cache.insert(
                    cache_key,
                    CachedListing {
                        fetched_at: Instant::now(),
                        builds: Vec::new(),
                        is_error: false,
                    },
                );
            } else {
                // 403/5xx is transient. Cache empty for the short
                // `error_cooldown` window so repeated calls within
                // (typically) 30s don't hammer the upstream; after the
                // cooldown the next call will refetch.
                tracing::warn!(
                    category,
                    profession,
                    status = %status,
                    cooldown_secs = self.error_cooldown.as_secs(),
                    "snowcrows: non-success fetching profession index; caching empty under cooldown"
                );
                cache.insert(
                    cache_key,
                    CachedListing {
                        fetched_at: Instant::now(),
                        builds: Vec::new(),
                        is_error: true,
                    },
                );
            }
            return Ok(Vec::new());
        }

        let html = resp.text().await.map_err(transport)?;
        let builds = parse_listing(&html, category, profession, &self.base_url);
        cache.insert(
            cache_key,
            CachedListing {
                fetched_at: Instant::now(),
                builds: builds.clone(),
                is_error: false,
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
            // No gamemode → raids only (Snow Crows' canonical category).
            // We still fan out across all nine professions in that one
            // category. Other categories require an explicit gamemode.
            None => vec![DEFAULT_CATEGORY],
        };

        let professions: Vec<&'static str> = match filter.profession.as_deref() {
            Some(want) => PROFESSIONS
                .iter()
                .copied()
                .filter(|p| p.eq_ignore_ascii_case(want))
                .collect(),
            None => PROFESSIONS.to_vec(),
        };

        let mut all = Vec::new();
        for category in &categories {
            for profession in &professions {
                all.extend(
                    self.list_profession_in_category(category, profession)
                        .await?,
                );
            }
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

/// Pure parser for a `/builds/<category>/<profession>` index page.
/// Extracts each `<a href="/builds/<cat>/<prof>/<slug>">` anchor whose
/// `cat` and `prof` match the page we asked for, with its first `<h2>`
/// as the title. Anchors with fewer than three path segments
/// (profession-filter / cross-profession nav) and anchors pointing at a
/// different category or profession are skipped. Duplicates by slug
/// are deduped.
fn parse_listing(
    html: &str,
    category: &str,
    profession: &str,
    base_url: &str,
) -> Vec<BuildSummary> {
    // .expect() is fine here: literals are statically-valid selectors.
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
        let (cat, prof, slug_part) = (parts[0], parts[1], parts[2]);
        if !cat.eq_ignore_ascii_case(category) || !prof.eq_ignore_ascii_case(profession) {
            continue;
        }
        let slug = format!("{cat}/{prof}/{slug_part}");
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
            profession: capitalise(prof),
            elite_spec: None,
            role: String::new(),
            gamemode: cat.to_owned(),
            rating: Some("Meta".to_owned()),
            source: SOURCE_NAME.to_owned(),
            source_url: format!("{base_url}/builds/{cat}/{prof}/{slug_part}"),
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

/// Tag names whose subtrees we exclude from the description. Scripts and
/// styles are obvious (Snow Crows inlines `nitroAds` and Cloudflare
/// challenge JS that's hundreds of lines of code); nav / header / footer
/// / aside are site chrome with no per-build content; `iframe` / `svg`
/// can carry text-but-not-useful-text; `noscript` is fallback for
/// ad-blocked users and isn't relevant.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "nav", "header", "footer", "aside", "iframe", "svg",
];

/// Collect plain text from `<body>`, skipping chrome subtrees, with a
/// hard byte cap. Returns whitespace-collapsed text.
fn extract_main_text(doc: &Html) -> String {
    let body_sel = Selector::parse("body").expect("static selector");
    let Some(body) = doc.select(&body_sel).next() else {
        return String::new();
    };
    let mut buf = String::with_capacity(4 * 1024);
    collect_text_filtered(body, &mut buf, MAX_DESCRIPTION_BYTES);
    collapse_whitespace(&buf)
}

/// Recursive pre-order text collector that prunes any subtree whose
/// root tag is in [`SKIP_TAGS`]. Stops appending as soon as `buf` hits
/// `max_len` bytes; the final post-trim might leave us slightly under.
fn collect_text_filtered(element: ElementRef<'_>, buf: &mut String, max_len: usize) {
    if buf.len() >= max_len {
        return;
    }
    if SKIP_TAGS.contains(&element.value().name()) {
        return;
    }
    for child in element.children() {
        if buf.len() >= max_len {
            return;
        }
        match child.value() {
            Node::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let remaining = max_len - buf.len();
                if trimmed.len() < remaining {
                    buf.push_str(trimmed);
                    buf.push(' ');
                } else {
                    // Truncate at the nearest char boundary at or below
                    // `remaining` so we don't split a multi-byte UTF-8 codepoint.
                    let mut end = remaining;
                    while end > 0 && !trimmed.is_char_boundary(end) {
                        end -= 1;
                    }
                    buf.push_str(&trimmed[..end]);
                    return;
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    collect_text_filtered(child_el, buf, max_len);
                }
            }
            _ => {}
        }
    }
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
    fn parse_listing_filters_to_requested_profession_and_skips_short_paths() {
        // Asking for (raids, elementalist) returns only ele builds. The
        // fixture also has guardian and necromancer anchors plus a
        // wrong-category (strikes) one — all must be skipped.
        let out = parse_listing(
            LISTING_FIXTURE,
            "raids",
            "elementalist",
            "https://snowcrows.com",
        );
        let slugs: Vec<&str> = out.iter().map(|b| b.slug.as_str()).collect();
        assert!(
            slugs.contains(&"raids/elementalist/power-tempest-spear"),
            "expected ele build; got {slugs:?}"
        );
        assert!(
            slugs.iter().all(|s| s.starts_with("raids/elementalist/")),
            "asking for (raids, elementalist) must return ONLY ele raid builds; got {slugs:?}"
        );
        assert!(
            !slugs
                .iter()
                .any(|s| s == &"raids/elementalist" || s == &"raids/guardian"),
            "profession-filter pages (2 segments) must be dropped"
        );
    }

    #[test]
    fn parse_listing_extracts_guardian_when_asked() {
        let out = parse_listing(
            LISTING_FIXTURE,
            "raids",
            "guardian",
            "https://snowcrows.com",
        );
        let slugs: Vec<&str> = out.iter().map(|b| b.slug.as_str()).collect();
        assert!(slugs.contains(&"raids/guardian/power-dragonhunter-longbow"));
        assert!(slugs.iter().all(|s| s.starts_with("raids/guardian/")));
    }

    #[test]
    fn parse_listing_dedupes_by_slug() {
        let out = parse_listing(
            LISTING_FIXTURE,
            "raids",
            "elementalist",
            "https://snowcrows.com",
        );
        let count = out
            .iter()
            .filter(|b| b.slug == "raids/elementalist/power-tempest-spear")
            .count();
        assert_eq!(count, 1, "duplicate hrefs must dedupe");
    }

    #[test]
    fn parse_listing_falls_back_to_slug_when_no_h2() {
        let out = parse_listing(
            LISTING_FIXTURE,
            "raids",
            "necromancer",
            "https://snowcrows.com",
        );
        let entry = out
            .iter()
            .find(|b| b.slug == "raids/necromancer/condition-scourge-pistol-torch")
            .expect("h2-less build present");
        assert_eq!(entry.title, "condition scourge pistol torch");
    }

    #[test]
    fn extract_main_text_skips_script_style_and_chrome() {
        let html = r"<html><body>
            <nav>NAV JUNK NAV JUNK</nav>
            <header>HEADER JUNK</header>
            <script>var x = 'INLINE JS BODY';</script>
            <style>.foo { color: red; CSS JUNK }</style>
            <main>
                <h1>Build Title Here</h1>
                <p>This is the real build description.</p>
                <script>more ads</script>
            </main>
            <footer>FOOTER JUNK</footer>
            <aside>ASIDE JUNK</aside>
            <noscript>NOSCRIPT JUNK</noscript>
        </body></html>";
        let doc = Html::parse_document(html);
        let text = extract_main_text(&doc);
        assert!(text.contains("Build Title Here"));
        assert!(text.contains("This is the real build description"));
        for chrome in [
            "NAV JUNK",
            "HEADER JUNK",
            "INLINE JS BODY",
            "CSS JUNK",
            "FOOTER JUNK",
            "ASIDE JUNK",
            "NOSCRIPT JUNK",
            "more ads",
        ] {
            assert!(
                !text.contains(chrome),
                "extract_main_text leaked chrome: {chrome} in {text}"
            );
        }
    }

    #[test]
    fn extract_main_text_respects_byte_cap() {
        // Build a body with a single huge text node well over the cap.
        let huge = "x".repeat(MAX_DESCRIPTION_BYTES * 4);
        let html = format!("<html><body><p>{huge}</p></body></html>");
        let doc = Html::parse_document(&html);
        let text = extract_main_text(&doc);
        // We may trim slightly during whitespace collapse; allow a small
        // overshoot but well under 2x the cap.
        assert!(
            text.len() <= MAX_DESCRIPTION_BYTES + 256,
            "extract_main_text returned {} bytes; cap is {}",
            text.len(),
            MAX_DESCRIPTION_BYTES
        );
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
        let out = parse_listing(
            LISTING_FIXTURE,
            "raids",
            "elementalist",
            "https://snowcrows.com",
        );
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
