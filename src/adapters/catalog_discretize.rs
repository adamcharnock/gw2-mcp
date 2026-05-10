//! `BuildCatalog` adapter for Discretize (fractal-focused builds).
//!
//! Source: <https://github.com/discretize/discretize-guides>
//!
//! - **Listing**: one call to the git-tree API (recursive) gives every build
//!   path in the repo. Cheap and bounded.
//! - **Fetch**: download `builds/<profession>/<slug>/index.md` from the
//!   `raw.githubusercontent.com` CDN; parse YAML front-matter; pass the
//!   markdown body through verbatim under `description`. Embedded
//!   `<Character gear='{...}'>` JSON is left in the body — the LLM can
//!   read it just fine, and parsing every character block out front is
//!   over-typing for an MVP.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::domain::BuildSlug;
use crate::ports::{BuildCatalog, BuildDetail, BuildSummary, CatalogError, CatalogFilter};

const SOURCE_NAME: &str = "discretize";
const REPO: &str = "discretize/discretize-guides";
const DEFAULT_BRANCH: &str = "master";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);

pub struct DiscretizeCatalog {
    client: Client,
    /// Override-able for tests against `wiremock`. Production uses
    /// `https://api.github.com` and `https://raw.githubusercontent.com`.
    api_base: String,
    raw_base: String,
}

impl DiscretizeCatalog {
    pub fn new() -> Result<Self, CatalogError> {
        Self::with_bases(
            "https://api.github.com".to_owned(),
            "https://raw.githubusercontent.com".to_owned(),
        )
    }

    pub fn with_bases(api_base: String, raw_base: String) -> Result<Self, CatalogError> {
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
            api_base,
            raw_base,
        })
    }
}

// `git/trees/<sha>?recursive=1` payload — only the bits we need.
#[derive(Deserialize)]
struct TreeResponse {
    tree: Vec<TreeEntry>,
}
#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

/// YAML front-matter shape; everything is optional so partially-filled
/// pages still parse.
#[derive(Debug, Default, Deserialize)]
struct FrontMatter {
    #[serde(default)]
    title: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    rating: Option<String>,
    #[serde(default)]
    profession: Option<String>,
    #[serde(default)]
    specialization: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    _archive: bool,
    #[serde(default)]
    _hidden: bool,
    /// Catch-all so unknown fields don't cause a parse error.
    #[serde(flatten, default)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[async_trait]
impl BuildCatalog for DiscretizeCatalog {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn list(&self, filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError> {
        let url = format!(
            "{}/repos/{REPO}/git/trees/{DEFAULT_BRANCH}?recursive=1",
            self.api_base,
        );
        let resp = self.client.get(&url).send().await.map_err(transport)?;
        if !resp.status().is_success() {
            return Err(CatalogError::Transport {
                source_name: SOURCE_NAME.to_owned(),
                message: format!("git tree returned {}", resp.status()),
            });
        }
        let body: TreeResponse = resp.json().await.map_err(parse)?;

        // Each build is `builds/<profession>/<slug>/index.md`.
        let mut out: Vec<BuildSummary> = body
            .tree
            .into_iter()
            .filter(|e| e.kind == "blob")
            .filter_map(|e| build_summary_from_path(&e.path))
            .filter(|s| match &filter.profession {
                Some(want) => s.profession.eq_ignore_ascii_case(want),
                None => true,
            })
            .collect();

        // Discretize is fractals-only — so the gamemode filter only excludes
        // when the caller asked for something other than "fractals".
        if let Some(gamemode) = &filter.gamemode
            && !gamemode.eq_ignore_ascii_case("fractals")
        {
            out.clear();
        }

        if let Some(limit) = filter.limit {
            out.truncate(limit as usize);
        }

        Ok(out)
    }

    async fn fetch(&self, slug: &BuildSlug) -> Result<BuildDetail, CatalogError> {
        // Slug format we accept: "<profession>/<build-name>".
        let slug_str = slug.as_str();
        let (profession, build_name) =
            slug_str
                .split_once('/')
                .ok_or_else(|| CatalogError::Parse {
                    source_name: SOURCE_NAME.to_owned(),
                    message: format!("slug must be `<profession>/<build>` (got: {slug_str})"),
                })?;

        let url = format!(
            "{}/{REPO}/{DEFAULT_BRANCH}/builds/{profession}/{build_name}/index.md",
            self.raw_base,
        );
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
                message: format!("raw fetch returned {}", resp.status()),
            });
        }
        let text = resp.text().await.map_err(transport)?;

        let (front, body) = split_frontmatter(&text)?;
        let fm: FrontMatter = serde_yaml_bw::from_str(front).map_err(|e| CatalogError::Parse {
            source_name: SOURCE_NAME.to_owned(),
            message: format!("yaml: {e}"),
        })?;

        let summary = BuildSummary {
            slug: slug_str.to_owned(),
            title: fm.title.clone(),
            profession: fm
                .profession
                .clone()
                .unwrap_or_else(|| profession.to_owned()),
            elite_spec: fm.specialization.clone(),
            role: fm.role.clone(),
            gamemode: "fractals".to_owned(),
            rating: fm.rating.clone(),
            source: SOURCE_NAME.to_owned(),
            source_url: format!("https://discretize.eu/builds/{profession}/{build_name}",),
        };

        // Surface extra front-matter fields under details so the LLM can see
        // boons/conditions/classification/etc. without us typing every variant.
        let details = serde_json::to_value(&fm.extra).unwrap_or(serde_json::Value::Null);

        Ok(BuildDetail {
            summary,
            details,
            description: body.to_owned(),
            chat_code: fm.code,
        })
    }
}

fn build_summary_from_path(path: &str) -> Option<BuildSummary> {
    let parts: Vec<&str> = path.split('/').collect();
    // builds / <profession> / <slug> / index.md
    if parts.len() != 4 || parts[0] != "builds" || parts[3] != "index.md" {
        return None;
    }
    let profession = parts[1];
    let slug_part = parts[2];
    Some(BuildSummary {
        slug: format!("{profession}/{slug_part}"),
        // List view doesn't fetch each file — title/role come from the slug;
        // get_catalog_build returns the rich version.
        title: humanise_slug(slug_part),
        profession: capitalise(profession),
        elite_spec: None,
        role: String::new(),
        gamemode: "fractals".to_owned(),
        rating: None,
        source: SOURCE_NAME.to_owned(),
        source_url: format!("https://discretize.eu/builds/{profession}/{slug_part}"),
    })
}

fn humanise_slug(s: &str) -> String {
    s.split('-').map(capitalise).collect::<Vec<_>>().join(" ")
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(chars).collect(),
    }
}

fn split_frontmatter(text: &str) -> Result<(&str, &str), CatalogError> {
    let stripped = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"));
    let Some(rest) = stripped else {
        return Err(CatalogError::Parse {
            source_name: SOURCE_NAME.to_owned(),
            message: "missing YAML front-matter (no leading `---`)".to_owned(),
        });
    };
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .ok_or_else(|| CatalogError::Parse {
            source_name: SOURCE_NAME.to_owned(),
            message: "unterminated YAML front-matter".to_owned(),
        })?;
    let front = &rest[..end];
    let body_start = end + "\n---\n".len();
    let body = if body_start <= rest.len() {
        rest[body_start..].trim_start()
    } else {
        ""
    };
    Ok((front, body))
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
    fn build_summary_from_path_parses_canonical_layout() {
        let s = build_summary_from_path("builds/guardian/power-dragonhunter/index.md").unwrap();
        assert_eq!(s.slug, "guardian/power-dragonhunter");
        assert_eq!(s.profession, "Guardian");
        assert_eq!(s.gamemode, "fractals");
        assert!(s.source_url.ends_with("guardian/power-dragonhunter"));
    }

    #[test]
    fn build_summary_from_path_rejects_off_layout() {
        assert!(build_summary_from_path("README.md").is_none());
        assert!(build_summary_from_path("builds/guardian/power/extra/index.md").is_none());
        assert!(build_summary_from_path("builds/guardian/power/other.md").is_none());
    }

    #[test]
    fn split_frontmatter_extracts_yaml_and_body() {
        let raw = "---\ntitle: Foo\nrole: DPS\n---\n\nBody content here\n";
        let (front, body) = split_frontmatter(raw).unwrap();
        assert!(front.contains("title: Foo"));
        assert_eq!(body, "Body content here\n");
    }

    #[test]
    fn split_frontmatter_rejects_missing_header() {
        assert!(split_frontmatter("no frontmatter").is_err());
        assert!(split_frontmatter("---\nunclosed").is_err());
    }

    #[test]
    fn humanise_slug_works() {
        assert_eq!(humanise_slug("power-dragonhunter"), "Power Dragonhunter");
    }
}
