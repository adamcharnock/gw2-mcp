//! Curated map-adjacency table embedded at compile time.
//!
//! The GW2 API exposes per-map metadata but not portal / adjacency
//! data. We ship `data/map_neighbors.yaml` (built by the one-shot
//! `scrape-map-neighbors` cargo bin from the GW2 wiki) so the LLM can
//! answer "what maps border this one?" in one call.
//!
//! The YAML is `include_str!`-embedded — no runtime file I/O, no
//! missing-file failure mode. Loader runs once at startup and the
//! parsed map lives behind an `Arc` in the [`Service`](crate::service).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Embedded YAML produced by `tools/scrape_map_neighbors`. Keep in
/// sync by rerunning the scraper when `ArenaNet` ships a new expansion.
const EMBEDDED_YAML: &str = include_str!("../../data/map_neighbors.yaml");

/// One source map's neighbors. The source-side metadata (name,
/// region) is owned by the loader; per-entry data lives in
/// [`MapNeighborLink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    pub neighbors: Vec<MapNeighborLink>,
}

/// One adjacency edge. `direction` is the compass label from the
/// wiki infobox (`NE`, `SW, S`, `SSW`, …) — opaque to us, surfaced
/// verbatim because the LLM can spell it out for the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborLink {
    pub map_id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
}

/// In-memory adjacency lookup. Cheap to clone — interior is a sorted
/// map keyed by source map id.
#[derive(Debug, Clone)]
pub struct MapNeighbors {
    by_id: BTreeMap<u32, MapNeighborEntry>,
}

#[derive(Debug, Clone, Error)]
pub enum MapNeighborsError {
    #[error("failed to parse embedded map_neighbors.yaml: {0}")]
    Parse(String),
}

impl MapNeighbors {
    /// Parse the embedded YAML. Compile-time guarantee that the file
    /// exists; runtime check that it round-trips into our shape.
    pub fn load_embedded() -> Result<Self, MapNeighborsError> {
        Self::from_yaml(EMBEDDED_YAML)
    }

    /// Parse a YAML string with the same shape as
    /// `data/map_neighbors.yaml`. Public so tests can build small
    /// fixtures.
    pub fn from_yaml(yaml: &str) -> Result<Self, MapNeighborsError> {
        let by_id: BTreeMap<u32, MapNeighborEntry> =
            serde_yaml_bw::from_str(yaml).map_err(|e| MapNeighborsError::Parse(e.to_string()))?;
        Ok(Self { by_id })
    }

    /// Look up adjacency for a map id. Returns `None` for maps that
    /// aren't in the table (instances, fractals, some `WvW` edge cases).
    #[must_use]
    pub fn get(&self, map_id: u32) -> Option<&MapNeighborEntry> {
        self.by_id.get(&map_id)
    }

    /// Total number of source maps covered. Useful for sanity tests
    /// and the `get_index_status` rollup.
    #[must_use]
    pub fn total(&self) -> usize {
        self.by_id.len()
    }

    /// Iterate every `(source_map_id, entry)` pair. Used by the
    /// integrity test that asserts every map id resolves against a
    /// frozen `/v2/maps?ids=all` snapshot.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &MapNeighborEntry)> {
        self.by_id.iter().map(|(k, v)| (*k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_yaml_parses_cleanly() {
        let nbrs = MapNeighbors::load_embedded().expect("embedded YAML must parse");
        assert!(
            nbrs.total() > 50,
            "embedded YAML should cover most open-world maps, got {}",
            nbrs.total()
        );
    }

    #[test]
    fn caledon_forest_neighbors_match_wiki_anchors() {
        // Hand-verified anchor (Caledon Forest, map id 34):
        // Brisban Wildlands (NW), Kessex Hills (NE), Metrica Province (W),
        // The Grove (S).
        let nbrs = MapNeighbors::load_embedded().expect("parse");
        let cf = nbrs.get(34).expect("Caledon Forest must be in YAML");
        assert_eq!(cf.name, "Caledon Forest");
        let names: Vec<&str> = cf.neighbors.iter().map(|n| n.name.as_str()).collect();
        for expected in [
            "Brisban Wildlands",
            "Kessex Hills",
            "Metrica Province",
            "The Grove",
        ] {
            assert!(
                names.contains(&expected),
                "Caledon Forest missing expected neighbor {expected}; got {names:?}"
            );
        }
    }

    #[test]
    fn shipwreck_strand_links_to_lions_arch_via_gate() {
        // Janthir Wilds anchor: Shipwreck Strand (1595) connects to
        // Lion's Arch (no direction — asura gate) and Starlit Weald (W).
        let nbrs = MapNeighbors::load_embedded().expect("parse");
        let ss = nbrs.get(1595).expect("Shipwreck Strand must be present");
        let names: Vec<&str> = ss.neighbors.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"Lion's Arch") && names.contains(&"Starlit Weald"),
            "Shipwreck Strand missing expected neighbors; got {names:?}"
        );
    }

    #[test]
    fn from_yaml_round_trips_minimal_fixture() {
        let raw = "
34:
  name: Caledon Forest
  region_name: Maguuma Jungle
  neighbors:
    - map_id: 22
      name: Brisban Wildlands
      direction: NW
";
        let nbrs = MapNeighbors::from_yaml(raw).expect("parse");
        assert_eq!(nbrs.total(), 1);
        let cf = nbrs.get(34).unwrap();
        assert_eq!(cf.neighbors[0].direction.as_deref(), Some("NW"));
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let nbrs = MapNeighbors::load_embedded().expect("parse");
        // 999999 is well out of GW2's id range.
        assert!(nbrs.get(999_999).is_none());
    }
}
