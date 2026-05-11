//! Map / region navigation queries that go beyond the live `find_nearby`
//! POI walk in [`navigation`](super::navigation).
//!
//! Today this is just `list_maps_in_region`. The data backing it is
//! the GW2 continents / floors / regions tree (`/v2/continents/{c}/
//! floors/{f}/regions`), which we walk once across continent 1
//! (Tyria) and continent 2 (Mistlands) on floor 1 (the canonical
//! detail floor), build an in-memory index keyed by both region id
//! and lowercased name, and cache under `STATIC_TTL`.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use super::{STATIC_TTL, Service, ServiceError};
use crate::domain::{MapNeighborLink, MapNeighbors, MapNeighborsError, Region};

/// How the caller asked us to find a region — by GW2 numeric id or by
/// (case-insensitive) name.
#[derive(Debug, Clone)]
pub enum RegionQuery {
    Id(u32),
    Name(String),
}

/// Listing response for `list_maps_in_region`. Wrapped in an object
/// so MCP's `structuredContent` schema accepts it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionMapList {
    pub region_id: u32,
    pub region_name: String,
    pub continent_id: u32,
    pub maps: Vec<RegionMapEntry>,
    pub total: usize,
}

/// One map (zone) inside the region. Slimmer than
/// [`crate::domain::RegionMap`] — strips fields the LLM doesn't need
/// for the "what maps live here?" answer.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionMapEntry {
    pub map_id: u32,
    pub name: String,
    pub min_level: u32,
    pub max_level: u32,
}

/// Errors specific to the region-lookup path.
#[derive(Debug, Error)]
pub enum RegionLookupError {
    #[error(
        "no region named `{name}` found in the GW2 continents catalogue. Try one of: {available}"
    )]
    NotFound { name: String, available: String },

    #[error("multiple regions match `{name}`: {matches}. Disambiguate by id.")]
    Ambiguous { name: String, matches: String },

    #[error("no region with id {id} in the GW2 continents catalogue")]
    UnknownId { id: u32 },

    /// The GW2 `/v2/continents/.../regions` endpoints failed for every
    /// continent we tried, so we couldn't build the lookup index at all.
    /// Distinct from `NotFound` so the LLM doesn't mistake an upstream
    /// outage for a typo'd region name.
    #[error(
        "the Guild Wars 2 continents API is unreachable, so list_maps_in_region can't build its \
         region index. This is usually transient — retry in a few seconds."
    )]
    UpstreamUnavailable,

    #[error(
        "no adjacency data for map id {map_id}. The curated table only covers public open-world \
         maps; instances, fractals, and WvW maps that don't appear in the wiki Category:Zones \
         page aren't included."
    )]
    NoNeighborData { map_id: u32 },

    #[error("failed to load curated map-neighbor table: {0}")]
    NeighborsLoad(MapNeighborsError),
}

/// Response shape for `get_map_neighbors`. Wrapped in an object so
/// MCP's `structuredContent` schema accepts it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborsResponse {
    pub map_id: u32,
    pub map_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    pub neighbors: Vec<MapNeighborLink>,
    pub total: usize,
}

/// Continents + floors we walk to enumerate regions. Hardcoded
/// because GW2 hasn't added a continent since launch — the worst case
/// when a future expansion ships is a missing region, which is the
/// same behaviour as before this code existed.
const CONTINENT_FLOOR_PAIRS: &[(u32, u32)] = &[(1, 1), (2, 1)];

const CACHE_KEY: &str = "regions:index";

/// In-memory index keyed by region id. The continent id is carried
/// per entry so the response can echo where a region lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegionIndex {
    entries: Vec<RegionIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegionIndexEntry {
    continent_id: u32,
    region: Region,
}

/// Parsed once per process. The YAML is identical for every Service
/// instance and the parse is pure CPU — no need to put it behind the
/// async `Cache` port.
static NEIGHBORS_TABLE: std::sync::OnceLock<Result<MapNeighbors, MapNeighborsError>> =
    std::sync::OnceLock::new();

fn neighbors_table() -> Result<&'static MapNeighbors, ServiceError> {
    NEIGHBORS_TABLE
        .get_or_init(MapNeighbors::load_embedded)
        .as_ref()
        .map_err(|e| ServiceError::Region(RegionLookupError::NeighborsLoad(e.clone())))
}

impl Service {
    /// Resolve a map's adjacent maps from the curated YAML table.
    ///
    /// Returns the named neighbors with their map ids, names, and
    /// (optional) compass direction labels lifted from the GW2 wiki.
    /// Maps that aren't in the curated table (instances, fractals,
    /// some `WvW` edges) produce a [`RegionLookupError::NoNeighborData`].
    pub fn get_map_neighbors(&self, map_id: u32) -> Result<MapNeighborsResponse, ServiceError> {
        let table = neighbors_table()?;
        let entry = table.get(map_id).cloned().ok_or(ServiceError::Region(
            RegionLookupError::NoNeighborData { map_id },
        ))?;
        Ok(MapNeighborsResponse {
            map_id,
            map_name: entry.name,
            region_name: entry.region_name,
            total: entry.neighbors.len(),
            neighbors: entry.neighbors,
        })
    }

    /// Resolve a region by id or name and return its map list.
    pub async fn list_maps_in_region(
        &self,
        query: RegionQuery,
    ) -> Result<RegionMapList, ServiceError> {
        let index = self.region_index().await?;
        let entry = match query {
            RegionQuery::Id(id) => index
                .entries
                .into_iter()
                .find(|e| e.region.id == id)
                .ok_or(ServiceError::Region(RegionLookupError::UnknownId { id }))?,
            RegionQuery::Name(name) => resolve_by_name(index, &name)?,
        };
        let mut maps: Vec<RegionMapEntry> = entry
            .region
            .maps
            .into_values()
            .map(|m| RegionMapEntry {
                map_id: m.id,
                name: m.name,
                min_level: m.min_level,
                max_level: m.max_level,
            })
            .collect();
        // Sort by min_level so a player exploring the region sees the
        // natural progression.
        maps.sort_by(|a, b| {
            a.min_level
                .cmp(&b.min_level)
                .then_with(|| a.map_id.cmp(&b.map_id))
        });
        Ok(RegionMapList {
            region_id: entry.region.id,
            region_name: entry.region.name,
            continent_id: entry.continent_id,
            total: maps.len(),
            maps,
        })
    }

    /// Build or fetch the cached region index. Best-effort: a fetch
    /// failure for one continent doesn't poison the whole index, but
    /// we do surface a hard failure if BOTH continents fail to load
    /// (otherwise we'd silently return "no such region" for any query).
    async fn region_index(&self) -> Result<RegionIndex, ServiceError> {
        if let Some(json) = self.cache.get(CACHE_KEY).await
            && let Ok(idx) = serde_json::from_str::<RegionIndex>(&json)
        {
            return Ok(idx);
        }
        let mut entries = Vec::new();
        let mut any_ok = false;
        for &(continent_id, floor_id) in CONTINENT_FLOOR_PAIRS {
            match self
                .gw2
                .fetch_regions_on_floor(continent_id, floor_id)
                .await
            {
                Ok(map) => {
                    any_ok = true;
                    for (_id, region) in map {
                        entries.push(RegionIndexEntry {
                            continent_id,
                            region,
                        });
                    }
                }
                Err(e) => {
                    warn!(
                        continent_id,
                        floor_id,
                        error = ?e,
                        "failed to enumerate regions on continent floor; partial index"
                    );
                }
            }
        }
        if !any_ok {
            return Err(ServiceError::Region(RegionLookupError::UpstreamUnavailable));
        }
        let index = RegionIndex { entries };
        if let Ok(json) = serde_json::to_string(&index) {
            self.cache.set(CACHE_KEY, json, STATIC_TTL).await;
        }
        Ok(index)
    }
}

fn resolve_by_name(index: RegionIndex, name: &str) -> Result<RegionIndexEntry, ServiceError> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return Err(ServiceError::Region(RegionLookupError::NotFound {
            name: name.to_owned(),
            available: "(empty query)".to_owned(),
        }));
    }
    let matches: Vec<RegionIndexEntry> = index
        .entries
        .iter()
        .filter(|e| e.region.name.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    match matches.len() {
        0 => {
            let mut available: Vec<String> = index
                .entries
                .iter()
                .map(|e| e.region.name.clone())
                .filter(|n| !n.is_empty())
                .collect();
            available.sort();
            available.dedup();
            Err(ServiceError::Region(RegionLookupError::NotFound {
                name: name.to_owned(),
                available: available.join(", "),
            }))
        }
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        _ => {
            let mut names: Vec<String> = matches.iter().map(|e| e.region.name.clone()).collect();
            names.sort();
            Err(ServiceError::Region(RegionLookupError::Ambiguous {
                name: name.to_owned(),
                matches: names.join(", "),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RegionMap;
    use std::collections::BTreeMap;

    fn region(id: u32, name: &str, maps: Vec<RegionMap>) -> Region {
        let mut map = BTreeMap::new();
        for m in maps {
            map.insert(m.id, m);
        }
        Region {
            id,
            name: name.to_owned(),
            label_coord: None,
            maps: map,
        }
    }

    fn map_entry(id: u32, name: &str, min: u32, max: u32) -> RegionMap {
        RegionMap {
            id,
            name: name.to_owned(),
            min_level: min,
            max_level: max,
            default_floor: 1,
        }
    }

    fn index_with(entries: Vec<(u32, Region)>) -> RegionIndex {
        RegionIndex {
            entries: entries
                .into_iter()
                .map(|(c, r)| RegionIndexEntry {
                    continent_id: c,
                    region: r,
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_by_name_exact_match() {
        let idx = index_with(vec![(
            1,
            region(
                4,
                "Maguuma Jungle",
                vec![map_entry(873, "Caledon Forest", 1, 15)],
            ),
        )]);
        let e = resolve_by_name(idx, "Maguuma Jungle").unwrap();
        assert_eq!(e.region.id, 4);
    }

    #[test]
    fn resolve_by_name_is_case_insensitive() {
        let idx = index_with(vec![(1, region(4, "Maguuma Jungle", vec![]))]);
        let e = resolve_by_name(idx, "maguuma jungle").unwrap();
        assert_eq!(e.region.id, 4);
    }

    #[test]
    fn resolve_by_name_substring() {
        let idx = index_with(vec![(1, region(4, "Maguuma Jungle", vec![]))]);
        let e = resolve_by_name(idx, "maguuma").unwrap();
        assert_eq!(e.region.id, 4);
    }

    #[test]
    fn resolve_by_name_ambiguous_lists_matches() {
        let idx = index_with(vec![
            (1, region(1, "Shiverpeak Mountains", vec![])),
            (1, region(2, "Heart of Shiverpeaks", vec![])),
        ]);
        let err = resolve_by_name(idx, "shiver").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Shiverpeak Mountains"), "got: {msg}");
        assert!(msg.contains("Heart of Shiverpeaks"), "got: {msg}");
    }

    #[test]
    fn resolve_by_name_not_found_lists_available() {
        let idx = index_with(vec![(1, region(4, "Maguuma Jungle", vec![]))]);
        let err = resolve_by_name(idx, "Atlantis").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Maguuma Jungle"),
            "must list available regions: {msg}"
        );
    }
}
