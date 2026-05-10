//! HTTP adapter for the Guild Wars 2 v2 REST API.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::domain::{ApiKey, Currency, CurrencyId, WalletEntry};
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
        // GW2 API caps `ids` parameter at 200 per request; chunk to be safe.
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
            let url = format!("{}/currencies?ids={}", self.base_url, ids_param);
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| Gw2ApiError::Transport(e.to_string()))?;
            let resp = check_status(resp).await?;
            let currencies: Vec<Currency> = resp
                .json()
                .await
                .map_err(|e| Gw2ApiError::Decode(e.to_string()))?;
            for c in currencies {
                out.insert(c.id, c);
            }
        }

        Ok(out)
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, Gw2ApiError> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(Gw2ApiError::Status { status, body })
    }
}
