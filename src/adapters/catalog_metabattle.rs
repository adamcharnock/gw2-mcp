//! `BuildCatalog` adapter for `MetaBattle` (community wiki).
//!
//! Source: <https://metabattle.com/wiki/api.php> — full `MediaWiki` API.
//!
//! - **Listing**: `action=query&list=categorymembers&cmtitle=Category:Meta` (or
//!   `Category:Great`) gives a flat page list.
//! - **Fetch**: `action=parse&page=Build:Foo` returns wikitext we pass through
//!   verbatim under `description` plus the parsed text. Templates like
//!   `{{Build}}`, `{{Skill bar}}`, etc. are visible to the LLM.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::ports::{BuildCatalog, BuildDetail, BuildSummary, CatalogError, CatalogFilter};

const SOURCE_NAME: &str = "metabattle";
const DEFAULT_BASE: &str = "https://metabattle.com/wiki/api.php";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);

pub struct MetaBattleCatalog {
    client: Client,
    base_url: String,
}

impl MetaBattleCatalog {
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

#[derive(Deserialize)]
struct ListResponse {
    query: ListQuery,
}
#[derive(Deserialize)]
struct ListQuery {
    categorymembers: Vec<CategoryMember>,
}
#[derive(Deserialize)]
struct CategoryMember {
    title: String,
}

#[derive(Deserialize)]
struct ParseResponse {
    parse: ParseInner,
}
#[derive(Deserialize)]
struct ParseInner {
    title: String,
    wikitext: WikiTextHolder,
}
#[derive(Deserialize)]
struct WikiTextHolder {
    #[serde(rename = "*")]
    star: String,
}

#[async_trait]
impl BuildCatalog for MetaBattleCatalog {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn list(&self, filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError> {
        // Default to Category:Meta for highest-tier builds; fall back to Great
        // if requested. We don't currently expose the category as a filter
        // dimension — keep it simple for the MVP.
        let limit = filter.limit.unwrap_or(50).min(500).to_string();
        let resp = self
            .client
            .get(&self.base_url)
            .query(&[
                ("action", "query"),
                ("list", "categorymembers"),
                ("cmtitle", "Category:Meta_builds"),
                ("cmlimit", &limit),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(transport)?;
        if !resp.status().is_success() {
            return Err(CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: format!("status {}", resp.status()),
            });
        }
        let body: ListResponse = resp.json().await.map_err(parse)?;

        Ok(body
            .query
            .categorymembers
            .into_iter()
            .filter_map(|m| {
                // MetaBattle build pages are titled "Build:Profession - Build name".
                let stripped = m.title.strip_prefix("Build:")?;
                let (profession_raw, build_part) = stripped.split_once(" - ")?;
                let profession = profession_raw.trim().to_owned();
                if let Some(want) = &filter.profession
                    && !profession.eq_ignore_ascii_case(want)
                {
                    return None;
                }
                let slug = m.title.replace(' ', "_");
                Some(BuildSummary {
                    slug: slug.clone(),
                    title: build_part.trim().to_owned(),
                    profession,
                    elite_spec: None,
                    role: String::new(),
                    gamemode: String::new(),
                    rating: Some("Meta".to_owned()),
                    source: SOURCE_NAME.to_owned(),
                    source_url: format!("https://metabattle.com/wiki/{slug}"),
                })
            })
            .collect())
    }

    async fn fetch(&self, slug: &str) -> Result<BuildDetail, CatalogError> {
        // Accept slugs in either underscore or space form; MediaWiki tolerates both.
        let page = slug.replace('_', " ");
        let resp = self
            .client
            .get(&self.base_url)
            .query(&[
                ("action", "parse"),
                ("page", page.as_str()),
                ("prop", "wikitext"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(transport)?;
        if !resp.status().is_success() {
            return Err(CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: format!("status {}", resp.status()),
            });
        }
        // MediaWiki returns 200 with `{"error":...}` for missing pages.
        let value: serde_json::Value = resp.json().await.map_err(parse)?;
        if value.get("error").is_some() {
            return Err(CatalogError::NotFound {
                source_name: SOURCE_NAME.to_owned(),
                slug: slug.to_owned(),
            });
        }
        let parsed: ParseResponse =
            serde_json::from_value(value).map_err(|e| CatalogError::Parse {
                source_name: SOURCE_NAME.to_owned(),
                message: e.to_string(),
            })?;

        let title = parsed.parse.title.clone();
        let wikitext = parsed.parse.wikitext.star;
        let summary = BuildSummary {
            slug: slug.to_owned(),
            title: title.clone(),
            profession: extract_profession(&wikitext).unwrap_or_default(),
            elite_spec: None,
            role: String::new(),
            gamemode: String::new(),
            rating: Some("Meta".to_owned()),
            source: SOURCE_NAME.to_owned(),
            source_url: format!("https://metabattle.com/wiki/{}", slug.replace(' ', "_")),
        };
        Ok(BuildDetail {
            summary,
            details: serde_json::Value::Null,
            description: wikitext,
            chat_code: None,
        })
    }
}

/// Pull `profession=Foo` out of the `{{Build}}` infobox if present.
fn extract_profession(wikitext: &str) -> Option<String> {
    for line in wikitext.lines() {
        let trimmed = line.trim_start_matches('|').trim();
        if let Some(rest) = trimmed.strip_prefix("profession")
            && let Some(val) = rest.trim_start().strip_prefix('=')
        {
            return Some(val.trim().to_owned());
        }
    }
    None
}

fn transport(e: reqwest::Error) -> CatalogError {
    CatalogError::Transport {
        source_name: SOURCE_NAME.to_owned(),
        message: e.to_string(),
    }
}

fn parse(e: reqwest::Error) -> CatalogError {
    CatalogError::Parse {
        source_name: SOURCE_NAME.to_owned(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_profession_from_infobox() {
        let wt = "{{Build\n|profession = Guardian\n|rating = Meta\n}}";
        assert_eq!(extract_profession(wt).as_deref(), Some("Guardian"));
    }

    #[test]
    fn extract_profession_returns_none_when_absent() {
        assert_eq!(extract_profession("nothing here"), None);
    }
}
