//! HTTP adapter for the GW2 wiki `MediaWiki` API.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::adapters::error_body::truncate_error_body;
use crate::domain::{SearchLimit, SearchQuery, SearchResult};
use crate::ports::{Wiki, WikiError};

const DEFAULT_BASE_URL: &str = "https://wiki.guildwars2.com/api.php";
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpWiki {
    client: Client,
    base_url: String,
}

impl HttpWiki {
    pub fn new() -> Result<Self, WikiError> {
        Self::with_base_url(DEFAULT_BASE_URL.to_owned())
    }

    pub fn with_base_url(base_url: String) -> Result<Self, WikiError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| WikiError::Transport(e.to_string()))?;
        Ok(Self { client, base_url })
    }
}

// MediaWiki search response shape.
#[derive(Deserialize)]
struct SearchWire {
    query: SearchWireQuery,
}
#[derive(Deserialize)]
struct SearchWireQuery {
    search: Vec<SearchWireHit>,
}
#[derive(Deserialize)]
struct SearchWireHit {
    title: String,
}

// MediaWiki extracts response shape (only the fields we need).
#[derive(Deserialize)]
struct ExtractWire {
    query: ExtractWireQuery,
}
#[derive(Deserialize)]
struct ExtractWireQuery {
    pages: serde_json::Map<String, serde_json::Value>,
}

#[async_trait]
impl Wiki for HttpWiki {
    async fn search(
        &self,
        query: &SearchQuery,
        limit: SearchLimit,
    ) -> Result<Vec<SearchResult>, WikiError> {
        let url = &self.base_url;
        let resp = self
            .client
            .get(url)
            .query(&[
                ("action", "query"),
                ("format", "json"),
                ("list", "search"),
                ("srsearch", query.as_str()),
                ("srlimit", &limit.get().to_string()),
                ("srprop", "size|wordcount|timestamp|snippet"),
            ])
            .send()
            .await
            .map_err(|e| WikiError::Transport(e.to_string()))?;
        let resp = check_status(resp).await?;
        let wire: SearchWire = resp
            .json()
            .await
            .map_err(|e| WikiError::Decode(e.to_string()))?;

        Ok(wire
            .query
            .search
            .into_iter()
            .map(|h| SearchResult {
                title: h.title,
                url: String::new(), // Filled in by service layer.
                extract: String::new(),
            })
            .collect())
    }

    async fn fetch_extract(&self, title: &str) -> Result<String, WikiError> {
        let url = &self.base_url;
        let resp = self
            .client
            .get(url)
            .query(&[
                ("action", "query"),
                ("format", "json"),
                ("prop", "extracts"),
                ("titles", title),
                ("exintro", "true"),
                ("explaintext", "true"),
                ("exsectionformat", "plain"),
                ("exchars", "500"),
            ])
            .send()
            .await
            .map_err(|e| WikiError::Transport(e.to_string()))?;
        let resp = check_status(resp).await?;
        let wire: ExtractWire = resp
            .json()
            .await
            .map_err(|e| WikiError::Decode(e.to_string()))?;

        // pages is an object keyed by page id; we want the first entry's "extract".
        let extract = wire
            .query
            .pages
            .values()
            .next()
            .and_then(|page| page.get("extract").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_owned();
        Ok(extract)
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, WikiError> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        // Cap before propagation — wiki maintenance pages are ~50 kB of HTML
        // and would otherwise blow the LLM's context budget.
        Err(WikiError::Status {
            status,
            body: truncate_error_body(&body),
        })
    }
}

/// Strip `MediaWiki` search-result HTML noise.
///
/// Public so the same pure transformation can be unit-tested without an
/// HTTP round-trip.
#[must_use]
pub fn clean_snippet(input: &str) -> String {
    let mut s = input
        .replace("<span class=\"searchmatch\">", "")
        .replace("</span>", "")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");

    s = s.replace(['\n', '\t'], " ");
    while s.contains("  ") {
        s = s.replace("  ", " ");
    }
    s.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_snippet_strips_searchmatch_spans() {
        let s = clean_snippet(r#"<span class="searchmatch">Dragon</span> Bash is a festival"#);
        assert_eq!(s, "Dragon Bash is a festival");
    }

    #[test]
    fn clean_snippet_decodes_entities() {
        let s = clean_snippet("&quot;Dragon Bash&quot; &amp; events &lt;test&gt;");
        assert_eq!(s, r#""Dragon Bash" & events <test>"#);
    }

    #[test]
    fn clean_snippet_collapses_whitespace() {
        let s = clean_snippet("Dragon\nBash\t  festival   with    spaces");
        assert_eq!(s, "Dragon Bash festival with spaces");
    }
}
