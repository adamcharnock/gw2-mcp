//! Domain types for GW2 instanced `PvE` content — raids and dungeons —
//! mirroring the canonical `/v2/raids` and `/v2/dungeons` shapes.
//!
//! The endpoints return id-only strings for encounters / paths (e.g.
//! `"vale_guardian"`, `"ascalon_catacombs_path_1"`). The
//! [`title_case`] helper in [`super::snake_case`] turns those into
//! human-readable names without us having to hand-maintain a lookup
//! table — every new raid wing `ArenaNet` ships works automatically.

use serde::{Deserialize, Serialize};

/// One raid release (e.g. `forsaken_thicket`). Holds the wing-by-wing
/// breakdown the API returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Raid {
    pub id: String,
    #[serde(default)]
    pub wings: Vec<RaidWing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaidWing {
    pub id: String,
    #[serde(default)]
    pub events: Vec<RaidEvent>,
}

/// A boss or checkpoint within a wing. The API's `type` field maps to
/// our `kind` so we can keep `type` available as a method name in Rust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaidEvent {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dungeon {
    pub id: String,
    #[serde(default)]
    pub paths: Vec<DungeonPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DungeonPath {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raid_round_trips() {
        let raw = r#"{
            "id": "forsaken_thicket",
            "wings": [
                {
                    "id": "spirit_vale",
                    "events": [
                        {"id": "vale_guardian", "type": "Boss"},
                        {"id": "spirit_woods", "type": "Checkpoint"},
                        {"id": "gorseval_the_multifarious", "type": "Boss"}
                    ]
                }
            ]
        }"#;
        let r: Raid = serde_json::from_str(raw).unwrap();
        assert_eq!(r.id, "forsaken_thicket");
        assert_eq!(r.wings.len(), 1);
        assert_eq!(r.wings[0].events.len(), 3);
        assert_eq!(r.wings[0].events[0].kind, "Boss");
    }

    #[test]
    fn dungeon_round_trips() {
        let raw = r#"{
            "id": "ascalon_catacombs",
            "paths": [
                {"id": "ascalon_catacombs_story", "type": "Story"},
                {"id": "ascalon_catacombs_path_1", "type": "Explorable"}
            ]
        }"#;
        let d: Dungeon = serde_json::from_str(raw).unwrap();
        assert_eq!(d.id, "ascalon_catacombs");
        assert_eq!(d.paths.len(), 2);
        assert_eq!(d.paths[0].kind, "Story");
        assert_eq!(d.paths[1].kind, "Explorable");
    }
}
