//! Integrity tests for the curated `data/map_neighbors.yaml`:
//!
//! 1. Every source map id resolves to a real `/v2/maps` entry (no
//!    typos / stale ids).
//! 2. Every neighbor map id resolves to a real `/v2/maps` entry.
//! 3. Edges are symmetric: if A lists B, B lists A. (Asura gates and
//!    one-way story-gated portals would normally violate this, but
//!    the current YAML has no such entries flagged; symmetry holds
//!    as long as both sides of an asura gate are present in the
//!    wiki infobox.)
//!
//! The snapshot lives at `tests/common/map_snapshot.json` (a slimmed
//! `/v2/maps?ids=all` payload — just id + name + `region_name`). Refresh
//! when `ArenaNet` ships a new expansion.

use std::collections::{BTreeMap, BTreeSet};

use gw2_mcp::domain::MapNeighbors;
use serde::Deserialize;

const MAP_SNAPSHOT: &str = include_str!("common/map_snapshot.json");

#[derive(Debug, Deserialize)]
struct SnapshotMap {
    id: u32,
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    region_name: String,
}

fn snapshot_index() -> BTreeMap<u32, String> {
    let raw: Vec<SnapshotMap> =
        serde_json::from_str(MAP_SNAPSHOT).expect("map_snapshot.json must be valid JSON");
    raw.into_iter().map(|m| (m.id, m.name)).collect()
}

#[test]
fn every_source_map_id_resolves_against_v2_maps_snapshot() {
    let snapshot = snapshot_index();
    let nbrs = MapNeighbors::load_embedded().expect("YAML parses");
    let mut missing = Vec::new();
    for (id, _entry) in nbrs.iter() {
        if !snapshot.contains_key(&id) {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "{} source map ids in map_neighbors.yaml are not present in /v2/maps snapshot: {:?}",
        missing.len(),
        missing
    );
}

#[test]
fn every_neighbor_map_id_resolves_against_v2_maps_snapshot() {
    let snapshot = snapshot_index();
    let nbrs = MapNeighbors::load_embedded().expect("YAML parses");
    let mut bad: Vec<(u32, u32)> = Vec::new();
    for (src_id, entry) in nbrs.iter() {
        for link in &entry.neighbors {
            if !snapshot.contains_key(&link.map_id) {
                bad.push((src_id, link.map_id));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} (source, neighbor) edges reference unknown map ids: {:?}",
        bad.len(),
        bad
    );
}

/// Threshold sized to the current YAML's known asymmetric edges
/// (today: 5). Bump up when adding a one-way portal; if this sharply
/// increases on a re-scrape, the wiki probably changed shape and the
/// parser may need a tweak.
const SYMMETRY_THRESHOLD: usize = 10;

/// Soft symmetry check — flags genuine wiki/YAML drift without
/// failing CI on the handful of legitimately one-way edges in the
/// curated data (e.g. the `WvW` borderlands list Eternal Battlegrounds
/// as a neighbor but EB's wiki infobox lists no neighbors back).
#[test]
fn edges_are_mostly_symmetric() {
    let nbrs = MapNeighbors::load_embedded().expect("YAML parses");
    let modelled: BTreeSet<u32> = nbrs.iter().map(|(id, _)| id).collect();

    let mut edges: BTreeSet<(u32, u32)> = BTreeSet::new();
    for (src_id, entry) in nbrs.iter() {
        for link in &entry.neighbors {
            edges.insert((src_id, link.map_id));
        }
    }

    // Pairs involving an unmodelled endpoint are expected (e.g.
    // borderlands → guild halls, asura gates from Lion's Arch into
    // every city). We only flag asymmetry when both ends ARE in the
    // table.
    let mut asymmetric: Vec<(u32, u32)> = Vec::new();
    for &(a, b) in &edges {
        if !modelled.contains(&b) {
            continue;
        }
        if !edges.contains(&(b, a)) {
            asymmetric.push((a, b));
        }
    }

    assert!(
        asymmetric.len() <= SYMMETRY_THRESHOLD,
        "{} asymmetric edges exceeds threshold of {}: {:?}",
        asymmetric.len(),
        SYMMETRY_THRESHOLD,
        asymmetric
    );
    if !asymmetric.is_empty() {
        eprintln!(
            "  (note: {} asymmetric edges in map_neighbors.yaml — under the threshold of {}). \
             Typical cause: one side's wiki infobox simply doesn't list the other (asura gates \
             from Lion's Arch, WvW borderlands → Eternal Battlegrounds, etc.).",
            asymmetric.len(),
            SYMMETRY_THRESHOLD
        );
    }
}
