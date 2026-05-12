//! HTTP adapter for the Guild Wars 2 v2 REST API.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use reqwest::{Client, Request, Response, StatusCode};
use serde::Deserialize;

use crate::adapters::error_body::truncate_error_body;
use crate::domain::{
    Account, AccountAchievement, AccountMastery, Achievement, AchievementId, ApiKey, CharacterName,
    Currency, CurrencyId, Dungeon, Item, ItemId, Mastery, MasteryId, Raid, Region, Skill, SkillId,
    Specialization, SpecializationId, Trait, TraitId, WalletEntry, WizardsVaultTrack,
};
use crate::ports::{Gw2Api, Gw2ApiError};

const DEFAULT_BASE_URL: &str = "https://api.guildwars2.com/v2";
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum `Retry-After` we honour with an in-process sleep+retry. Larger
/// values are surfaced to the caller as a typed error so the LLM can decide
/// whether to retry later — we never want to block the request thread for
/// minutes.
const MAX_AUTO_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Upper bound on the random jitter added to `Retry-After` to avoid every
/// MCP client waking the GW2 API at exactly the same moment.
const MAX_JITTER: Duration = Duration::from_millis(500);

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
        let req = self
            .client
            .get(&url)
            .bearer_auth(key.expose())
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;

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
                    // Cap annotations are populated by the service
                    // layer (it owns the cap table) — adapters only
                    // map wire → typed domain.
                    holding_cap: None,
                    weekly_earn_cap: None,
                    at_risk: None,
                })
            })
            .collect()
    }

    async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError> {
        let url = format!("{}/currencies", self.base_url);
        let req = self
            .client
            .get(&url)
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;
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

    async fn fetch_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, Gw2ApiError> {
        self.fetch_by_ids("items", ids, |i: Item| (i.id, i)).await
    }

    async fn fetch_achievements(
        &self,
        ids: &[AchievementId],
    ) -> Result<BTreeMap<AchievementId, Achievement>, Gw2ApiError> {
        self.fetch_by_ids("achievements", ids, |a: Achievement| (a.id, a))
            .await
    }

    async fn fetch_all_skill_ids(&self) -> Result<Vec<SkillId>, Gw2ApiError> {
        self.fetch_id_list("skills", SkillId::new).await
    }

    async fn fetch_all_trait_ids(&self) -> Result<Vec<TraitId>, Gw2ApiError> {
        self.fetch_id_list("traits", TraitId::new).await
    }

    async fn fetch_all_specialization_ids(&self) -> Result<Vec<SpecializationId>, Gw2ApiError> {
        self.fetch_id_list("specializations", SpecializationId::new)
            .await
    }

    async fn fetch_all_item_ids(&self) -> Result<Vec<ItemId>, Gw2ApiError> {
        self.fetch_id_list("items", ItemId::new).await
    }

    async fn fetch_all_achievement_ids(&self) -> Result<Vec<AchievementId>, Gw2ApiError> {
        self.fetch_id_list("achievements", AchievementId::new).await
    }

    async fn fetch_build(&self) -> Result<u32, Gw2ApiError> {
        #[derive(Deserialize)]
        struct Wire {
            id: u32,
        }
        let url = format!("{}/build", self.base_url);
        let req = self
            .client
            .get(&url)
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;
        let resp = check_status(resp).await?;
        let wire: Wire = resp
            .json()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;
        Ok(wire.id)
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

    // -------------------------------------------------------------------
    // Tier 6A — account state for PvE coaching prompts.
    // -------------------------------------------------------------------

    async fn fetch_account(&self, key: &ApiKey) -> Result<Account, Gw2ApiError> {
        let url = format!("{}/account", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_characters_list(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        let url = format!("{}/characters", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_account_achievements(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<AccountAchievement>, Gw2ApiError> {
        let url = format!("{}/account/achievements", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_account_masteries(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<AccountMastery>, Gw2ApiError> {
        let url = format!("{}/account/masteries", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_account_mastery_points(
        &self,
        key: &ApiKey,
    ) -> Result<crate::domain::AccountMasteryPoints, Gw2ApiError> {
        let url = format!("{}/account/mastery/points", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_account_bank(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<crate::domain::InventorySlot>, Gw2ApiError> {
        let url = format!("{}/account/bank", self.base_url);
        // Bank is Vec<Option<Slot>>; empty bank slots come back as
        // explicit nulls. Drop them so the service only sees occupied
        // slots — the LLM never cares about "slot 42 is empty".
        let raw: Vec<Option<crate::domain::InventorySlot>> =
            self.fetch_authed_json(&url, key, None).await?;
        Ok(raw.into_iter().flatten().collect())
    }

    async fn fetch_all_mastery_ids(&self) -> Result<Vec<MasteryId>, Gw2ApiError> {
        self.fetch_id_list("masteries", MasteryId::new).await
    }

    async fn fetch_masteries(
        &self,
        ids: &[MasteryId],
    ) -> Result<BTreeMap<MasteryId, Mastery>, Gw2ApiError> {
        self.fetch_by_ids("masteries", ids, |m: Mastery| (m.id, m))
            .await
    }

    async fn fetch_account_raids(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        let url = format!("{}/account/raids", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_all_raid_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
        let url = format!("{}/raids", self.base_url);
        self.fetch_public_json(&url).await
    }

    async fn fetch_raids(&self, ids: &[String]) -> Result<BTreeMap<String, Raid>, Gw2ApiError> {
        self.fetch_by_ids("raids", ids, |r: Raid| (r.id.clone(), r))
            .await
    }

    async fn fetch_account_dungeons(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        let url = format!("{}/account/dungeons", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_all_dungeon_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
        let url = format!("{}/dungeons", self.base_url);
        self.fetch_public_json(&url).await
    }

    async fn fetch_dungeons(
        &self,
        ids: &[String],
    ) -> Result<BTreeMap<String, Dungeon>, Gw2ApiError> {
        self.fetch_by_ids("dungeons", ids, |d: Dungeon| (d.id.clone(), d))
            .await
    }

    async fn fetch_wizards_vault_daily(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        let url = format!("{}/account/wizardsvault/daily", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_wizards_vault_weekly(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        let url = format!("{}/account/wizardsvault/weekly", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_wizards_vault_special(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        let url = format!("{}/account/wizardsvault/special", self.base_url);
        self.fetch_authed_json(&url, key, None).await
    }

    async fn fetch_regions_on_floor(
        &self,
        continent_id: u32,
        floor_id: u32,
    ) -> Result<BTreeMap<u32, Region>, Gw2ApiError> {
        // GW2 keys the response by string-form region id. Deserialise
        // into `BTreeMap<String, Region>` then convert keys to u32.
        let url = format!(
            "{}/continents/{continent_id}/floors/{floor_id}/regions",
            self.base_url
        );
        let raw: BTreeMap<String, Region> = self.fetch_public_json(&url).await?;
        let mut out = BTreeMap::new();
        for (k, v) in raw {
            match k.parse::<u32>() {
                Ok(id) => {
                    out.insert(id, v);
                }
                Err(_) => {
                    // Region keys are integers — anything else is a bug.
                    return Err(Gw2ApiError::Decode(format!(
                        "expected integer region key, got `{k}`"
                    )));
                }
            }
        }
        Ok(out)
    }
}

impl HttpGw2Api {
    /// GET an unauthenticated GW2 v2 endpoint and decode the JSON body
    /// into `T`. Mirrors `fetch_authed_json` minus the bearer header.
    /// Used by the raid / dungeon enumeration paths where the endpoint
    /// is public and the response shape is an arbitrary JSON value
    /// (not the numeric-id-list convention `fetch_id_list` handles).
    async fn fetch_public_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T, Gw2ApiError> {
        let req = self
            .client
            .get(url)
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;
        let resp = check_status(resp).await?;
        resp.json::<T>()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))
    }

    /// Generic helper for `/v2/<endpoint>` with no `?ids=` parameter — the
    /// GW2 v2 convention is that omitting `ids` returns the full id list as
    /// a JSON array of integers. Used by the Tier-6C indexer to enumerate
    /// every entity before chunked fetching.
    async fn fetch_id_list<Id, F>(&self, endpoint: &str, ctor: F) -> Result<Vec<Id>, Gw2ApiError>
    where
        F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
    {
        let url = format!("{}/{endpoint}", self.base_url);
        let req = self
            .client
            .get(&url)
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;
        let resp = check_status(resp).await?;
        let raw: Vec<i64> = resp
            .json()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;
        raw.into_iter()
            .map(|id| ctor(id).map_err(|e| Gw2ApiError::Decode(format!("{endpoint} id {id}: {e}"))))
            .collect()
    }

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
            let req = self
                .client
                .get(&url)
                .build()
                .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
            let resp = self.send_request(req).await?;
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
        let req = self
            .client
            .get(url)
            .bearer_auth(key.expose())
            .build()
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        let resp = self.send_request(req).await?;
        let resp = check_status_with_context(resp, character).await?;
        resp.json::<T>()
            .await
            .map_err(|e| Gw2ApiError::Decode(e.to_string()))
    }

    /// Send a request with rate-limit retry: on 429 with a parseable
    /// `Retry-After ≤ 60s`, sleep `retry_after + jitter(0..500ms)` once
    /// and re-issue. After the single retry, the response is returned
    /// as-is for `check_status_with_context` to translate into a typed
    /// error (or success).
    ///
    /// Other status codes (including 401/403) flow through unchanged
    /// — only 429 short-circuits here, because rate limits are the one
    /// case where automatic backoff is unambiguously the right move.
    async fn send_request(&self, request: Request) -> Result<Response, Gw2ApiError> {
        // Clone first so the retry path still has a usable request — the
        // original is consumed by `client.execute`.
        let retry_request = request.try_clone();
        let resp = self
            .client
            .execute(request)
            .await
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
        if resp.status() != StatusCode::TOO_MANY_REQUESTS {
            return Ok(resp);
        }

        // 429 path. Honour Retry-After only when small; bail otherwise.
        let Some(retry_after) = parse_retry_after(resp.headers(), SystemTime::now()) else {
            // No Retry-After header at all — return as-is.
            return Ok(resp);
        };
        if retry_after > MAX_AUTO_RETRY_DELAY {
            // Server asked us to wait too long — surface to the caller.
            return Ok(resp);
        }
        let Some(retry_request) = retry_request else {
            // Non-clonable body (e.g. streamed). Can't retry; return.
            return Ok(resp);
        };

        // MAX_JITTER is 500ms — fits comfortably in u64.
        let jitter_ms_max = u64::try_from(MAX_JITTER.as_millis()).unwrap_or(500);
        let jitter = Duration::from_millis(rand::random::<u64>() % jitter_ms_max.max(1));
        let delay = retry_after.min(MAX_AUTO_RETRY_DELAY) + jitter;
        tracing::debug!(?delay, "429 received; sleeping then retrying once");
        tokio::time::sleep(delay).await;

        self.client
            .execute(retry_request)
            .await
            .map_err(|e| Gw2ApiError::Transport(e.to_string()))
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
        // `send_request` already attempted one retry for short delays;
        // we land here when the upstream is still throttling. Carry the
        // Retry-After (if any) forward so the caller can quote a precise
        // backoff to the user.
        let retry_after = parse_retry_after(resp.headers(), SystemTime::now());
        return Err(Gw2ApiError::RateLimited(retry_after));
    }
    // 401 vs 403 split. 403 with "requires scope"/"insufficient scope"
    // means the key is structurally fine but lacks a scope; we extract
    // the scope name so the LLM can tell the user exactly which checkbox
    // to tick. Bare 401 (or 403 without that signal) means the key is
    // bad — fall back to Unauthorized.
    if status == 401 {
        // Drain body so it doesn't leak — we don't need it.
        let _ = resp.text().await;
        return Err(Gw2ApiError::Unauthorized);
    }
    if status == 403 {
        let body = resp.text().await.unwrap_or_default();
        let message = extract_gw2_error_text(&body).unwrap_or_else(|| body.clone());
        if let Some(scope) = parse_missing_scope(&message) {
            return Err(Gw2ApiError::MissingScope { needed: scope });
        }
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
    // Cap the message before bubbling it up — GW2 occasionally returns
    // an HTML maintenance page or a multi-kilobyte stack trace, and the
    // raw body lands in the LLM context window otherwise.
    Err(Gw2ApiError::Upstream {
        status,
        message: truncate_error_body(&message),
    })
}

/// Parse `Retry-After`. Accepts both seconds (e.g. `120`) and HTTP-date
/// (`Wed, 21 Oct 2025 07:28:00 GMT`) forms — the spec allows either.
/// Returns `None` if the header is absent or unparseable.
fn parse_retry_after(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = v.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(date) = httpdate::parse_http_date(v.trim()) {
        return date.duration_since(now).ok();
    }
    None
}

/// Pull a missing scope name out of GW2's "requires scope X" body.
/// The match is case-insensitive on the prefix; the scope token itself
/// is the next punctuation-bounded word, lower-cased. Returns `None` if
/// the body doesn't carry a scope hint at all (caller falls back to
/// generic `Unauthorized`).
fn parse_missing_scope(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    for needle in ["requires scope", "insufficient scope"] {
        if let Some(idx) = lower.find(needle) {
            let tail = &lower[idx + needle.len()..];
            let scope = tail
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .find(|s| !s.is_empty());
            return Some(scope.map_or_else(|| "unknown".to_owned(), str::to_owned));
        }
    }
    None
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

#[cfg(test)]
mod missing_scope_tests {
    use super::parse_missing_scope;

    #[test]
    fn extracts_lowercase_scope() {
        assert_eq!(
            parse_missing_scope("requires scope wallet").as_deref(),
            Some("wallet")
        );
    }

    #[test]
    fn extracts_with_colon_separator() {
        assert_eq!(
            parse_missing_scope("requires scope: characters").as_deref(),
            Some("characters")
        );
    }

    #[test]
    fn handles_insufficient_scope_phrasing() {
        assert_eq!(
            parse_missing_scope("Insufficient scope (builds)").as_deref(),
            Some("builds")
        );
    }

    #[test]
    fn returns_unknown_when_no_scope_token() {
        assert_eq!(
            parse_missing_scope("requires scope").as_deref(),
            Some("unknown")
        );
    }

    #[test]
    fn returns_none_when_no_phrase_found() {
        assert!(parse_missing_scope("invalid key").is_none());
    }

    #[test]
    fn case_insensitive_match() {
        assert_eq!(
            parse_missing_scope("REQUIRES SCOPE wallet").as_deref(),
            Some("wallet")
        );
    }
}

#[cfg(test)]
mod retry_after_tests {
    use super::parse_retry_after;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::{Duration, SystemTime};

    #[test]
    fn parses_seconds_form() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(
            parse_retry_after(&h, SystemTime::now()),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn parses_http_date_form() {
        let mut h = HeaderMap::new();
        // 60 seconds in the future from a fixed `now`. Use httpdate to format.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let future = now + Duration::from_secs(60);
        let formatted = httpdate::fmt_http_date(future);
        h.insert(RETRY_AFTER, HeaderValue::from_str(&formatted).unwrap());
        let got = parse_retry_after(&h, now).unwrap();
        // Allow a 1s slop because http_date drops sub-second precision.
        assert!(got <= Duration::from_secs(60));
        assert!(got >= Duration::from_secs(59));
    }

    #[test]
    fn returns_none_for_garbage() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("not a number"));
        assert!(parse_retry_after(&h, SystemTime::now()).is_none());
    }

    #[test]
    fn returns_none_when_header_absent() {
        let h = HeaderMap::new();
        assert!(parse_retry_after(&h, SystemTime::now()).is_none());
    }
}
