//! HTTP adapter for `/v2/maps/{id}` and `/v2/continents/.../maps/{id}`.
//!
//! Map data only changes on patch days, so the [`Service`](crate::service)
//! caches everything we return at `STATIC_TTL`. This adapter is dumb:
//! it issues the two HTTP calls, normalises the POI shape, and bails.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::adapters::error_body::truncate_error_body;
use crate::ports::{MapData, MapDataError, MapId, MapInfo, MapPoi};

const DEFAULT_BASE_URL: &str = "https://api.guildwars2.com/v2";
const USER_AGENT: &str = concat!(
    "gw2-mcp/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamcharnock/gw2-mcp)"
);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpMapData {
    client: Client,
    base_url: String,
}

impl HttpMapData {
    pub fn new() -> Result<Self, MapDataError> {
        Self::with_base_url(DEFAULT_BASE_URL.to_owned())
    }

    pub fn with_base_url(base_url: String) -> Result<Self, MapDataError> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| MapDataError::Transport(e.to_string()))?;
        Ok(Self { client, base_url })
    }
}

#[async_trait]
impl MapData for HttpMapData {
    async fn get_map(&self, id: MapId) -> Result<MapInfo, MapDataError> {
        #[derive(Deserialize)]
        struct Wire {
            id: u32,
            name: String,
            #[serde(default, rename = "type")]
            map_type: Option<String>,
            #[serde(default)]
            min_level: Option<u32>,
            #[serde(default)]
            max_level: Option<u32>,
            #[serde(default)]
            default_floor: Option<i32>,
            #[serde(default)]
            region_id: Option<u32>,
            #[serde(default)]
            region_name: Option<String>,
            #[serde(default)]
            continent_id: Option<u32>,
            #[serde(default)]
            continent_name: Option<String>,
            #[serde(default)]
            continent_rect: Option<[[f64; 2]; 2]>,
            #[serde(default)]
            map_rect: Option<[[f64; 2]; 2]>,
        }

        let url = format!("{}/maps/{id}", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| MapDataError::Transport(e.to_string()))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(MapDataError::NotFound(id));
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MapDataError::Status {
                status,
                body: truncate_error_body(&body),
            });
        }
        let w: Wire = resp
            .json()
            .await
            .map_err(|e| MapDataError::Decode(e.to_string()))?;
        Ok(MapInfo {
            id: w.id,
            name: w.name,
            map_type: w.map_type,
            min_level: w.min_level,
            max_level: w.max_level,
            default_floor: w.default_floor.unwrap_or(1),
            region_id: w.region_id.unwrap_or(0),
            region_name: w.region_name.unwrap_or_default(),
            continent_id: w.continent_id.unwrap_or(0),
            continent_name: w.continent_name.unwrap_or_default(),
            continent_rect: w.continent_rect.unwrap_or([[0.0, 0.0], [0.0, 0.0]]),
            map_rect: w.map_rect.unwrap_or([[0.0, 0.0], [0.0, 0.0]]),
        })
    }

    async fn list_pois(&self, map_id: MapId) -> Result<Vec<MapPoi>, MapDataError> {
        // Need the map's continent + region + default_floor first; the
        // `/v2/maps/{id}` payload carries them.
        let info = self.get_map(map_id).await?;

        let url = format!(
            "{}/continents/{}/floors/{}/regions/{}/maps/{}",
            self.base_url, info.continent_id, info.default_floor, info.region_id, map_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| MapDataError::Transport(e.to_string()))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(MapDataError::NotFound(map_id));
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MapDataError::Status {
                status,
                body: truncate_error_body(&body),
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MapDataError::Decode(e.to_string()))?;
        Ok(flatten_pois(&v, info.default_floor))
    }
}

/// Walk the per-map detail object and pull every POI / task / skill
/// challenge into a flat list. Robust to missing arrays (tasks-free
/// maps, no-poi instances) — those just yield empty contributions.
fn flatten_pois(v: &serde_json::Value, floor: i32) -> Vec<MapPoi> {
    let mut out = Vec::new();
    if let Some(obj) = v.get("points_of_interest").and_then(|v| v.as_object()) {
        for (_id_key, poi) in obj {
            if let Some(p) = parse_poi_object(poi, floor) {
                out.push(p);
            }
        }
    }
    if let Some(obj) = v.get("tasks").and_then(|v| v.as_object()) {
        for (_id_key, t) in obj {
            if let Some(p) = parse_task_object(t, floor) {
                out.push(p);
            }
        }
    }
    if let Some(arr) = v.get("skill_challenges").and_then(|v| v.as_array()) {
        for sc in arr {
            if let Some(p) = parse_skill_challenge(sc, floor) {
                out.push(p);
            }
        }
    }
    out
}

fn parse_poi_object(v: &serde_json::Value, floor: i32) -> Option<MapPoi> {
    let id = v.get("id").and_then(serde_json::Value::as_u64)?;
    let name = v
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("(unnamed)")
        .to_owned();
    // Map "waypoint" → "waypoint", "landmark" → "landmark", etc. GW2 also
    // emits "vista" and "unlock" via the same `type` field; pass through.
    let kind = v
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("landmark")
        .to_owned();
    let coord = parse_coord(v.get("coord"))?;
    let chat_link = v
        .get("chat_link")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Some(MapPoi {
        id,
        name,
        kind,
        coord,
        chat_link,
        floor,
    })
}

fn parse_task_object(v: &serde_json::Value, floor: i32) -> Option<MapPoi> {
    let id = v.get("id").and_then(serde_json::Value::as_u64)?;
    let name = v
        .get("objective")
        .and_then(|v| v.as_str())
        .unwrap_or("(renown heart)")
        .to_owned();
    let coord = parse_coord(v.get("coord"))?;
    let chat_link = v
        .get("chat_link")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Some(MapPoi {
        id,
        name,
        kind: "task".to_owned(),
        coord,
        chat_link,
        floor,
    })
}

fn parse_skill_challenge(v: &serde_json::Value, floor: i32) -> Option<MapPoi> {
    // Skill challenges (hero points) ship with an `id` like "0-1234"
    // (numeric component matters, prefix is the expansion). We parse
    // out the trailing integer; if anything's odd, just hash the string.
    let raw_id = v.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id = raw_id
        .rsplit_once('-')
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .or_else(|| raw_id.parse::<u64>().ok())
        .unwrap_or(0);
    let coord = parse_coord(v.get("coord"))?;
    Some(MapPoi {
        id,
        name: format!("Hero Point {raw_id}"),
        kind: "hero_point".to_owned(),
        coord,
        chat_link: None,
        floor,
    })
}

fn parse_coord(v: Option<&serde_json::Value>) -> Option<(f64, f64)> {
    let arr = v?.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    Some((arr[0].as_f64()?, arr[1].as_f64()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_handles_empty_payload() {
        let v = serde_json::json!({});
        assert!(flatten_pois(&v, 1).is_empty());
    }

    #[test]
    fn flatten_extracts_pois_tasks_and_hero_points() {
        let v = serde_json::json!({
            "points_of_interest": {
                "118": {
                    "name": "Shaemoor Garrison Waypoint",
                    "type": "waypoint",
                    "coord": [10000.0, 12000.0],
                    "id": 118,
                    "chat_link": "[&BHcAAAA=]"
                },
                "555": {
                    "name": "Vista A",
                    "type": "vista",
                    "coord": [11000.0, 13000.0],
                    "id": 555
                }
            },
            "tasks": {
                "1": {
                    "id": 1,
                    "objective": "Help the farmers",
                    "coord": [9000.0, 8000.0],
                    "chat_link": "[&BAEAAAA=]"
                }
            },
            "skill_challenges": [
                { "id": "0-77", "coord": [12345.0, 6789.0] }
            ]
        });
        let mut pois = flatten_pois(&v, 1);
        pois.sort_by_key(|p| p.id);
        assert_eq!(pois.len(), 4);
        assert!(pois.iter().any(|p| p.kind == "waypoint" && p.id == 118));
        assert!(pois.iter().any(|p| p.kind == "vista" && p.id == 555));
        assert!(pois.iter().any(|p| p.kind == "task" && p.id == 1));
        assert!(pois.iter().any(|p| p.kind == "hero_point" && p.id == 77));
    }

    #[test]
    fn flatten_skips_pois_with_missing_coord() {
        let v = serde_json::json!({
            "points_of_interest": {
                "1": { "id": 1, "name": "broken", "type": "waypoint" }
            }
        });
        assert!(flatten_pois(&v, 1).is_empty());
    }
}
