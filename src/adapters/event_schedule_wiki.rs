//! HTTP adapter for the wiki "Event timer" widget data.
//!
//! Hits `https://wiki.guildwars2.com/index.php?title=Widget:Event_timer/data.json&action=raw`.
//! The bare `/wiki/Widget:.../data.json` URL renders the wiki chrome
//! (HTML); only `action=raw` returns the JSON body. Verified live
//! against widget version `v5.1`.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::adapters::error_body::truncate_error_body;
use crate::domain::EventScheduleRaw;
use crate::ports::{EventSchedule, EventScheduleError};

pub const DEFAULT_EVENT_SCHEDULE_URL: &str =
    "https://wiki.guildwars2.com/index.php?title=Widget:Event_timer/data.json&action=raw";

const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpEventSchedule {
    client: Client,
    url: String,
}

impl HttpEventSchedule {
    pub fn new() -> Result<Self, EventScheduleError> {
        Self::with_url(DEFAULT_EVENT_SCHEDULE_URL.to_owned())
    }

    pub fn with_url(url: String) -> Result<Self, EventScheduleError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| EventScheduleError::Transport(e.to_string()))?;
        Ok(Self { client, url })
    }
}

#[async_trait]
impl EventSchedule for HttpEventSchedule {
    async fn fetch_raw(&self) -> Result<EventScheduleRaw, EventScheduleError> {
        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| EventScheduleError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(EventScheduleError::Status {
                status,
                body: truncate_error_body(&body),
            });
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EventScheduleError::Decode(e.to_string()))?;
        EventScheduleRaw::from_json_value(value).map_err(EventScheduleError::Decode)
    }
}
