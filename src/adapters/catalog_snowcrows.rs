//! `BuildCatalog` adapter for Snow Crows (raid/strike meta).
//!
//! Snow Crows publishes no machine-readable API, only HTML. Their
//! `robots.txt` carries a Cloudflare `ai-train=no` content signal — we
//! comply by:
//! - **No bulk listing.** `list()` returns an empty vec with a note in
//!   the slug; the LLM is expected to know specific build names from
//!   user prompt or wiki/MetaBattle results, then call `fetch(slug)`.
//! - **On-demand fetch only**, with attribution: every response includes
//!   `source_url` so the LLM can cite back to Snow Crows.
//! - **No long-term caching.** `service.rs` doesn't cache catalog calls
//!   at the moment, so this happens for free.
//!
//! `slug` format: `<category>/<profession>/<build-slug>`, mirroring the URL.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use scraper::{Html, Selector};

use crate::ports::{BuildCatalog, BuildDetail, BuildSummary, CatalogError, CatalogFilter};

const SOURCE_NAME: &str = "snowcrows";
const DEFAULT_BASE: &str = "https://snowcrows.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp; contact: github issues)"
);

pub struct SnowCrowsCatalog {
    client: Client,
    base_url: String,
}

impl SnowCrowsCatalog {
    pub fn new() -> Result<Self, CatalogError> {
        Self::with_base_url(DEFAULT_BASE.to_owned())
    }

    pub fn with_base_url(base_url: String) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: e.to_string(),
            })?;
        Ok(Self { client, base_url })
    }
}

#[async_trait]
impl BuildCatalog for SnowCrowsCatalog {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn list(&self, _filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError> {
        // We do not bulk-scrape Snow Crows. Returning empty is intentional;
        // see the module docstring for the rationale.
        Ok(Vec::new())
    }

    async fn fetch(&self, slug: &str) -> Result<BuildDetail, CatalogError> {
        // slug shape: <category>/<profession>/<build-slug>
        let parts: Vec<&str> = slug.splitn(3, '/').collect();
        if parts.len() != 3 {
            return Err(CatalogError::Parse {
                source_name: SOURCE_NAME.to_owned(),
                message: format!(
                    "snowcrows slug must be `<category>/<profession>/<build-slug>` (got: {slug})"
                ),
            });
        }
        let url = format!("{}/builds/{slug}", self.base_url);
        let resp = self.client.get(&url).send().await.map_err(transport)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(CatalogError::NotFound {
                source_name: SOURCE_NAME.to_owned(),
                slug: slug.to_owned(),
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
            slug: slug.to_owned(),
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
}
