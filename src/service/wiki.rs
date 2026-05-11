//! Wiki search + extract enrichment. Hits the `Wiki` port for search,
//! then fans out one extract fetch per hit (cached separately so repeat
//! queries reuse extracts even when the search results differ).

use tracing::{debug, warn};

use super::{Service, ServiceError, WIKI_TTL};
use crate::domain::{SearchLimit, SearchQuery, SearchResponse};

impl Service {
    /// Search the wiki, enriching each hit with a short prose extract.
    pub async fn search_wiki(
        &self,
        query: &SearchQuery,
        limit: SearchLimit,
    ) -> Result<SearchResponse, ServiceError> {
        let cache_key = wiki_search_cache_key(query, limit);

        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(resp) = serde_json::from_str::<SearchResponse>(&json)
        {
            debug!(query = %query, "wiki search cache hit");
            return Ok(resp);
        }

        let mut results = self.wiki.search(query, limit).await?;
        // Enrich with extracts. A failure on one page must not poison the rest.
        for r in &mut results {
            match self.fetch_or_cache_extract(&r.title).await {
                Ok(extract) => r.extract = extract,
                Err(e) => warn!(title = %r.title, error = ?e, "failed to fetch extract"),
            }
            r.url = wiki_page_url(&r.title);
        }

        let total = results.len();
        let response = SearchResponse {
            query: query.as_str().to_owned(),
            results,
            total,
            searched_at: self.clock.now(),
        };

        if let Ok(json) = serde_json::to_string(&response) {
            self.cache.set(&cache_key, json, WIKI_TTL).await;
        }

        Ok(response)
    }

    async fn fetch_or_cache_extract(&self, title: &str) -> Result<String, ServiceError> {
        let key = wiki_extract_cache_key(title);
        if let Some(extract) = self.cache.get(&key).await {
            return Ok(extract);
        }
        let extract = self.wiki.fetch_extract(title).await?;
        self.cache.set(&key, extract.clone(), WIKI_TTL).await;
        Ok(extract)
    }
}

fn wiki_search_cache_key(query: &SearchQuery, limit: SearchLimit) -> String {
    format!("wiki:search:{}:{}", query.normalised(), limit.get())
}

fn wiki_extract_cache_key(title: &str) -> String {
    format!("wiki:extract:{title}")
}

/// Public so adapters can build canonical wiki URLs.
#[must_use]
pub fn wiki_page_url(title: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>();
    format!("https://wiki.guildwars2.com/wiki/{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_page_url_escapes_spaces() {
        let url = wiki_page_url("Dragon Bash");
        assert_eq!(url, "https://wiki.guildwars2.com/wiki/Dragon+Bash");
    }

    #[test]
    fn wiki_search_cache_key_includes_limit() {
        let q = SearchQuery::new("foo").unwrap();
        let l = SearchLimit::new(5).unwrap();
        assert_eq!(wiki_search_cache_key(&q, l), "wiki:search:foo:5");
    }
}
