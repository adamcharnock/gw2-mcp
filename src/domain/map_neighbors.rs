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

/// Kind of connection between two adjacent maps. Distinguishes "walk
/// through" from "step through a portal" so the LLM can give accurate
/// travel directions ("ride east", "use the asura gate at <X>", "you
/// need to finish story step Y first").
///
/// Defaults to [`Self::Physical`] when missing from the YAML — that's
/// the most common case and the safest assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    /// Walk or mount across the physical map border. Most open-world
    /// edges are this.
    Physical,
    /// Asura gate (or equivalent magical portal). One-shot teleport
    /// in/out of a hub area — Lion's Arch / Eye of the North /
    /// Arborstone / Aerodrome etc.
    AsuraGate,
    /// Map entrance gated behind a specific story / mastery step
    /// (e.g. Skywatch Archipelago via the Eye of the North `SotO` portal,
    /// or Wizard's Tower after the `SotO` prologue).
    StoryGate,
    /// Portal into an instanced dungeon, raid wing, fractal, or strike.
    InstancePortal,
    /// Guild hall instance, accessed via the guild initiative. Rarely
    /// useful for general navigation but represented for completeness.
    GuildHall,
}

/// Expansion / release a map belongs to. Used both per source map and
/// per neighbor so the LLM can filter recommendations by what the
/// account owns.
///
/// Strings rather than abbreviations because the LLM is more likely to
/// surface them verbatim than to remember the canonical short codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Expansion {
    /// Core game (Tyria) — released 2012, free since 2015.
    Core,
    /// Heart of Thorns — 2015. Maguuma Jungle expansion zones.
    HeartOfThorns,
    /// Living World Season 3 — 2016–2017. Bloodstone Fen, Ember Bay,
    /// Bitterfrost Frontier, Lake Doric, Draconis Mons, Siren's Landing.
    LivingWorldSeason3,
    /// Path of Fire — 2017. Crystal Desert + Elona zones.
    PathOfFire,
    /// Living World Season 4 — 2018–2019. Domain of Istan, Sandswept,
    /// Kourna, Jahai, Thunderhead, Dragonfall.
    LivingWorldSeason4,
    /// The Icebrood Saga — 2019–2021. Bjora Marches, Drizzlewood,
    /// Eye of the North revisited. Also known as "Living World
    /// Season 5" (the wiki's `requires = lws5` value maps here).
    IcebroodSaga,
    /// End of Dragons — 2022. Cantha zones.
    EndOfDragons,
    /// Secrets of the Obscure — 2023. Skywatch / Amnytas / Inner Nayos.
    SecretsOfTheObscure,
    /// Janthir Wilds — 2024. Lowland Shore / Bava Nisos / Mistburned
    /// Barrens.
    JanthirWilds,
    /// Castora (Visions of Eternity) — 2025. Starlit Weald / Shipwreck
    /// Strand / Sunqua Peak open-world etc.
    Castora,
}

/// One source map's neighbors. The source-side metadata (name,
/// region) is owned by the loader; per-entry data lives in
/// [`MapNeighborLink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    /// Minimum recommended level for this map. Useful for "where can I
    /// go that's appropriate for my character" planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<u32>,
    /// Maximum recommended level for this map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<u32>,
    /// Which expansion / release introduced this map. Lets callers
    /// filter recommendations by what the player owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Expansion>,
    pub neighbors: Vec<MapNeighborLink>,
}

/// One adjacency edge. `direction` is the compass label from the
/// wiki infobox (`NE`, `SW, S`, `SSW`, …) — opaque to us, surfaced
/// verbatim because the LLM can spell it out for the user.
///
/// The remaining fields enrich the bare adjacency with portal-type
/// detail (`connection`, `gate_location`), level-range guidance
/// (`min_level`, `max_level`), and expansion ownership filtering
/// (`expansion`). All are optional so older YAML fixtures (and tests
/// that build minimal fixtures inline) still parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborLink {
    pub map_id: u32,
    pub name: String,
    /// Compass bearing as written in the wiki infobox. Empty / missing
    /// when the connection isn't a physical border (use `connection`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Kind of connection. Defaults to [`ConnectionType::Physical`] if
    /// missing; the YAML omits it on physical edges to keep size down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionType>,
    /// Free-text "where in the source map" hint for non-physical
    /// connections — e.g. "Astorea Waypoint", "Aerodrome", "Vigil
    /// Keep". Useful for asura-gate edges where the player needs to
    /// know which waypoint to head to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_location: Option<String>,
    /// Whether this edge is one-directional (A → B but not B → A).
    /// Dungeon entrances and some story portals are one-way; defaults
    /// to false (bidirectional) when missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_way: Option<bool>,
    /// Minimum recommended level for the target map. Cheap to inline
    /// here so the LLM doesn't have to follow up with `list_maps_in_region`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<u32>,
    /// Maximum recommended level for the target map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<u32>,
    /// Which expansion / release the target map belongs to. Pairs with
    /// the account's access list for filtering ("show only maps I own").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Expansion>,
    /// Free-text note for anything else worth surfacing — unlock
    /// prerequisites ("requires Aerodrome key"), story-step gating
    /// ("after `SotO` prologue"), or one-line context the wiki carries
    /// that doesn't fit the structured fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    fn from_yaml_parses_enriched_fields() {
        // Forward-compat check: when the scraper populates the new
        // optional fields (`connection`, `min_level`, etc.), they
        // round-trip through serde without losing data.
        let raw = "
34:
  name: Caledon Forest
  region_name: Maguuma Jungle
  min_level: 1
  max_level: 15
  expansion: core
  neighbors:
    - map_id: 22
      name: Brisban Wildlands
      direction: NW
      connection: physical
      min_level: 15
      max_level: 25
      expansion: core
    - map_id: 50
      name: Lion's Arch
      connection: asura_gate
      gate_location: Caledon Forest Waypoint
      min_level: 1
      max_level: 80
      expansion: core
      note: Hub city; reach via the asura gate near Astorian Waypoint.
";
        let nbrs = MapNeighbors::from_yaml(raw).expect("parse");
        let cf = nbrs.get(34).unwrap();
        assert_eq!(cf.min_level, Some(1));
        assert_eq!(cf.max_level, Some(15));
        assert_eq!(cf.expansion, Some(Expansion::Core));
        assert_eq!(cf.neighbors[0].connection, Some(ConnectionType::Physical));
        assert_eq!(cf.neighbors[0].min_level, Some(15));
        assert_eq!(cf.neighbors[1].connection, Some(ConnectionType::AsuraGate));
        assert_eq!(
            cf.neighbors[1].gate_location.as_deref(),
            Some("Caledon Forest Waypoint")
        );
        assert!(cf.neighbors[1].note.is_some());
    }

    #[test]
    fn connection_type_serialises_as_snake_case() {
        // The YAML uses `physical`, `asura_gate`, etc. — lock that in
        // so a future enum rename doesn't silently break the table.
        let yaml = serde_yaml_bw::to_string(&ConnectionType::AsuraGate).unwrap();
        assert!(yaml.contains("asura_gate"), "got: {yaml}");
        let yaml = serde_yaml_bw::to_string(&ConnectionType::InstancePortal).unwrap();
        assert!(yaml.contains("instance_portal"), "got: {yaml}");
    }

    #[test]
    fn expansion_serialises_as_snake_case() {
        let yaml = serde_yaml_bw::to_string(&Expansion::HeartOfThorns).unwrap();
        assert!(yaml.contains("heart_of_thorns"), "got: {yaml}");
        let yaml = serde_yaml_bw::to_string(&Expansion::SecretsOfTheObscure).unwrap();
        assert!(yaml.contains("secrets_of_the_obscure"), "got: {yaml}");
        let yaml = serde_yaml_bw::to_string(&Expansion::IcebroodSaga).unwrap();
        assert!(yaml.contains("icebrood_saga"), "got: {yaml}");
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let nbrs = MapNeighbors::load_embedded().expect("parse");
        // 999999 is well out of GW2's id range.
        assert!(nbrs.get(999_999).is_none());
    }
}
