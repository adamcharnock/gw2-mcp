//! HTTP adapter for the Guild Wars 2 v2 REST API.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::domain::{
    ApiKey, CharacterName, Currency, CurrencyId, Skill, SkillId, Specialization, SpecializationId,
    Trait, TraitId, WalletEntry,
};
use crate::ports::{Gw2Api, Gw2ApiError};

const DEFAULT_BASE_URL: &str = "https://api.guildwars2.com/v2";
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpGw2Api {
    client: Client,
    base_url: String,
}

impl HttpGw2Api {
    pub fn new() -> Result<Self, Gw2ApiError> {
        Self::with_base_url(DEFAULT_BASE_URL.to_owned())
    }

    /// Override the base URL — used by integration tests against `wiremock`.
    pub fn with_base_url(base_url: String) -> Result<Self, Gw2ApiError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        Ok(Self { client, base_url })
    }
}

#[async_trait]
impl Gw2Api for HttpGw2Api {
    async fn fetch_wallet(&self, key: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError> {
        // Inline payload type so the wire format is local to this adapter.
        #[derive(Deserialize)]
        struct Wire {
            id: i64,
            value: i64,
        }

        let url = format!("{}/account/wallet", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(key.expose())
            .send()
            .await
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;

        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            return Err(Gw2ApiError::Unauthorized);
        }
        let resp = check_status(resp).await?;
        let wire: Vec<Wire> = resp
            .json()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;

        wire.into_iter()
            .map(|w| {
                Ok(WalletEntry {
                    id: CurrencyId::new(w.id)
                        .map_err(|e| Gw2ApiError::Decode(format!("currency id {}: {e}", w.id)))?,
                    value: w.value,
                })
            })
            .collect()
    }

    async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError> {
        let url = format!("{}/currencies", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = check_status(resp).await?;
        let raw: Vec<i64> = resp
            .json()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;

        raw.into_iter()
            .map(|id| {
                CurrencyId::new(id)
                    .map_err(|e| Gw2ApiError::Decode(format!("currency id {id}: {e}")))
            })
            .collect()
    }

    async fn fetch_currencies(
        &self,
        ids: &[CurrencyId],
    ) -> Result<BTreeMap<CurrencyId, Currency>, Gw2ApiError> {
        self.fetch_by_ids("currencies", ids, |c: Currency| (c.id, c))
            .await
    }

    async fn fetch_skills(&self, ids: &[SkillId]) -> Result<BTreeMap<SkillId, Skill>, Gw2ApiError> {
        self.fetch_by_ids("skills", ids, |s: Skill| (s.id, s)).await
    }

    async fn fetch_traits(&self, ids: &[TraitId]) -> Result<BTreeMap<TraitId, Trait>, Gw2ApiError> {
        self.fetch_by_ids("traits", ids, |t: Trait| (t.id, t)).await
    }

    async fn fetch_specializations(
        &self,
        ids: &[SpecializationId],
    ) -> Result<BTreeMap<SpecializationId, Specialization>, Gw2ApiError> {
        self.fetch_by_ids("specializations", ids, |s: Specialization| (s.id, s))
            .await
    }

    async fn fetch_buildtabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
        let url = format!(
            "{}/characters/{}/buildtabs?tabs=all",
            self.base_url,
            url_encode_segment(name.as_str()),
        );
        self.fetch_authed_json(&url, key, Some(name)).await
    }

    async fn fetch_equipmenttabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
        let url = format!(
            "{}/characters/{}/equipmenttabs?tabs=all",
            self.base_url,
            url_encode_segment(name.as_str()),
        );
        self.fetch_authed_json(&url, key, Some(name)).await
    }
}

impl HttpGw2Api {
    /// Generic helper for `/v2/<endpoint>?ids=…`. Chunks at the GW2 200-id limit.
    async fn fetch_by_ids<Id, T, K, F>(
        &self,
        endpoint: &str,
        ids: &[Id],
        index: F,
    ) -> Result<BTreeMap<K, T>, Gw2ApiError>
    where
        Id: std::fmt::Display,
        T: for<'de> Deserialize<'de>,
        K: Ord,
        F: Fn(T) -> (K, T),
    {
        const CHUNK: usize = 200;
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let mut out = BTreeMap::new();
        for chunk in ids.chunks(CHUNK) {
            let ids_param = chunk
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let url = format!("{}/{endpoint}?ids={ids_param}", self.base_url);
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
            let resp = check_status(resp).await?;
            let items: Vec<T> = resp
                .json()
                .await
                .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;
            for item in items {
                let (k, v) = index(item);
                out.insert(k, v);
            }
        }
        Ok(out)
    }

    /// `character` lets us recognise the GW2-specific "no such character"
    /// payload and surface it as a typed [`Gw2ApiError::CharacterNotFound`]
    /// rather than a noisy raw-JSON dump.
    async fn fetch_authed_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        key: &ApiKey,
        character: Option<&CharacterName>,
    ) -> Result<T, Gw2ApiError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(key.expose())
            .send()
            .await
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            return Err(Gw2ApiError::Unauthorized);
        }
        let resp = check_status_with_context(resp, character).await?;
        resp.json::<T>()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))
    }
}

fn url_encode_segment(s: &str) -> String {
    // GW2 character names allow spaces and apostrophes. We must use *path*
    // percent-encoding (space → `%20`), not form-encoding (space → `+`) —
    // the GW2 API responds to `Vesta+Vey` with HTTP 400 "no such character".
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod url_encode_tests {
    use super::url_encode_segment;

    #[test]
    fn space_becomes_percent_20_not_plus() {
        // The whole reason this function exists.
        assert_eq!(url_encode_segment("Vesta Vey"), "Vesta%20Vey");
    }

    #[test]
    fn apostrophe_is_encoded() {
        assert_eq!(url_encode_segment("Lara's"), "Lara%27s");
    }

    #[test]
    fn pure_ascii_alnum_is_unchanged() {
        assert_eq!(url_encode_segment("HeroOne"), "HeroOne");
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, Gw2ApiError> {
    check_status_with_context(resp, None).await
}

/// Map a non-success GW2 response into the most specific error variant we
/// can. GW2 errors are uniformly shaped `{"text":"..."}`, which we extract
/// and pattern-match against the handful of cases worth surfacing typed.
async fn check_status_with_context(
    resp: reqwest::Response,
    character: Option<&CharacterName>,
) -> Result<reqwest::Response, Gw2ApiError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    if status == 429 {
        return Err(Gw2ApiError::RateLimited);
    }
    if status == 401 || status == 403 {
        return Err(Gw2ApiError::Unauthorized);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = extract_gw2_error_text(&body).unwrap_or_else(|| body.clone());

    if message.eq_ignore_ascii_case("no such character")
        && let Some(name) = character
    {
        return Err(Gw2ApiError::CharacterNotFound {
            name: name.as_str().to_owned(),
        });
    }
    if message.to_ascii_lowercase().contains("invalid key")
        || message
            .to_ascii_lowercase()
            .contains("invalid access token")
    {
        return Err(Gw2ApiError::Unauthorized);
    }
    Err(Gw2ApiError::Upstream { status, message })
}

/// GW2 v2 errors are JSON of the form `{"text":"..."}`. Pull that out;
/// fall back to `None` if the body isn't recognisable.
fn extract_gw2_error_text(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("text").and_then(|t| t.as_str()).map(str::to_owned)
}

#[cfg(test)]
mod gw2_error_extraction_tests {
    use super::extract_gw2_error_text;

    #[test]
    fn extracts_text_from_canonical_shape() {
        assert_eq!(
            extract_gw2_error_text(r#"{"text":"no such character"}"#).as_deref(),
            Some("no such character")
        );
    }

    #[test]
    fn returns_none_when_body_is_not_json() {
        assert!(extract_gw2_error_text("plain text").is_none());
    }

    #[test]
    fn returns_none_when_no_text_field() {
        assert!(extract_gw2_error_text(r#"{"other":"x"}"#).is_none());
    }
}
