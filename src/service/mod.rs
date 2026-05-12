//! Business logic. Orchestrates ports without ever knowing which concrete
//! adapter is plugged in. Caching policy lives here — adapters are dumb.

mod account;
mod catalogs;
mod character_build;
mod decode_build;
mod maps;
mod navigation;
mod reference;
mod search;
mod wiki;

pub use account::{
    AccountAchievementsSnapshot, AccountBankSnapshot, AccountMasteriesSnapshot,
    AccountMaterialsSnapshot, BankItemEntry, CharacterInventorySnapshot, CharacterList,
    DailiesWhich, MasteryPointBalance, MaterialItemEntry, RefreshAccountCacheResult,
    WizardsVaultSnapshot,
};
pub use character_build::{CharacterBuildSnapshot, TabSelector};
use maps::RegionLookupError;
pub use maps::{
    MapNeighborsResponse, MapRef, MapRefResolved, RegionMapEntry, RegionMapList, RegionQuery,
    RouteEdge, RouteFilters, RouteFiltersSummary, RouteHop, RoutePath, RoutePlan, RoutePreference,
    expand_account_access,
};
pub use navigation::{
    DirectionsResult, FacingDescription, LocationRef, MapSummary, MountInfo, MyLocationSnapshot,
    NearbyFilter, NearbySearchResult, ResolvedLocation,
};
pub use wiki::wiki_page_url;

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::domain::ApiKey;
use crate::ports::{
    BuildCodeDecoder, BuildCodeError, Cache, CacheError, Clock, Gw2Api, Gw2ApiError, MapData,
    MapDataError, MapId, MumbleError, MumbleLink, SearchError, SearchIndex, Wiki, WikiError,
};

// Cache TTLs centralised so changes are atomic.
pub const STATIC_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365); // 1 year
pub const WIKI_TTL: Duration = Duration::from_secs(60 * 60 * 24); // 1 day
pub const WALLET_TTL: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Number of whole minutes from `from` to `to`. Positive when `to` is
/// in the future, negative when in the past. Saturates on overflow.
///
/// **Tool convention**: every response field that carries an absolute
/// `DateTime<Utc>` (`*_at`) ships a sibling relative delta computed via
/// this helper (`*_in_minutes` for future events / resets,
/// `*_minutes_ago` for past timestamps like `fetched_at`). LLMs are
/// notoriously bad at converting ISO 8601 strings into "in 38 minutes"
/// — pre-computing kills that hallucination class. New tools with
/// time-valued responses MUST follow this convention.
#[must_use]
pub(crate) fn minutes_between(
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> i64 {
    (to - from).num_minutes()
}

#[cfg(test)]
mod minutes_between_tests {
    use super::minutes_between;
    use chrono::{Duration, Utc};

    #[test]
    fn same_instant_is_zero() {
        let t = Utc::now();
        assert_eq!(minutes_between(t, t), 0);
    }

    #[test]
    fn future_is_positive() {
        let t = Utc::now();
        let later = t + Duration::minutes(42);
        assert_eq!(minutes_between(t, later), 42);
    }

    #[test]
    fn past_is_negative() {
        let t = Utc::now();
        let earlier = t - Duration::minutes(17);
        assert_eq!(minutes_between(t, earlier), -17);
    }

    #[test]
    fn truncates_partial_minutes() {
        // `num_minutes` rounds toward zero — 30s → 0 min, 89s → 1 min, etc.
        let t = Utc::now();
        let near = t + Duration::seconds(30);
        assert_eq!(minutes_between(t, near), 0);
        let further = t + Duration::seconds(89);
        assert_eq!(minutes_between(t, further), 1);
    }
}

/// Dailies roll over once per day. A 1-hour TTL keeps the worst case at
/// 24 fetches/day per server while still picking up the rollover within
/// an hour — short enough that the LLM never plans an obsolete routine
/// against last night's dailies.
pub const DAILIES_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Service errors are pass-through wrappers — the inner port errors are
/// already user-facing (see `Gw2ApiError`, `WikiError`, etc.). Adding a
/// "upstream X error: " prefix would just push the useful sentence further
/// down the user's eye-line.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{0}")]
    Gw2(#[from] Gw2ApiError),

    #[error("{0}")]
    Wiki(#[from] WikiError),

    #[error("{0}")]
    Cache(#[from] CacheError),

    #[error("{0}")]
    BuildCode(#[from] BuildCodeError),

    #[error("{0}")]
    Catalog(#[from] crate::ports::CatalogError),

    #[error("{0}")]
    MapData(#[from] MapDataError),

    /// Mumble Link state was requested but isn't available — surfaces
    /// the platform-specific reason (no GW2 client running, headless
    /// container, etc.) verbatim so the LLM can quote it back.
    #[error("{0}")]
    Mumble(#[from] MumbleError),

    /// A `LocationRef::NamedPoi` referenced a name that didn't match
    /// any POI on the given map. Carries the failed name so the LLM
    /// can correct or ask the user.
    #[error(
        "no POI named `{name}` found on map id {map_id}. Try `find_nearby` first to enumerate \
         POIs on this map."
    )]
    NoSuchPoi { map_id: MapId, name: String },

    /// `list_maps_in_region` failed: no region matched, multiple regions
    /// matched, or the continents catalogue is unreachable.
    #[error("{0}")]
    Region(#[from] RegionLookupError),

    #[error("{0}")]
    Search(#[from] SearchError),

    /// The server was started with `--no-search-index`, so the search tools
    /// have no backing store at all. Distinct from [`SearchError::NotIndexed`]
    /// (which means "still populating") so the LLM gets a different
    /// recovery hint.
    #[error(
        "search index is disabled on this server (started with --no-search-index). Use the typed \
         get_* tools with explicit ids."
    )]
    SearchDisabled,
}

/// Orchestrates GW2 / wiki access with a TTL cache in front.
///
/// Cloning is cheap — internals are `Arc`-wrapped trait objects.
#[derive(Clone)]
pub struct Service {
    gw2: Arc<dyn Gw2Api>,
    wiki: Arc<dyn Wiki>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
    build_decoder: Arc<dyn BuildCodeDecoder>,
    catalogs: Arc<crate::ports::CatalogRegistry>,
    /// Server-default API key. Tools that take an optional `api_key` arg
    /// fall back to this when the caller omits it. Set via the binary's
    /// `--api-key` flag / `GW2_API_KEY` env var.
    default_api_key: Option<ApiKey>,
    /// Live game-state reader. Plumbed in unconditionally; the binary
    /// wires a `StubMumbleLink` when no real reader is reachable so
    /// non-navigation tools keep working on headless hosts.
    mumble: Arc<dyn MumbleLink>,
    /// GW2 map / POI client.
    maps: Arc<dyn MapData>,
    /// Optional on-disk search index. `None` when started with
    /// `--no-search-index`, in which case the `search_*` methods all return
    /// [`ServiceError::SearchDisabled`].
    search_index: Option<Arc<dyn SearchIndex>>,
}

impl Service {
    // The constructor takes every port the service needs explicitly, so
    // callers (main.rs + tests) can see the full dependency surface at
    // wiring time. Refactoring into a builder buys little for the
    // friction it adds in the integration tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gw2: Arc<dyn Gw2Api>,
        wiki: Arc<dyn Wiki>,
        cache: Arc<dyn Cache>,
        clock: Arc<dyn Clock>,
        build_decoder: Arc<dyn BuildCodeDecoder>,
        catalogs: Arc<crate::ports::CatalogRegistry>,
        mumble: Arc<dyn MumbleLink>,
        maps: Arc<dyn MapData>,
    ) -> Self {
        Self {
            gw2,
            wiki,
            cache,
            clock,
            build_decoder,
            catalogs,
            default_api_key: None,
            mumble,
            maps,
            search_index: None,
        }
    }

    /// Set the default API key used when MCP callers omit `api_key`.
    /// Builder-style so wiring stays a one-liner.
    #[must_use]
    pub fn with_default_api_key(mut self, key: ApiKey) -> Self {
        self.default_api_key = Some(key);
        self
    }

    /// Wire a search index in. `None` = `--no-search-index` mode.
    #[must_use]
    pub fn with_search_index(mut self, idx: Arc<dyn SearchIndex>) -> Self {
        self.search_index = Some(idx);
        self
    }

    fn search_index(&self) -> Result<&Arc<dyn SearchIndex>, ServiceError> {
        self.search_index
            .as_ref()
            .ok_or(ServiceError::SearchDisabled)
    }

    /// Returns the server-default API key, if one was wired in.
    #[must_use]
    pub fn default_api_key(&self) -> Option<&ApiKey> {
        self.default_api_key.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Static GW2 byte/index → name maps. Stable for the life of the game (only
// change on new content releases); carrying them in code rather than going
// through GW2 `/v2/...` endpoints saves an HTTP round trip on every nav-tool
// / decode-build call.
// ---------------------------------------------------------------------------

pub(super) fn profession_byte_to_name(byte: u8) -> Option<&'static str> {
    match byte {
        1 => Some("Guardian"),
        2 => Some("Warrior"),
        3 => Some("Engineer"),
        4 => Some("Ranger"),
        5 => Some("Thief"),
        6 => Some("Elementalist"),
        7 => Some("Mesmer"),
        8 => Some("Necromancer"),
        9 => Some("Revenant"),
        _ => None,
    }
}

/// Mount index → display name, per the GW2 `MumbleLink` wiki table.
///
/// Index 0 ("None") means the player is *not* on a mount — we still
/// return `Some("None")` rather than `None` so callers can distinguish
/// "not mounted" from "future mount this table hasn't learned about
/// yet" (which returns `None`).
pub(super) fn mount_index_to_name(index: u8) -> Option<&'static str> {
    match index {
        0 => Some("None"),
        1 => Some("Jackal"),
        2 => Some("Griffon"),
        3 => Some("Springer"),
        4 => Some("Skimmer"),
        5 => Some("Raptor"),
        6 => Some("Roller Beetle"),
        7 => Some("Warclaw"),
        8 => Some("Skyscale"),
        9 => Some("Skiff"),
        10 => Some("Siege Turtle"),
        _ => None,
    }
}

/// Character race byte → display name. Comes from `MumbleIdentity.race`
/// (0-indexed, currently 0..=4 since GW2 launch).
pub(super) fn race_byte_to_name(byte: u8) -> Option<&'static str> {
    match byte {
        0 => Some("Asura"),
        1 => Some("Charr"),
        2 => Some("Human"),
        3 => Some("Norn"),
        4 => Some("Sylvari"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profession_byte_map_is_complete() {
        for byte in 1u8..=9 {
            assert!(profession_byte_to_name(byte).is_some());
        }
        assert!(profession_byte_to_name(0).is_none());
        assert!(profession_byte_to_name(10).is_none());
    }

    #[test]
    fn mount_index_map_covers_known_mounts() {
        // 0..=10 are all defined per the wiki (including 0 = "None" /
        // dismounted, which we deliberately return as Some).
        for idx in 0u8..=10 {
            assert!(
                mount_index_to_name(idx).is_some(),
                "mount index {idx} should be known"
            );
        }
        // Anything past 10 (until a future mount adds to the table) is
        // unknown — caller surfaces null rather than guessing.
        assert!(mount_index_to_name(11).is_none());
        assert!(mount_index_to_name(0xff).is_none());
    }

    #[test]
    fn race_byte_map_covers_known_races() {
        for byte in 0u8..=4 {
            assert!(
                race_byte_to_name(byte).is_some(),
                "race byte {byte} should be known"
            );
        }
        assert!(race_byte_to_name(5).is_none());
        assert!(race_byte_to_name(0xff).is_none());
    }
}
