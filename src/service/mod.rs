//! Business logic. Orchestrates ports without ever knowing which concrete
//! adapter is plugged in. Caching policy lives here — adapters are dumb.

mod account;
mod catalogs;
mod character_build;
mod decode_build;
mod navigation;
mod reference;
mod search;
mod wiki;

pub use account::DailiesWhich;
pub use character_build::{CharacterBuildSnapshot, TabSelector};
pub use navigation::{
    DirectionsResult, FacingDescription, LocationRef, MapSummary, MyLocationSnapshot, NearbyFilter,
    ResolvedLocation,
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
// Profession byte → name map. Stable for the life of the game; carrying it
// in code rather than going through GW2 /v2/professions saves an HTTP round
// trip on every decode_build_code call.
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
}
