//! Map / region navigation queries that go beyond the live `find_nearby`
//! POI walk in [`navigation`](super::navigation).
//!
//! Today this is just `list_maps_in_region`. The data backing it is
//! the GW2 continents / floors / regions tree (`/v2/continents/{c}/
//! floors/{f}/regions`), which we walk once across continent 1
//! (Tyria) and continent 2 (Mistlands) on floor 1 (the canonical
//! detail floor), build an in-memory index keyed by both region id
//! and lowercased name, and cache under `STATIC_TTL`.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use super::{STATIC_TTL, Service, ServiceError};
use crate::domain::{
    ConnectionType, Expansion, MapNeighborLink, MapNeighbors, MapNeighborsError, Region,
};

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

/// How the caller asked us to identify a single map. Distinct from
/// [`RegionQuery`] because the input shapes overlap with the
/// `find_nearby` / `get_my_location` "where am I?" idiom — adding a
/// `Here` variant lets the LLM chain `plan_route` with Mumble Link
/// state directly.
#[derive(Debug, Clone)]
pub enum MapRef {
    Id(u32),
    Name(String),
    Here,
}

/// Ranking criterion for the candidate paths after K-shortest selects
/// them. Re-orders the output without changing which paths are
/// considered (that's controlled by `k`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RoutePreference {
    /// Fewest hops (default — same as no preference).
    #[default]
    Shortest,
    /// Among the K shortest, prefer paths with more physical edges
    /// (walking/mounting between maps).
    Walking,
    /// Among the K shortest, prefer paths with more `asura_gate`
    /// transitions (fewest border-crossings).
    Gates,
}

/// Filter knobs applied to `plan_route` paths. All optional; defaults
/// mean "no filtering".
#[derive(Debug, Clone, Default)]
pub struct RouteFilters {
    pub prefer: RoutePreference,
    pub exclude_connections: HashSet<ConnectionType>,
    /// `None` means "no access-based filtering"; `Some(set)` filters
    /// edges whose target map's `expansion` isn't in `set` (with
    /// always-permitted variants like `Core`, `Festival`, LW1/LW2
    /// implicitly added).
    pub player_access: Option<HashSet<Expansion>>,
}

/// Echo of which map the caller meant after we resolved it. Lets the
/// LLM confirm "yes, by 'Caledon Forest' you meant map 873" without a
/// second lookup.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapRefResolved {
    pub map_id: u32,
    pub name: String,
}

/// Output of `plan_route`. The top-K hop-count-shortest paths between
/// two maps in the curated adjacency graph.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutePlan {
    pub from: MapRefResolved,
    pub to: MapRefResolved,
    pub paths: Vec<RoutePath>,
    pub total: usize,
    /// Echo of the effective filter state for the call. Lets the LLM
    /// see what was applied without having to remember its own args.
    pub filters_applied: RouteFiltersSummary,
}

/// Serialisable echo of [`RouteFilters`] for the response. Strings
/// instead of enum variants so the JSON stays self-describing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RouteFiltersSummary {
    pub prefer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_connections: Vec<String>,
    /// `None` when no access filtering was applied (no key, no auto-
    /// fetch, or caller passed an empty list explicitly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_access: Option<Vec<String>>,
    /// Why `player_access` was populated — `"auto"` (fetched from
    /// `/v2/account`) or `"explicit"` (caller passed it in). Absent
    /// when `player_access` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_access_source: Option<String>,
}

/// One candidate route. Counts are pre-summed so the LLM can pick
/// "fewest gate transitions" or "most physical exploration" without
/// re-walking `hops`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoutePath {
    pub hop_count: usize,
    pub asura_gate_count: usize,
    pub physical_count: usize,
    pub story_gate_count: usize,
    pub hops: Vec<RouteHop>,
}

/// One stop along the route. `arrived_via` is `None` on the first hop
/// (the starting map) and `Some(...)` on every subsequent hop,
/// describing how the player got there from the previous hop.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RouteHop {
    pub map_id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrived_via: Option<RouteEdge>,
}

/// Connection metadata for a single edge between two consecutive hops.
/// Mirrors [`MapNeighborLink`] but only the fields the LLM needs to
/// narrate the step.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RouteEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
        "no adjacency data for map id {map_id}. The curated table covers public open-world maps \
         and the major hub cities (Lion's Arch, Divinity's Reach, Black Citadel, Rata Sum, \
         Hoelbrak, The Grove, Eye of the North, Arborstone, Thousand Seas Pavilion, Mistlock \
         Sanctuary, The Wizard's Tower). It does NOT cover instances, fractals, dungeons, raids, \
         guild halls, or WvW maps — those are excluded by design."
    )]
    NoNeighborData { map_id: u32 },

    #[error("failed to load curated map-neighbor table: {0}")]
    NeighborsLoad(MapNeighborsError),

    #[error(
        "no map named `{name}` is present in the curated adjacency table. The graph only covers public open-world maps and the major hub cities — try the map's exact wiki name, or pass a numeric id."
    )]
    MapNotFound { name: String },

    #[error("multiple maps match `{name}` in the adjacency table: {matches}. Disambiguate by id.")]
    MapAmbiguous { name: String, matches: String },

    #[error(
        "no route exists between map {from} and map {to} in the curated adjacency graph. This usually means one or both maps aren't reachable from open-world Tyria via the modelled portals/borders (e.g. a one-way-from-only entrance, or a map that's an island in the graph)."
    )]
    NoRouteFound { from: u32, to: u32 },
}

/// Response shape for `get_map_neighbors`. Wrapped in an object so
/// MCP's `structuredContent` schema accepts it.
///
/// Source-map metadata (`min_level`, `max_level`, `expansion`) is
/// surfaced alongside the neighbor list so the LLM can answer "what's
/// near me and is it level-appropriate?" without a second
/// `list_maps_in_region` call.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MapNeighborsResponse {
    pub map_id: u32,
    pub map_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Expansion>,
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
            min_level: entry.min_level,
            max_level: entry.max_level,
            expansion: entry.expansion,
            total: entry.neighbors.len(),
            neighbors: entry.neighbors,
        })
    }

    /// Plan up to `k` shortest-hop routes between two maps in the
    /// curated adjacency graph. Uses Yen's K-shortest loopless paths
    /// over the directed graph defined by `MapNeighbors` edges, with
    /// uniform edge weight (= one hop per edge).
    ///
    /// `from` and `to` accept any of [`MapRef::Id`], [`MapRef::Name`]
    /// (case-insensitive substring match against the curated table),
    /// or [`MapRef::Here`] (resolves via Mumble Link).
    ///
    /// `filters` applies post-Yen re-ranking ([`RoutePreference`])
    /// and during-search edge skipping (`exclude_connections`,
    /// `player_access`). Defaults — `RouteFilters::default()` — are
    /// "no preference, no exclusions, no access filter".
    ///
    /// `player_access_source` is an informational string echoed back
    /// in the response (`"auto"` / `"explicit"`); the dispatcher
    /// passes it so the LLM can see whether the filter came from
    /// auto-fetched account data or from explicit caller args.
    ///
    /// `k` is clamped to `1..=10` so a runaway request can't burn CPU.
    pub fn plan_route(
        &self,
        from: MapRef,
        to: MapRef,
        k: usize,
        filters: RouteFilters,
        player_access_source: Option<&'static str>,
    ) -> Result<RoutePlan, ServiceError> {
        let table = neighbors_table()?;
        let from_id = self.resolve_map_ref(&from, table)?;
        let to_id = self.resolve_map_ref(&to, table)?;
        let k = k.clamp(1, 10);
        let summary = summarise_filters(&filters, player_access_source);

        let from_entry =
            table
                .get(from_id)
                .ok_or(ServiceError::Region(RegionLookupError::NoNeighborData {
                    map_id: from_id,
                }))?;
        let to_entry =
            table
                .get(to_id)
                .ok_or(ServiceError::Region(RegionLookupError::NoNeighborData {
                    map_id: to_id,
                }))?;

        // Trivial path: start == end.
        if from_id == to_id {
            return Ok(RoutePlan {
                from: MapRefResolved {
                    map_id: from_id,
                    name: from_entry.name.clone(),
                },
                to: MapRefResolved {
                    map_id: to_id,
                    name: to_entry.name.clone(),
                },
                paths: vec![RoutePath {
                    hop_count: 0,
                    asura_gate_count: 0,
                    physical_count: 0,
                    story_gate_count: 0,
                    hops: vec![RouteHop {
                        map_id: from_id,
                        name: from_entry.name.clone(),
                        arrived_via: None,
                    }],
                }],
                total: 1,
                filters_applied: summary,
            });
        }

        let id_paths = k_shortest_paths_filtered(table, from_id, to_id, k, &filters);
        if id_paths.is_empty() {
            return Err(ServiceError::Region(RegionLookupError::NoRouteFound {
                from: from_id,
                to: to_id,
            }));
        }

        let mut paths: Vec<RoutePath> = id_paths
            .into_iter()
            .map(|p| build_route_path(table, &p))
            .collect();
        sort_paths_by_preference(&mut paths, filters.prefer);
        Ok(RoutePlan {
            total: paths.len(),
            from: MapRefResolved {
                map_id: from_id,
                name: from_entry.name.clone(),
            },
            to: MapRefResolved {
                map_id: to_id,
                name: to_entry.name.clone(),
            },
            paths,
            filters_applied: summary,
        })
    }

    fn resolve_map_ref(
        &self,
        r: &MapRef,
        table: &'static MapNeighbors,
    ) -> Result<u32, ServiceError> {
        match r {
            MapRef::Id(id) => Ok(*id),
            MapRef::Name(n) => resolve_map_name(table, n),
            MapRef::Here => {
                let snap = self.mumble.snapshot()?;
                Ok(snap.context.map_id)
            }
        }
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

/// Look up a map id by case-insensitive name. Builds a fresh index
/// each call — the table is small (<150 entries) so the cost is
/// negligible, and avoiding a second `OnceLock` keeps the module
/// simple.
fn resolve_map_name(table: &MapNeighbors, name: &str) -> Result<u32, ServiceError> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return Err(ServiceError::Region(RegionLookupError::MapNotFound {
            name: name.to_owned(),
        }));
    }
    let mut exact: Vec<(u32, String)> = Vec::new();
    let mut substring: Vec<(u32, String)> = Vec::new();
    for (id, entry) in table.iter() {
        let lower = entry.name.to_lowercase();
        if lower == needle {
            exact.push((id, entry.name.clone()));
        } else if lower.contains(&needle) {
            substring.push((id, entry.name.clone()));
        }
    }
    if exact.len() == 1 {
        return Ok(exact[0].0);
    }
    if !exact.is_empty() {
        let names: Vec<String> = exact.iter().map(|(_, n)| n.clone()).collect();
        return Err(ServiceError::Region(RegionLookupError::MapAmbiguous {
            name: name.to_owned(),
            matches: names.join(", "),
        }));
    }
    match substring.len() {
        0 => Err(ServiceError::Region(RegionLookupError::MapNotFound {
            name: name.to_owned(),
        })),
        1 => Ok(substring[0].0),
        _ => {
            let mut names: Vec<String> = substring.iter().map(|(_, n)| n.clone()).collect();
            names.sort();
            Err(ServiceError::Region(RegionLookupError::MapAmbiguous {
                name: name.to_owned(),
                matches: names.join(", "),
            }))
        }
    }
}

/// Edge-filter checks for Yen's during-search exclusion. Returns
/// `true` when the edge `src → link` should be traversable under
/// `filters`.
fn edge_passes_filters(link: &MapNeighborLink, filters: &RouteFilters) -> bool {
    if let Some(conn) = link.connection
        && filters.exclude_connections.contains(&conn)
    {
        return false;
    }
    if let Some(allowed) = &filters.player_access
        && let Some(req) = link.expansion
    {
        let always_ok = matches!(
            req,
            Expansion::Core
                | Expansion::Festival
                | Expansion::LivingWorldSeason1
                | Expansion::LivingWorldSeason2
        );
        if !always_ok && !allowed.contains(&req) {
            return false;
        }
    }
    true
}

/// Yen's K-shortest loopless paths over the directed graph defined by
/// `nbrs`, with edge filters applied during the BFS subroutine.
/// Uniform edge weight (one hop per edge).
///
/// Returns up to `k` paths, sorted by ascending hop count. Empty if no
/// path exists under the filters.
fn k_shortest_paths_filtered(
    nbrs: &MapNeighbors,
    src: u32,
    dst: u32,
    k: usize,
    filters: &RouteFilters,
) -> Vec<Vec<u32>> {
    let mut accepted: Vec<Vec<u32>> = Vec::new();
    // Candidate set keyed by path content. The BTreeSet keeps
    // insertion stable for the same-length tie-break.
    let mut candidates: BTreeSet<Vec<u32>> = BTreeSet::new();

    let Some(first) = bfs_path(nbrs, src, dst, &HashSet::new(), &HashSet::new(), filters) else {
        return Vec::new();
    };
    accepted.push(first);

    while accepted.len() < k {
        let prev = accepted.last().expect("at least one accepted").clone();
        if prev.len() < 2 {
            break;
        }
        for i in 0..prev.len() - 1 {
            let spur_node = prev[i];
            let root_path: &[u32] = &prev[0..=i];

            let mut blocked_edges: HashSet<(u32, u32)> = HashSet::new();
            for p in &accepted {
                if p.len() > i + 1 && &p[0..=i] == root_path {
                    blocked_edges.insert((p[i], p[i + 1]));
                }
            }
            let blocked_nodes: HashSet<u32> = root_path[..i].iter().copied().collect();

            let Some(spur) = bfs_path(
                nbrs,
                spur_node,
                dst,
                &blocked_nodes,
                &blocked_edges,
                filters,
            ) else {
                continue;
            };
            let mut total = root_path.to_vec();
            total.extend(spur.iter().skip(1).copied());
            candidates.insert(total);
        }
        let Some(next) = candidates.iter().min_by_key(|p| p.len()).cloned() else {
            break;
        };
        candidates.remove(&next);
        accepted.push(next);
    }
    accepted
}

/// BFS shortest path with blocked nodes, blocked edges, and the
/// caller's filter set.
fn bfs_path(
    nbrs: &MapNeighbors,
    src: u32,
    dst: u32,
    blocked_nodes: &HashSet<u32>,
    blocked_edges: &HashSet<(u32, u32)>,
    filters: &RouteFilters,
) -> Option<Vec<u32>> {
    if blocked_nodes.contains(&src) {
        return None;
    }
    if src == dst {
        return Some(vec![src]);
    }
    let mut prev: HashMap<u32, u32> = HashMap::new();
    let mut q: VecDeque<u32> = VecDeque::new();
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(src);
    q.push_back(src);
    while let Some(cur) = q.pop_front() {
        let Some(entry) = nbrs.get(cur) else {
            continue;
        };
        for link in &entry.neighbors {
            let next = link.map_id;
            if seen.contains(&next) {
                continue;
            }
            if blocked_nodes.contains(&next) {
                continue;
            }
            if blocked_edges.contains(&(cur, next)) {
                continue;
            }
            if !edge_passes_filters(link, filters) {
                continue;
            }
            prev.insert(next, cur);
            if next == dst {
                let mut path = vec![dst];
                let mut cursor = dst;
                while let Some(&p) = prev.get(&cursor) {
                    path.push(p);
                    cursor = p;
                }
                path.reverse();
                return Some(path);
            }
            seen.insert(next);
            q.push_back(next);
        }
    }
    None
}

/// Re-rank the K accepted paths according to the caller's preference.
/// Yen's already produces them in ascending-hop order; this is purely
/// a stable secondary sort. For `Shortest` it's a no-op.
fn sort_paths_by_preference(paths: &mut [RoutePath], prefer: RoutePreference) {
    match prefer {
        RoutePreference::Shortest => {}
        RoutePreference::Walking => {
            paths.sort_by(|a, b| {
                b.physical_count
                    .cmp(&a.physical_count)
                    .then_with(|| a.hop_count.cmp(&b.hop_count))
            });
        }
        RoutePreference::Gates => {
            paths.sort_by(|a, b| {
                b.asura_gate_count
                    .cmp(&a.asura_gate_count)
                    .then_with(|| a.hop_count.cmp(&b.hop_count))
            });
        }
    }
}

fn summarise_filters(
    filters: &RouteFilters,
    player_access_source: Option<&'static str>,
) -> RouteFiltersSummary {
    let prefer = match filters.prefer {
        RoutePreference::Shortest => "shortest",
        RoutePreference::Walking => "walking",
        RoutePreference::Gates => "gates",
    }
    .to_owned();
    let mut excludes: Vec<String> = filters
        .exclude_connections
        .iter()
        .copied()
        .map(connection_type_to_snake)
        .collect();
    excludes.sort();
    let player_access = filters.player_access.as_ref().map(|set| {
        let mut v: Vec<String> = set.iter().copied().map(expansion_to_snake).collect();
        v.sort();
        v
    });
    RouteFiltersSummary {
        prefer,
        exclude_connections: excludes,
        player_access: player_access.clone(),
        player_access_source: if player_access.is_some() {
            player_access_source.map(str::to_owned)
        } else {
            None
        },
    }
}

fn connection_type_to_snake(c: ConnectionType) -> String {
    match c {
        ConnectionType::Physical => "physical",
        ConnectionType::AsuraGate => "asura_gate",
        ConnectionType::StoryGate => "story_gate",
        ConnectionType::InstancePortal => "instance_portal",
        ConnectionType::GuildHall => "guild_hall",
    }
    .to_owned()
}

fn expansion_to_snake(e: Expansion) -> String {
    match e {
        Expansion::Core => "core",
        Expansion::LivingWorldSeason1 => "living_world_season1",
        Expansion::LivingWorldSeason2 => "living_world_season2",
        Expansion::HeartOfThorns => "heart_of_thorns",
        Expansion::LivingWorldSeason3 => "living_world_season3",
        Expansion::PathOfFire => "path_of_fire",
        Expansion::LivingWorldSeason4 => "living_world_season4",
        Expansion::IcebroodSaga => "icebrood_saga",
        Expansion::EndOfDragons => "end_of_dragons",
        Expansion::SecretsOfTheObscure => "secrets_of_the_obscure",
        Expansion::JanthirWilds => "janthir_wilds",
        Expansion::Castora => "castora",
        Expansion::Festival => "festival",
    }
    .to_owned()
}

/// Map the `access` array from `/v2/account` into the set of
/// `Expansion` variants the player effectively owns. Includes implicit
/// LW season access (`HoT` ⇒ LW3 maps; `PoF` ⇒ LW4 maps) and the
/// always-permitted variants (Core, LW1, LW2, Festival).
pub fn expand_account_access(access: &[String]) -> HashSet<Expansion> {
    let mut owned: HashSet<Expansion> = HashSet::new();
    // Always-permitted maps don't need explicit account flags.
    owned.insert(Expansion::Core);
    owned.insert(Expansion::LivingWorldSeason1);
    owned.insert(Expansion::LivingWorldSeason2);
    owned.insert(Expansion::Festival);
    for s in access {
        match s.as_str() {
            "GuildWars2" | "PlayForFree" => {
                owned.insert(Expansion::Core);
            }
            "HeartOfThorns" => {
                owned.insert(Expansion::HeartOfThorns);
            }
            "PathOfFire" => {
                owned.insert(Expansion::PathOfFire);
            }
            "EndOfDragons" => {
                owned.insert(Expansion::EndOfDragons);
            }
            "SecretsOfTheObscure" => {
                owned.insert(Expansion::SecretsOfTheObscure);
            }
            "JanthirWilds" => {
                owned.insert(Expansion::JanthirWilds);
            }
            // Future-proofing: the API may surface Castora once it
            // ships under a specific tag (currently unconfirmed).
            "Castora" | "VisionsOfEternity" => {
                owned.insert(Expansion::Castora);
            }
            // Icebrood Saga was sold separately at launch; the API
            // hasn't (historically) returned a dedicated flag for
            // it, so any LW5/IBS-tagged map is currently treated as
            // permitted only if explicitly added to the access list.
            "IcebroodSaga" => {
                owned.insert(Expansion::IcebroodSaga);
            }
            _ => {}
        }
    }
    if owned.contains(&Expansion::HeartOfThorns) {
        owned.insert(Expansion::LivingWorldSeason3);
    }
    if owned.contains(&Expansion::PathOfFire) {
        owned.insert(Expansion::LivingWorldSeason4);
    }
    owned
}

/// Translate a sequence of map ids into a [`RoutePath`] with hop
/// metadata pulled from the curated table.
fn build_route_path(nbrs: &MapNeighbors, ids: &[u32]) -> RoutePath {
    let mut hops: Vec<RouteHop> = Vec::with_capacity(ids.len());
    let mut asura = 0usize;
    let mut physical = 0usize;
    let mut story = 0usize;
    for (idx, &id) in ids.iter().enumerate() {
        let entry = nbrs.get(id);
        let name = entry.map_or_else(|| format!("map {id}"), |e| e.name.clone());
        let arrived_via = if idx == 0 {
            None
        } else {
            let prev_id = ids[idx - 1];
            nbrs.get(prev_id).and_then(|prev_entry| {
                prev_entry
                    .neighbors
                    .iter()
                    .find(|n| n.map_id == id)
                    .map(|link| {
                        match link.connection {
                            Some(ConnectionType::AsuraGate) => asura += 1,
                            Some(ConnectionType::Physical) => physical += 1,
                            Some(ConnectionType::StoryGate) => story += 1,
                            _ => {}
                        }
                        RouteEdge {
                            connection: link.connection,
                            direction: link.direction.clone(),
                            gate_location: link.gate_location.clone(),
                            note: link.note.clone(),
                        }
                    })
            })
        };
        hops.push(RouteHop {
            map_id: id,
            name,
            arrived_via,
        });
    }
    RoutePath {
        hop_count: ids.len().saturating_sub(1),
        asura_gate_count: asura,
        physical_count: physical,
        story_gate_count: story,
        hops,
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

    // ----- plan_route / Yen's K-shortest -----

    /// Build a tiny synthetic adjacency graph from a (src, dst) edge
    /// list. Names match the ids ("Map 1", "Map 2", …) so the test
    /// only has to think about ids.
    fn graph_from_edges(edges: &[(u32, u32)]) -> MapNeighbors {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;
        let mut by_id: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &(a, b) in edges {
            by_id.entry(a).or_default().push(b);
            // Materialise destination nodes too — otherwise k_shortest
            // can't return paths that pass through them.
            by_id.entry(b).or_default();
        }
        let yaml_lines: Vec<String> = by_id
            .iter()
            .map(|(id, ns)| {
                let mut s = format!("{id}:\n  name: Map {id}\n");
                if ns.is_empty() {
                    s.push_str("  neighbors: []\n");
                } else {
                    s.push_str("  neighbors:\n");
                    for n in ns {
                        let _ = writeln!(
                            s,
                            "    - map_id: {n}\n      name: Map {n}\n      connection: physical",
                        );
                    }
                }
                s
            })
            .collect();
        MapNeighbors::from_yaml(&yaml_lines.join("")).expect("test fixture parses")
    }

    #[test]
    fn k_shortest_paths_returns_top_k_by_hop_count() {
        // Diamond: 1 → 2 → 4, 1 → 3 → 4, 1 → 2 → 3 → 4.
        let g = graph_from_edges(&[(1, 2), (1, 3), (2, 4), (3, 4), (2, 3)]);
        let paths = k_shortest_paths_filtered(&g, 1, 4, 3, &RouteFilters::default());
        assert_eq!(paths.len(), 3);
        // Two shortest paths of length 3 (1→2→4 and 1→3→4) plus the
        // length-4 path (1→2→3→4).
        assert_eq!(paths[0].len(), 3);
        assert_eq!(paths[1].len(), 3);
        assert_eq!(paths[2].len(), 4);
    }

    #[test]
    fn k_shortest_paths_returns_empty_on_no_route() {
        // 1 and 4 are in disjoint components.
        let g = graph_from_edges(&[(1, 2), (3, 4)]);
        let paths = k_shortest_paths_filtered(&g, 1, 4, 3, &RouteFilters::default());
        assert!(paths.is_empty());
    }

    #[test]
    fn k_shortest_paths_returns_trivial_path_on_self() {
        let g = graph_from_edges(&[(1, 2)]);
        let paths = k_shortest_paths_filtered(&g, 1, 1, 3, &RouteFilters::default());
        assert_eq!(paths, vec![vec![1]]);
    }

    #[test]
    fn k_shortest_paths_loopless() {
        // A graph with a cycle that could trap a naive enumerator.
        // 1 → 2 → 3 → 4; 2 → 3 → 2 (loop) so we must reject paths
        // that revisit a node.
        let g = graph_from_edges(&[(1, 2), (2, 3), (3, 2), (3, 4)]);
        let paths = k_shortest_paths_filtered(&g, 1, 4, 5, &RouteFilters::default());
        // Only one loopless path exists.
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], vec![1, 2, 3, 4]);
    }

    #[test]
    fn k_shortest_paths_handles_directed_graph() {
        // 1 → 2 → 3, but 3 has no edge back to 2. The reverse query
        // should fail.
        let g = graph_from_edges(&[(1, 2), (2, 3)]);
        let forward = k_shortest_paths_filtered(&g, 1, 3, 3, &RouteFilters::default());
        assert_eq!(forward.len(), 1);
        let reverse = k_shortest_paths_filtered(&g, 3, 1, 3, &RouteFilters::default());
        assert!(reverse.is_empty());
    }

    #[test]
    fn k_shortest_paths_returns_real_caledon_to_auric_basin() {
        // Real curated table — at least one route must exist; first
        // route must be reasonably short.
        let table = MapNeighbors::load_embedded().expect("embedded YAML parses");
        // 34 = Caledon Forest, 1043 = Auric Basin.
        let paths = k_shortest_paths_filtered(&table, 34, 1043, 3, &RouteFilters::default());
        assert!(!paths.is_empty(), "expected at least one route");
        let first = &paths[0];
        assert!(first.first() == Some(&34) && first.last() == Some(&1043));
        assert!(
            first.len() <= 6,
            "shortest route from Caledon Forest to Auric Basin should be <=5 hops, got {first:?}",
        );
    }

    #[test]
    fn resolve_map_name_finds_canonical_name() {
        let table = MapNeighbors::load_embedded().expect("parses");
        assert_eq!(resolve_map_name(&table, "Caledon Forest").unwrap(), 34);
        // Substring matches too.
        assert_eq!(resolve_map_name(&table, "caledon").unwrap(), 34);
    }

    #[test]
    fn resolve_map_name_empty_returns_not_found() {
        let table = MapNeighbors::load_embedded().expect("parses");
        let err = resolve_map_name(&table, "   ").unwrap_err();
        assert!(err.to_string().contains("no map named"), "got: {err}");
    }

    #[test]
    fn expand_account_access_adds_implicit_and_free_variants() {
        let owned = expand_account_access(&[
            "GuildWars2".to_owned(),
            "HeartOfThorns".to_owned(),
            "PathOfFire".to_owned(),
        ]);
        // Direct
        assert!(owned.contains(&Expansion::Core));
        assert!(owned.contains(&Expansion::HeartOfThorns));
        assert!(owned.contains(&Expansion::PathOfFire));
        // Always-permitted free / festival / LW1-2
        assert!(owned.contains(&Expansion::Festival));
        assert!(owned.contains(&Expansion::LivingWorldSeason1));
        assert!(owned.contains(&Expansion::LivingWorldSeason2));
        // Implicit-from-direct
        assert!(owned.contains(&Expansion::LivingWorldSeason3));
        assert!(owned.contains(&Expansion::LivingWorldSeason4));
        // Not owned
        assert!(!owned.contains(&Expansion::EndOfDragons));
        assert!(!owned.contains(&Expansion::JanthirWilds));
    }

    #[test]
    fn k_shortest_paths_filtered_excludes_blocked_connection() {
        // 1 → 2 (physical), 1 → 3 (asura_gate) → 2 (physical)
        let yaml = r"
1:
  name: A
  neighbors:
    - map_id: 2
      name: B
      connection: physical
    - map_id: 3
      name: C
      connection: asura_gate
2:
  name: B
  neighbors: []
3:
  name: C
  neighbors:
    - map_id: 2
      name: B
      connection: physical
";
        let g = MapNeighbors::from_yaml(yaml).expect("parses");
        let mut filters = RouteFilters::default();
        filters
            .exclude_connections
            .insert(ConnectionType::AsuraGate);
        let paths = k_shortest_paths_filtered(&g, 1, 2, 5, &filters);
        // The direct physical edge is still usable; the gate path is
        // blocked.
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], vec![1, 2]);
    }

    #[test]
    fn k_shortest_paths_filtered_respects_player_access() {
        // 1 (core) → 2 (heart_of_thorns) → 3 (end_of_dragons)
        let yaml = r"
1:
  name: A
  expansion: core
  neighbors:
    - map_id: 2
      name: B
      connection: physical
      expansion: heart_of_thorns
2:
  name: B
  expansion: heart_of_thorns
  neighbors:
    - map_id: 3
      name: C
      connection: physical
      expansion: end_of_dragons
3:
  name: C
  expansion: end_of_dragons
  neighbors: []
";
        let g = MapNeighbors::from_yaml(yaml).expect("parses");
        // Player owns HoT only — can reach B but not C.
        let mut filters = RouteFilters::default();
        let mut access = HashSet::new();
        access.insert(Expansion::Core);
        access.insert(Expansion::HeartOfThorns);
        filters.player_access = Some(access);
        let paths = k_shortest_paths_filtered(&g, 1, 3, 5, &filters);
        assert!(paths.is_empty(), "EoD unreachable without that expansion");
        let two = k_shortest_paths_filtered(&g, 1, 2, 5, &filters);
        assert_eq!(two.len(), 1, "HoT-only player can reach HoT map");
    }

    #[test]
    fn sort_paths_by_preference_walking_promotes_physical_count() {
        let mut paths = vec![
            RoutePath {
                hop_count: 3,
                asura_gate_count: 3,
                physical_count: 0,
                story_gate_count: 0,
                hops: vec![],
            },
            RoutePath {
                hop_count: 3,
                asura_gate_count: 0,
                physical_count: 3,
                story_gate_count: 0,
                hops: vec![],
            },
        ];
        sort_paths_by_preference(&mut paths, RoutePreference::Walking);
        assert_eq!(paths[0].physical_count, 3);
        sort_paths_by_preference(&mut paths, RoutePreference::Gates);
        assert_eq!(paths[0].asura_gate_count, 3);
    }

    #[test]
    fn build_route_path_classifies_edges() {
        // 1 →(physical) 2 →(asura_gate) 3 →(story_gate) 4
        let yaml = r"
1:
  name: Alpha
  neighbors:
    - map_id: 2
      name: Beta
      connection: physical
      direction: E
2:
  name: Beta
  neighbors:
    - map_id: 3
      name: Gamma
      connection: asura_gate
      gate_location: Beta Waypoint
3:
  name: Gamma
  neighbors:
    - map_id: 4
      name: Delta
      connection: story_gate
      note: requires story step
4:
  name: Delta
  neighbors: []
";
        let g = MapNeighbors::from_yaml(yaml).expect("parses");
        let path = build_route_path(&g, &[1, 2, 3, 4]);
        assert_eq!(path.hop_count, 3);
        assert_eq!(path.physical_count, 1);
        assert_eq!(path.asura_gate_count, 1);
        assert_eq!(path.story_gate_count, 1);
        assert_eq!(path.hops.len(), 4);
        assert!(path.hops[0].arrived_via.is_none());
        let edge_to_beta = path.hops[1].arrived_via.as_ref().unwrap();
        assert_eq!(edge_to_beta.connection, Some(ConnectionType::Physical));
        assert_eq!(edge_to_beta.direction.as_deref(), Some("E"));
        let edge_to_gamma = path.hops[2].arrived_via.as_ref().unwrap();
        assert_eq!(
            edge_to_gamma.gate_location.as_deref(),
            Some("Beta Waypoint")
        );
        let edge_to_delta = path.hops[3].arrived_via.as_ref().unwrap();
        assert_eq!(edge_to_delta.note.as_deref(), Some("requires story step"));
    }
}
