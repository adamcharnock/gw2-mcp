//! Continent / region metadata from `/v2/continents/{c}/floors/{f}/regions`.
//!
//! GW2 doesn't expose a flat `/v2/regions` endpoint, but the region
//! lookup keyed by `(continent_id, floor_id, region_id)` carries the
//! full per-region map list. We treat continent 1 = Tyria and
//! continent 2 = Mistlands, both on floor 1 (the canonical detail
//! floor) as the universe of "open-world public maps."

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One region (`Maguuma Jungle`, `Janthir Wilds`, `Cantha`, …) under a
/// continent/floor. The `maps` table is keyed by GW2 map id so callers
/// don't need an extra `/v2/maps?ids=...` join to list them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    /// `[x, y]` continent coordinate of the region label.
    #[serde(default)]
    pub label_coord: Option<[f64; 2]>,
    /// Maps in this region, keyed by GW2 map id.
    #[serde(default)]
    pub maps: BTreeMap<u32, RegionMap>,
}

/// One map (zone) inside a region — `Caledon Forest`, `Verdant Brink`,
/// `Shipwreck Strand`, etc. Trimmed to the fields a navigation-LLM
/// actually needs; the full `/v2/maps/{id}` payload is richer but
/// goes through a different endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionMap {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub min_level: u32,
    #[serde(default)]
    pub max_level: u32,
    /// Floor id this map's detail data lives on (usually `1`).
    #[serde(default)]
    pub default_floor: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_with_maps_round_trips() {
        let raw = r#"{
            "id": 4,
            "name": "Maguuma Jungle",
            "label_coord": [13784, 17091],
            "maps": {
                "873": {
                    "id": 873,
                    "name": "Caledon Forest",
                    "min_level": 1,
                    "max_level": 15,
                    "default_floor": 1
                },
                "22": {
                    "id": 22,
                    "name": "Brisban Wildlands",
                    "min_level": 15,
                    "max_level": 25,
                    "default_floor": 1
                }
            }
        }"#;
        let r: Region = serde_json::from_str(raw).unwrap();
        assert_eq!(r.id, 4);
        assert_eq!(r.name, "Maguuma Jungle");
        assert_eq!(r.maps.len(), 2);
        assert_eq!(r.maps[&873].name, "Caledon Forest");
        assert_eq!(r.maps[&22].max_level, 25);
    }

    #[test]
    fn region_with_no_label_coord_round_trips() {
        // Some regions don't have a label_coord in the API response.
        let raw = r#"{
            "id": 1,
            "name": "Shiverpeak Mountains",
            "maps": {}
        }"#;
        let r: Region = serde_json::from_str(raw).unwrap();
        assert_eq!(r.id, 1);
        assert!(r.label_coord.is_none());
        assert!(r.maps.is_empty());
    }
}
