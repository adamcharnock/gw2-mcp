//! Navigation tools — `get_my_location`, `get_directions`, `find_nearby`,
//! `describe_facing`. Composes the `MumbleLink` and `MapData` ports
//! plus the cache.
//!
//! The navigation public types (`LocationRef`, `NearbyFilter`, the
//! per-tool result structs) live here too, re-exported from
//! `super::mod` so the MCP adapter's `oneOf` schemas keep working
//! against the same paths.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    STATIC_TTL, Service, ServiceError, mount_index_to_name, profession_byte_to_name,
    race_byte_to_name,
};
use crate::domain::bearing::{Bearing16, bearing, distance_meters, distance_units};
use crate::ports::{MapId, MapInfo, MapPoi};

impl Service {
    // -----------------------------------------------------------------
    // Navigation (Tier 6B): Mumble Link + MapData composition.
    // -----------------------------------------------------------------

    /// Where the player currently is, in their own words.
    pub async fn get_my_location(&self) -> Result<MyLocationSnapshot, ServiceError> {
        let snap = self.mumble.snapshot()?;
        let map_id = snap.context.map_id;
        let map_info = self.get_map_cached(map_id).await.ok();
        let position = (
            f64::from(snap.context.player_x),
            f64::from(snap.context.player_y),
        );
        // Facing in the 2D map plane. Mumble's `f_avatar_front` is a 3D
        // unit vector with Z-up; the X axis aligns with map-east, but the
        // Y axis is map-NORTH (Mumble's Y is up out of the world plane,
        // which corresponds to map-north because GW2's world plane is XY
        // with Y-axis pointing north). Empirically this produces correct
        // bearings; live verification is queued (see runbook).
        let facing = facing_bearing(snap.avatar_front);
        let profession_name = snap
            .identity
            .profession
            .and_then(|p| profession_byte_to_name(p).map(str::to_owned));
        let race_name = snap
            .identity
            .race
            .and_then(|b| race_byte_to_name(b).map(str::to_owned));
        let mount_index = snap.context.mount_index;
        let mount = MountInfo {
            index: mount_index,
            name: mount_index_to_name(mount_index).map(str::to_owned),
        };
        Ok(MyLocationSnapshot {
            character_name: snap.identity.name.clone().unwrap_or_default(),
            profession_name,
            race_name,
            mount,
            map: map_info.map(map_summary),
            map_id,
            position,
            facing_bearing: facing,
            ui_tick: snap.ui_tick,
            captured_at: self.clock.now(),
        })
    }

    /// Bearing + distance from one point to another. Handles three
    /// `LocationRef` shapes per the MCP schema: literal coords, named
    /// POI on a known map, or "wherever I am right now".
    pub async fn get_directions(
        &self,
        from: LocationRef,
        to: LocationRef,
    ) -> Result<DirectionsResult, ServiceError> {
        let from_pt = self.resolve_location(&from).await?;
        let to_pt = self.resolve_location(&to).await?;
        let bearing16 = bearing(from_pt.coord, to_pt.coord);
        let units = distance_units(from_pt.coord, to_pt.coord);
        let meters = distance_meters(from_pt.coord, to_pt.coord);
        Ok(DirectionsResult {
            from: from_pt,
            to: to_pt,
            bearing: bearing16,
            bearing_label: bearing16.label().to_owned(),
            distance_units: units,
            distance_meters: meters,
        })
    }

    /// Top `limit` POIs near `around`, optionally filtered by kind.
    /// Defaults to the player's current location if `around` is `Here`.
    ///
    /// Wraps the result list in a [`NearbySearchResult`] so MCP's
    /// `structuredContent` schema (object-only) accepts it.
    pub async fn find_nearby(
        &self,
        filter: NearbyFilter,
        around: LocationRef,
        limit: usize,
    ) -> Result<NearbySearchResult, ServiceError> {
        let here = self.resolve_location(&around).await?;
        // We need *some* map id to enumerate POIs. If the caller passed
        // literal coords without a map context, we fall back to the
        // Mumble Link's current map.
        let map_id = match here.map_id {
            Some(m) => m,
            None => self.mumble.snapshot()?.context.map_id,
        };
        let pois = self.list_pois_cached(map_id).await?;
        let mut results: Vec<NearbyResult> = pois
            .into_iter()
            .filter(|p| filter.matches(&p.kind))
            .map(|p| {
                let bearing16 = bearing(here.coord, p.coord);
                NearbyResult {
                    distance_units: distance_units(here.coord, p.coord),
                    distance_meters: distance_meters(here.coord, p.coord),
                    bearing: bearing16,
                    bearing_label: bearing16.label().to_owned(),
                    poi: p,
                }
            })
            .collect();
        // Stable sort by distance ascending; ties broken by id for
        // determinism so paginated callers get the same order across
        // requests.
        results.sort_by(|a, b| {
            a.distance_units
                .partial_cmp(&b.distance_units)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.poi.id.cmp(&b.poi.id))
        });
        results.truncate(limit);
        Ok(NearbySearchResult {
            total: results.len(),
            results,
            origin: here.coord,
            map_id,
            filter: filter.label().to_owned(),
        })
    }

    /// One-shot description of which way the avatar is currently facing,
    /// plus the closest POI in that quadrant. Surfaces `MumbleError`
    /// directly if the link isn't available.
    pub async fn describe_facing(&self) -> Result<FacingDescription, ServiceError> {
        let snap = self.mumble.snapshot()?;
        let position = (
            f64::from(snap.context.player_x),
            f64::from(snap.context.player_y),
        );
        let facing = facing_bearing(snap.avatar_front);
        let map_id = snap.context.map_id;

        // Best landmark in the same compass direction as we're facing.
        // We don't strictly require an exact match; ±2 wedges (≈45°) is
        // enough to pick a "in front of you" POI.
        let pois = self.list_pois_cached(map_id).await.unwrap_or_default();
        let nearest = pois
            .into_iter()
            .filter(|p| matches!(p.kind.as_str(), "waypoint" | "landmark"))
            .filter_map(|p| {
                let b = bearing(position, p.coord);
                if compass_index(b) == compass_index(facing)
                    || (compass_index_diff(b, facing).unwrap_or(99) <= 2)
                {
                    Some((distance_units(position, p.coord), p, b))
                } else {
                    None
                }
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let (nearest_landmark, dist_meters) = match nearest {
            Some((_, p, _)) => {
                let m = distance_meters(position, p.coord);
                (Some(p), Some(m))
            }
            None => (None, None),
        };

        Ok(FacingDescription {
            facing_bearing: facing,
            facing_label: facing.label().to_owned(),
            facing_long_name: facing.long_name().to_owned(),
            nearest_landmark,
            nearest_landmark_distance_meters: dist_meters,
            map_id,
            position,
            ui_tick: snap.ui_tick,
        })
    }

    // ----- internal helpers ------------------------------------------------

    async fn resolve_location(&self, loc: &LocationRef) -> Result<ResolvedLocation, ServiceError> {
        match loc {
            LocationRef::Coords { coord, map_id } => Ok(ResolvedLocation {
                coord: *coord,
                map_id: *map_id,
                label: format!("({:.1}, {:.1})", coord.0, coord.1),
            }),
            LocationRef::NamedPoi { map_id, name } => {
                let pois = self.list_pois_cached(*map_id).await?;
                let needle = name.trim().to_ascii_lowercase();
                let hit = pois
                    .into_iter()
                    .find(|p| p.name.trim().to_ascii_lowercase() == needle)
                    .ok_or_else(|| ServiceError::NoSuchPoi {
                        map_id: *map_id,
                        name: name.clone(),
                    })?;
                Ok(ResolvedLocation {
                    coord: hit.coord,
                    map_id: Some(*map_id),
                    label: hit.name,
                })
            }
            LocationRef::Here => {
                let snap = self.mumble.snapshot()?;
                Ok(ResolvedLocation {
                    coord: (
                        f64::from(snap.context.player_x),
                        f64::from(snap.context.player_y),
                    ),
                    map_id: Some(snap.context.map_id),
                    label: "current position".to_owned(),
                })
            }
        }
    }

    async fn get_map_cached(&self, id: MapId) -> Result<MapInfo, ServiceError> {
        let key = format!("map:info:{id}");
        if let Some(json) = self.cache.get(&key).await
            && let Ok(info) = serde_json::from_str::<MapInfo>(&json)
        {
            return Ok(info);
        }
        let info = self.maps.get_map(id).await?;
        if let Ok(json) = serde_json::to_string(&info) {
            self.cache.set(&key, json, STATIC_TTL).await;
        }
        Ok(info)
    }

    async fn list_pois_cached(&self, map_id: MapId) -> Result<Vec<MapPoi>, ServiceError> {
        let key = format!("map:pois:{map_id}");
        if let Some(json) = self.cache.get(&key).await
            && let Ok(pois) = serde_json::from_str::<Vec<MapPoi>>(&json)
        {
            return Ok(pois);
        }
        let pois = self.maps.list_pois(map_id).await?;
        if let Ok(json) = serde_json::to_string(&pois) {
            self.cache.set(&key, json, STATIC_TTL).await;
        }
        Ok(pois)
    }
}

// ---------------------------------------------------------------------------
// Public navigation types — exposed so MCP layer can typed-encode.
// ---------------------------------------------------------------------------

/// One of three ways to identify a navigation target. Encoded as an MCP
/// `oneOf` schema by the stdio adapter.
#[derive(Debug, Clone)]
pub enum LocationRef {
    /// Literal continent-space coordinates. `map_id` is optional; if
    /// supplied, `find_nearby` uses it to scope the POI list. If
    /// omitted, the service falls back to the Mumble Link map id.
    Coords {
        coord: (f64, f64),
        map_id: Option<MapId>,
    },
    /// Named POI on a specific map (case-insensitive match).
    NamedPoi { map_id: MapId, name: String },
    /// "Here" — the player's current Mumble Link position.
    Here,
}

/// Filter applied to POI lists by `find_nearby`. `Any` matches every kind.
#[derive(Debug, Clone, Copy)]
pub enum NearbyFilter {
    Waypoint,
    Poi,
    Vista,
    HeroPoint,
    Task,
    Any,
}

impl NearbyFilter {
    fn matches(self, kind: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Waypoint => kind == "waypoint",
            // `poi` is a meta-bucket: anything that lives under
            // `points_of_interest` in the API and isn't a waypoint or
            // a vista (i.e. landmarks / unlocks).
            Self::Poi => matches!(kind, "landmark" | "unlock"),
            Self::Vista => kind == "vista",
            Self::HeroPoint => kind == "hero_point",
            Self::Task => kind == "task",
        }
    }

    /// Lowercase wire-format label matching the MCP tool parameter
    /// names, so we can echo the requested filter back to the caller
    /// inside [`NearbySearchResult`].
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Waypoint => "waypoint",
            Self::Poi => "poi",
            Self::Vista => "vista",
            Self::HeroPoint => "hero_point",
            Self::Task => "task",
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ResolvedLocation {
    pub coord: (f64, f64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_id: Option<MapId>,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MyLocationSnapshot {
    pub character_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profession_name: Option<String>,
    /// Character race resolved from `MumbleIdentity.race`. `None` if the
    /// identity block was empty (character select / very early load) or
    /// the byte was outside the known race table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub race_name: Option<String>,
    /// Mount state. `mount.index == 0` means "not on a mount"; otherwise
    /// `mount.name` carries the human-readable mount name. Always
    /// present in the JSON; `name` is `null` only for a future mount
    /// the byte→name table hasn't been updated for.
    pub mount: MountInfo,
    /// Map metadata (name, region, etc.) when the GW2 API lookup
    /// succeeded. None on transient API errors — we return position even
    /// if the metadata failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapSummary>,
    /// Raw map id from the Mumble Link context. Always present.
    pub map_id: MapId,
    pub position: (f64, f64),
    pub facing_bearing: Bearing16,
    pub ui_tick: u32,
    pub captured_at: DateTime<Utc>,
}

/// Mount index + resolved name. `index == 0` is the canonical "no mount"
/// state (sent by GW2 when the player is dismounted); the LLM can use
/// `name == "None"` as the dismounted indicator without having to know
/// the index→name mapping itself.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MountInfo {
    pub index: u8,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MapSummary {
    pub id: MapId,
    pub name: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_type: Option<String>,
    pub continent: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DirectionsResult {
    pub from: ResolvedLocation,
    pub to: ResolvedLocation,
    pub bearing: Bearing16,
    pub bearing_label: String,
    pub distance_units: f64,
    pub distance_meters: f64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NearbyResult {
    pub poi: MapPoi,
    pub bearing: Bearing16,
    pub bearing_label: String,
    pub distance_units: f64,
    pub distance_meters: f64,
}

/// Object wrapper around the `find_nearby` result list. MCP's
/// `structuredContent` schema rejects bare arrays, so the dispatcher
/// needs a single object at the top. The extra fields (`origin`,
/// `map_id`, `filter`, `total`) double as request echoes so the LLM
/// can reason about the response without re-reading its own call args.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NearbySearchResult {
    pub results: Vec<NearbyResult>,
    /// Resolved continent-coordinate origin the search was performed
    /// from (`(x, y)`).
    pub origin: (f64, f64),
    /// Map the search ran against — resolved from `around` or, for
    /// literal-coord requests, the player's current Mumble Link map.
    pub map_id: MapId,
    /// Echoes the request filter (`waypoint` / `poi` / `vista` /
    /// `hero_point` / `task` / `any`).
    pub filter: String,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FacingDescription {
    pub facing_bearing: Bearing16,
    pub facing_label: String,
    pub facing_long_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest_landmark: Option<MapPoi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest_landmark_distance_meters: Option<f64>,
    pub map_id: MapId,
    pub position: (f64, f64),
    pub ui_tick: u32,
}

fn map_summary(info: MapInfo) -> MapSummary {
    MapSummary {
        id: info.id,
        name: info.name,
        region: info.region_name,
        map_type: info.map_type,
        continent: info.continent_name,
    }
}

/// Convert a Mumble `f_avatar_front` 3D vector to a 16-point compass
/// bearing in the GW2 map plane.
///
/// GW2's world frame is X-east / Y-north / Z-up. Map space inverts the
/// Y axis (Y grows southward) — hence we flip the Y component before
/// running the bearing math, matching the convention `domain::bearing`
/// already encodes for 2D map coords.
fn facing_bearing(front: [f32; 3]) -> Bearing16 {
    let dx = f64::from(front[0]);
    // Mumble's avatar Y axis is map-north; in our Y-down map convention
    // that's a *negative* delta. Pretend the player is at (0,0) and
    // their facing vector lands them at (dx, -dy) one unit ahead.
    let dy_world = f64::from(front[1]);
    bearing((0.0, 0.0), (dx, -dy_world))
}

fn compass_index(b: Bearing16) -> u8 {
    match b {
        // `Same` falls through to 0 — it's only ever passed to
        // `compass_index_diff` which short-circuits before reading it,
        // so this collapse is intentional rather than a missing arm.
        Bearing16::N | Bearing16::Same => 0,
        Bearing16::NNE => 1,
        Bearing16::NE => 2,
        Bearing16::ENE => 3,
        Bearing16::E => 4,
        Bearing16::ESE => 5,
        Bearing16::SE => 6,
        Bearing16::SSE => 7,
        Bearing16::S => 8,
        Bearing16::SSW => 9,
        Bearing16::SW => 10,
        Bearing16::WSW => 11,
        Bearing16::W => 12,
        Bearing16::WNW => 13,
        Bearing16::NW => 14,
        Bearing16::NNW => 15,
    }
}

/// Minimum-wrap distance between two compass indices on the 16-wedge
/// circle. None when either side is `Same`.
fn compass_index_diff(a: Bearing16, b: Bearing16) -> Option<u8> {
    if matches!(a, Bearing16::Same) || matches!(b, Bearing16::Same) {
        return None;
    }
    let i = i16::from(compass_index(a));
    let j = i16::from(compass_index(b));
    let raw = (i - j).rem_euclid(16);
    Some(u8::try_from(raw.min(16 - raw)).unwrap_or(0))
}
