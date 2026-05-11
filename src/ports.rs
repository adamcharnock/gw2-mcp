//! Trait definitions for everything the [service](crate::service) depends on.
//!
//! Every external concern enters the codebase through one of these traits.
//! Domain code never imports `adapters/` directly — only `ports`. This means
//! the service can be tested with mocks (or in-memory fakes) without ever
//! making a network call.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::{
    Account, AccountAchievement, AccountMastery, Achievement, AchievementId, ApiKey, BuildSlug,
    CharacterName, Currency, CurrencyId, Dungeon, Item, ItemId, Mastery, MasteryId, Raid, Region,
    SearchLimit, SearchQuery, SearchResult, Skill, SkillId, Specialization, SpecializationId,
    Trait, TraitId, WalletEntry, WizardsVaultTrack,
};

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Source of the current time.
///
/// Inject a `MockClock` in tests to make any time-dependent behaviour
/// deterministic. The trait is `Send + Sync` so it can live behind an `Arc`
/// across async tasks.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("failed to serialise cache value: {0}")]
    Serialise(#[source] serde_json::Error),

    #[error("failed to deserialise cache value: {0}")]
    Deserialise(#[source] serde_json::Error),
}

/// A simple TTL cache port.
///
/// Values are stored as JSON strings — keeping the interface object-safe
/// and avoiding generic gymnastics. Adapters are free to use any in-memory
/// representation internally.
#[async_trait]
pub trait Cache: Send + Sync + 'static {
    /// Insert `value` under `key` with the given TTL.
    async fn set(&self, key: &str, value: String, ttl: Duration);

    /// Fetch the value at `key`, if present and not expired.
    async fn get(&self, key: &str) -> Option<String>;

    /// Remove a key. No-op if absent.
    async fn delete(&self, key: &str);

    /// Number of entries currently cached. Mostly useful for tests.
    async fn len(&self) -> usize;

    /// Whether the cache is empty. Mostly useful for tests.
    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

// ---------------------------------------------------------------------------
// GW2 API
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum Gw2ApiError {
    #[error("could not reach the Guild Wars 2 API ({0}) — check your network and try again")]
    Transport(String),

    /// The GW2 API responded but with an error. Carries the most useful
    /// info we can extract: the inner `text` payload (GW2 errors are
    /// shaped `{"text":"..."}`) plus the raw status for diagnostics.
    #[error("Guild Wars 2 API error ({status}): {message}")]
    Upstream { status: u16, message: String },

    #[error(
        "could not understand the Guild Wars 2 API response — this usually means the API \
         changed shape; please open an issue. Detail: {0}"
    )]
    Decode(String),

    #[error(
        "the Guild Wars 2 API rejected this key. Verify the key at \
         https://account.arena.net/applications and check it has the required scopes \
         (`account` + `wallet` for get_wallet; `account` + `characters` + `builds` for \
         get_character_build)."
    )]
    Unauthorized,

    /// 403 with body matching `requires scope`. The key is otherwise
    /// valid but lacks the named scope. We carry the scope name so the
    /// LLM can tell the user exactly which checkbox to tick.
    #[error(
        "the Guild Wars 2 API rejected this key for missing scope: {needed}. Generate a new key \
         at https://account.arena.net/applications and ensure that scope is checked."
    )]
    MissingScope { needed: String },

    #[error("character `{name}` does not exist on this account")]
    CharacterNotFound { name: String },

    /// 429 from the GW2 API. Carries the parsed `Retry-After` value if
    /// the upstream supplied one — surfaced to the user so they don't
    /// retry too eagerly.
    #[error("{}", rate_limited_message(*.0))]
    RateLimited(Option<std::time::Duration>),
}

/// Render the rate-limit message with a concrete retry hint when we
/// have one. Defined here (not as an `impl Display` body) so the
/// `#[error]` attribute above stays a single-line literal.
fn rate_limited_message(retry_after: Option<std::time::Duration>) -> String {
    match retry_after {
        Some(d) if d.as_secs() > 0 => format!(
            "the Guild Wars 2 API rate limit was hit. Try again in {} seconds.",
            d.as_secs()
        ),
        _ => "the Guild Wars 2 API rate limit was hit. Try again in a few seconds.".into(),
    }
}

/// Read-only Guild Wars 2 API client.
#[async_trait]
pub trait Gw2Api: Send + Sync + 'static {
    /// `/v2/account/wallet` — requires an API key with `wallet` scope.
    async fn fetch_wallet(&self, key: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError>;

    /// `/v2/currencies` (no parameters) — returns every known currency id.
    async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError>;

    /// `/v2/currencies?ids=…` — fetch metadata for specific ids.
    async fn fetch_currencies(
        &self,
        ids: &[CurrencyId],
    ) -> Result<BTreeMap<CurrencyId, Currency>, Gw2ApiError>;

    /// `/v2/skills?ids=…`
    async fn fetch_skills(&self, ids: &[SkillId]) -> Result<BTreeMap<SkillId, Skill>, Gw2ApiError>;

    /// `/v2/traits?ids=…`
    async fn fetch_traits(&self, ids: &[TraitId]) -> Result<BTreeMap<TraitId, Trait>, Gw2ApiError>;

    /// `/v2/specializations?ids=…`
    async fn fetch_specializations(
        &self,
        ids: &[SpecializationId],
    ) -> Result<BTreeMap<SpecializationId, Specialization>, Gw2ApiError>;

    /// `/v2/items?ids=…`
    async fn fetch_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, Gw2ApiError>;

    /// `/v2/achievements?ids=…`
    async fn fetch_achievements(
        &self,
        ids: &[AchievementId],
    ) -> Result<BTreeMap<AchievementId, Achievement>, Gw2ApiError>;

    /// `/v2/skills` (no `?ids=`) returns the full id list. Used by the
    /// Tier-6C indexer to enumerate every skill before chunked fetching.
    async fn fetch_all_skill_ids(&self) -> Result<Vec<SkillId>, Gw2ApiError>;

    /// `/v2/traits` — full id list.
    async fn fetch_all_trait_ids(&self) -> Result<Vec<TraitId>, Gw2ApiError>;

    /// `/v2/specializations` — full id list.
    async fn fetch_all_specialization_ids(&self) -> Result<Vec<SpecializationId>, Gw2ApiError>;

    /// `/v2/items` — full id list. **Heavy**: ~85k entries, ~1MB response.
    async fn fetch_all_item_ids(&self) -> Result<Vec<ItemId>, Gw2ApiError>;

    /// `/v2/achievements` — full id list.
    async fn fetch_all_achievement_ids(&self) -> Result<Vec<AchievementId>, Gw2ApiError>;

    /// `/v2/build` — current game build number. One cheap call; used by the
    /// indexer to detect game patches and trigger re-indexing.
    async fn fetch_build(&self) -> Result<u32, Gw2ApiError>;

    /// `/v2/characters/:name/buildtabs?tabs=all` — requires `builds` scope.
    /// Returned as raw JSON values (variants too rich to be worth typing).
    async fn fetch_buildtabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError>;

    /// `/v2/characters/:name/equipmenttabs?tabs=all` — requires `builds` scope.
    async fn fetch_equipmenttabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError>;

    // ---------------------------------------------------------------------
    // Tier 6A — PvE coaching surface (account state, achievements,
    // masteries, weekly clears, dailies).
    // ---------------------------------------------------------------------

    /// `/v2/account` — requires `account` scope.
    async fn fetch_account(&self, key: &ApiKey) -> Result<Account, Gw2ApiError>;

    /// `/v2/characters` — requires `characters` scope. Returns just the
    /// character names (cheap; the heavy `/v2/characters/:name` family is
    /// out of scope here).
    async fn fetch_characters_list(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError>;

    /// `/v2/account/achievements` — requires `progression` scope. Heavy:
    /// 2000-3000 entries on a long-lived account. Service projects this
    /// down (drops completed + not-started) before returning to MCP
    /// callers by default.
    async fn fetch_account_achievements(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<AccountAchievement>, Gw2ApiError>;

    /// `/v2/account/masteries` — requires `progression` scope.
    async fn fetch_account_masteries(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<AccountMastery>, Gw2ApiError>;

    /// `/v2/masteries` — public, no key. Returns the full id list.
    async fn fetch_all_mastery_ids(&self) -> Result<Vec<MasteryId>, Gw2ApiError>;

    /// `/v2/masteries?ids=…` — public, no key.
    async fn fetch_masteries(
        &self,
        ids: &[MasteryId],
    ) -> Result<BTreeMap<MasteryId, Mastery>, Gw2ApiError>;

    /// `/v2/account/raids` — requires `progression` scope. Returns the raid
    /// encounter ids cleared this reset week (e.g. `vale_guardian`,
    /// `sabetha`).
    async fn fetch_account_raids(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError>;

    /// `/v2/raids` — public, no key. Returns the list of raid release ids
    /// (e.g. `forsaken_thicket`, `bastion_of_the_penitent`).
    async fn fetch_all_raid_ids(&self) -> Result<Vec<String>, Gw2ApiError>;

    /// `/v2/raids?ids=…` — public, no key. Returns the wing + encounter
    /// breakdown for each raid release.
    async fn fetch_raids(&self, ids: &[String]) -> Result<BTreeMap<String, Raid>, Gw2ApiError>;

    /// `/v2/account/dungeons` — requires `progression` scope. Returns the
    /// dungeon-path ids cleared today (resets daily, **not** weekly).
    async fn fetch_account_dungeons(&self, key: &ApiKey) -> Result<Vec<String>, Gw2ApiError>;

    /// `/v2/dungeons` — public, no key. Returns the list of dungeon ids
    /// (e.g. `ascalon_catacombs`).
    async fn fetch_all_dungeon_ids(&self) -> Result<Vec<String>, Gw2ApiError>;

    /// `/v2/dungeons?ids=…` — public, no key. Returns the path breakdown
    /// for each dungeon.
    async fn fetch_dungeons(
        &self,
        ids: &[String],
    ) -> Result<BTreeMap<String, Dungeon>, Gw2ApiError>;

    /// `/v2/account/wizardsvault/daily` — per-account daily Wizard's
    /// Vault objectives. Authenticated (scopes: `account` +
    /// `progression`). Replaces the deprecated
    /// `/v2/achievements/daily` endpoint, which returns 503 since the
    /// Vault launch.
    async fn fetch_wizards_vault_daily(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError>;

    /// `/v2/account/wizardsvault/weekly` — per-account weekly Wizard's
    /// Vault objectives. Same shape and scopes as `_daily`.
    async fn fetch_wizards_vault_weekly(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError>;

    /// `/v2/account/wizardsvault/special` — per-account special
    /// (limited-time / seasonal) Vault objectives. Same shape as the
    /// other two.
    async fn fetch_wizards_vault_special(
        &self,
        key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError>;

    /// `/v2/continents/{continent_id}/floors/{floor_id}/regions` —
    /// every region on the given (continent, floor), each with its
    /// `maps` table. Public, no key. Used to back `list_maps_in_region`.
    async fn fetch_regions_on_floor(
        &self,
        continent_id: u32,
        floor_id: u32,
    ) -> Result<BTreeMap<u32, Region>, Gw2ApiError>;
}

// ---------------------------------------------------------------------------
// Curated build catalogs (Phase B)
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("no such build source: {0}")]
    NoSuchSource(String),

    // `source` is reserved by thiserror; use `source_name` everywhere.
    #[error("build not found in {source_name}: {slug}")]
    NotFound { source_name: String, slug: String },

    #[error("transport error from {source_name}: {message}")]
    Transport {
        source_name: String,
        message: String,
    },

    #[error("parse error from {source_name}: {message}")]
    Parse {
        source_name: String,
        message: String,
    },
}

/// What to filter the catalog listing by. None means "no filter on that
/// dimension". Sources interpret unknown values by returning an empty
/// listing rather than erroring.
#[derive(Debug, Clone, Default)]
pub struct CatalogFilter {
    pub profession: Option<String>,
    pub gamemode: Option<String>,
    pub limit: Option<u32>,
}

/// Lightweight summary returned from a catalog listing — enough for an
/// LLM to pick which build to fetch in detail.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
pub struct BuildSummary {
    pub slug: String,
    pub title: String,
    pub profession: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elite_spec: Option<String>,
    pub role: String,
    pub gamemode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    pub source: String,
    pub source_url: String,
}

/// Detailed build view. We keep this loose (`details: serde_json::Value`)
/// because each catalog's data is shaped differently and the LLM is the
/// consumer — typing every variant would be premature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, schemars::JsonSchema)]
pub struct BuildDetail {
    pub summary: BuildSummary,
    #[schemars(schema_with = "opaque_value_schema")]
    pub details: serde_json::Value,
    /// Plain-text description / rotation notes if the source provides them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Build chat code, if the source publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_code: Option<String>,
}

/// Schema override for fields backed by `serde_json::Value` (or other
/// opaque payloads). schemars renders `Value` as the JSON Schema boolean
/// `true`, which is semantically "matches anything" — valid per the spec,
/// but rejected by `zod`-style MCP-client validators (Claude Code being
/// one) that expect every subschema to be an object. Emit `{}` instead:
/// same meaning, valid for those validators too.
pub fn opaque_value_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({})
}

/// Schema override for fields of type `Vec<serde_json::Value>`. Same
/// reasoning as [`opaque_value_schema`], but emits an array whose items
/// schema is `{}` instead of `true`.
pub fn opaque_value_array_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "array", "items": {} })
}

#[async_trait]
pub trait BuildCatalog: Send + Sync + 'static {
    /// Stable identifier — used as the `source` field in MCP tool calls.
    fn name(&self) -> &'static str;

    async fn list(&self, filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError>;

    /// Fetch a single build by its (already-validated) slug. The
    /// [`BuildSlug`] newtype guarantees the value is safe to interpolate
    /// into a URL path (no `..`, no newlines, ≤ 256 chars, all-lowercase).
    async fn fetch(&self, slug: &BuildSlug) -> Result<BuildDetail, CatalogError>;
}

/// Tiny registry so `Service` can route by source name without owning a
/// fixed set of adapters.
pub struct CatalogRegistry {
    catalogs: std::collections::BTreeMap<&'static str, std::sync::Arc<dyn BuildCatalog>>,
}

impl CatalogRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            catalogs: std::collections::BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with(mut self, c: std::sync::Arc<dyn BuildCatalog>) -> Self {
        self.catalogs.insert(c.name(), c);
        self
    }

    pub fn get(&self, name: &str) -> Option<&std::sync::Arc<dyn BuildCatalog>> {
        self.catalogs.get(name)
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.catalogs.keys().copied().collect()
    }
}

impl Default for CatalogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Build code decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BuildCodeError {
    #[error("malformed chat code: {0}")]
    Malformed(String),
}

/// Decodes GW2 build template chat codes (`[&Dw...=]`) into structured
/// data. Behind a port so we can swap the underlying decoder later
/// (currently `chatr`).
pub trait BuildCodeDecoder: Send + Sync + 'static {
    fn decode(
        &self,
        code: &crate::domain::BuildChatCode,
    ) -> Result<serde_json::Value, BuildCodeError>;
}

// ---------------------------------------------------------------------------
// Wiki
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum WikiError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("wiki API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("failed to decode wiki response: {0}")]
    Decode(String),
}

/// Read-only Guild Wars 2 wiki client.
#[async_trait]
pub trait Wiki: Send + Sync + 'static {
    async fn search(
        &self,
        query: &SearchQuery,
        limit: SearchLimit,
    ) -> Result<Vec<SearchResult>, WikiError>;

    /// Returns the leading prose extract for a page. Empty string if missing.
    async fn fetch_extract(&self, title: &str) -> Result<String, WikiError>;
}

// ---------------------------------------------------------------------------
// Map data (POIs, waypoints, hero points, …) — Tier 6B navigation.
// ---------------------------------------------------------------------------

/// Numeric GW2 map id (e.g. 15 = Queensdale). Newtype-light: maps and
/// continents are open-coded as `u32` everywhere in the GW2 API; we
/// don't need value-class invariants beyond "non-zero".
pub type MapId = u32;

#[derive(Debug, Error)]
pub enum MapDataError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("GW2 map API returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("could not decode GW2 map response: {0}")]
    Decode(String),

    #[error("no such map id: {0}")]
    NotFound(MapId),
}

/// Metadata for a single GW2 map. Only the fields we actually use for
/// navigation are pulled out — the raw payload is otherwise enormous.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct MapInfo {
    pub id: MapId,
    pub name: String,
    /// Map kind — `Public`, `Instance`, `Pvp`, `WvW`, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level: Option<u32>,
    pub default_floor: i32,
    pub region_id: u32,
    pub region_name: String,
    pub continent_id: u32,
    pub continent_name: String,
    /// `[[x_min,y_min],[x_max,y_max]]` in continent space. Used to pin
    /// POIs onto navigable coords.
    pub continent_rect: [[f64; 2]; 2],
    pub map_rect: [[f64; 2]; 2],
}

/// Map point of interest. The GW2 API splits these across several arrays
/// per `/v2/continents/.../regions/.../maps/.../{id}` (`points_of_interest`,
/// `tasks`, `skill_challenges`, `sectors`); we flatten them into one
/// shape so the LLM doesn't have to special-case.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct MapPoi {
    pub id: u64,
    pub name: String,
    /// `waypoint`, `landmark`, `vista`, `unlock`, `hero_point`, or `task`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Continent-space (x, y) — same coordinate frame the Mumble Link
    /// `context.player_x` / `context.player_y` reports.
    pub coord: (f64, f64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_link: Option<String>,
    pub floor: i32,
}

#[async_trait]
pub trait MapData: Send + Sync + 'static {
    async fn get_map(&self, id: MapId) -> Result<MapInfo, MapDataError>;
    async fn list_pois(&self, map_id: MapId) -> Result<Vec<MapPoi>, MapDataError>;
}

// ---------------------------------------------------------------------------
// Mumble Link — live in-game state read from a memory-mapped region the GW2
// client writes every frame. Per CLAUDE.rust.md the trait lives here; concrete
// adapters (`StubMumbleLink`, file-backed `FileMumbleLink`, Windows-native
// `WindowsMumbleLink`) live in `adapters/mumble_link.rs`.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MumbleError {
    /// The shared-memory region was found but contains no live game data
    /// (`ui_tick == 0`), or the region itself wasn't found at all.
    #[error(
        "Mumble Link is not connected — start Guild Wars 2 on the same machine running this MCP \
         server, log a character in, and try again. (Detail: {0})"
    )]
    NotConnected(String),

    /// We're on a platform with no implementation. The error message
    /// names the platform and gives the user something actionable.
    #[error("Mumble Link is not supported in this configuration: {0}")]
    Unsupported(String),

    #[error("Mumble Link I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Mumble Link decode error: {0}")]
    Decode(String),
}

/// Read-only port for the live Mumble Link state.
pub trait MumbleLink: Send + Sync + 'static {
    fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError>;
}

/// What we hand back to the service layer. This is *not* the raw 5876-
/// byte struct — only the fields we actually use, with the GW2 context
/// block and identity JSON pre-parsed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MumbleSnapshot {
    /// Mumble protocol version (always 1 for GW2).
    pub ui_version: u32,
    /// Frame counter; if 0 the client isn't writing yet.
    pub ui_tick: u32,
    /// 3D world position in metres, Z-up.
    pub avatar_position: [f32; 3],
    /// Unit vector for the direction the avatar is facing.
    pub avatar_front: [f32; 3],
    /// 3D camera position in metres.
    pub camera_position: [f32; 3],
    /// Unit vector for the camera-look direction.
    pub camera_front: [f32; 3],
    /// Parsed `identity` JSON (character name, profession id, …).
    pub identity: MumbleIdentity,
    /// Parsed GW2-specific context block.
    pub context: MumbleContext,
}

/// `identity` JSON shape published by GW2. Fields are left optional
/// because Mumble Link's identity is a freeform string and the publisher
/// has historically renamed keys across patches; we'd rather render
/// `null` than reject a snapshot.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MumbleIdentity {
    #[serde(default)]
    pub name: Option<String>,
    /// 1..=9 — see profession byte → name map elsewhere.
    #[serde(default)]
    pub profession: Option<u8>,
    /// Elite specialisation id (0 if none equipped).
    #[serde(default)]
    pub spec: Option<u32>,
    /// Race id (1..=5 in current patches).
    #[serde(default)]
    pub race: Option<u8>,
    #[serde(default)]
    pub map_id: Option<u32>,
    #[serde(default)]
    pub world_id: Option<u64>,
    #[serde(default)]
    pub team_color_id: Option<u32>,
    #[serde(default)]
    pub commander: Option<bool>,
    #[serde(default)]
    pub fov: Option<f32>,
    #[serde(default)]
    pub uisz: Option<u8>,
}

/// GW2 binary `context` block — the only bit of "context" we care about.
/// `player_x` and `player_y` are the **2D map coordinates** the LLM wants
/// for navigation; they are *not* the same as the 3D position above.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MumbleContext {
    pub server_address: [u8; 28],
    pub map_id: u32,
    pub map_type: u32,
    pub shard_id: u32,
    pub instance: u32,
    pub build_id: u32,
    pub ui_state: u32,
    pub compass_width: u16,
    pub compass_height: u16,
    pub compass_rotation: f32,
    /// 2D map x — the coord to use for navigation.
    pub player_x: f32,
    /// 2D map y — the coord to use for navigation.
    pub player_y: f32,
    pub map_center_x: f32,
    pub map_center_y: f32,
    pub map_scale: f32,
    pub process_id: u32,
    pub mount_index: u8,
}

// ---------------------------------------------------------------------------
// Search index (Tier 6C) — on-disk fuzzy name search across the GW2 reference
// corpus (skills/traits/specializations/items/achievements). Decoupled from
// the GW2 API port so the index can be backed by SQLite, an in-process
// in-memory store (for tests), or a future Tantivy/Meilisearch process.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SearchError {
    /// The requested entity kind has no rows yet — happens when the
    /// background indexer hasn't finished populating, or when items are
    /// disabled (`--with-items` is opt-in).
    #[error(
        "the {kind} index is still populating, or has not been built yet. Try again shortly, or \
         use the typed get_* tool with explicit ids."
    )]
    NotIndexed { kind: &'static str },

    /// A refresh is in flight — a writer is mutating the index. We surface
    /// this as a typed error rather than blocking the search call so the
    /// caller can decide whether to back off.
    #[error("the search index is being refreshed; retry in a few seconds")]
    IndexLocked,

    #[error("search storage error: {0}")]
    Storage(String),

    #[error("search internal error: {0}")]
    Internal(String),
}

/// Lightweight result row for skill searches. Carries enough for the LLM to
/// identify the hit + decide whether to round-trip via `get_skills` for the
/// full payload. `raw_json` is *not* included — that's what `get_skills` is
/// for.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SkillRef {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub professions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weapon_type: Option<String>,
    /// FTS5 BM25 rank. Lower is better (negated by `SQLite` to make ORDER BY
    /// `rank` ASC return best-first). Returned for diagnostics — clients can
    /// ignore.
    #[serde(default)]
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct TraitRef {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Specialization id this trait belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specialization: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default)]
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SpecRef {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profession: Option<String>,
    #[serde(default)]
    pub elite: bool,
    #[serde(default)]
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ItemRef {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rarity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_class: Option<String>,
    #[serde(default)]
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct AchievementRef {
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achievement_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<u32>,
    #[serde(default)]
    pub score: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SkillSearchFilter {
    pub profession: Option<String>,
    pub slot: Option<String>,
    pub weapon_type: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TraitSearchFilter {
    pub specialization: Option<u32>,
    pub tier: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct SpecSearchFilter {
    pub profession: Option<String>,
    pub elite: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ItemSearchFilter {
    pub item_type: Option<String>,
    pub rarity: Option<String>,
    pub min_level: Option<u32>,
    pub max_level: Option<u32>,
    pub weight_class: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AchievementSearchFilter {
    pub achievement_type: Option<String>,
}

/// Status of one entity kind in the index — used by `get_index_status` to
/// expose "still populating" state to MCP callers.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct KindStatus {
    /// Stable identifier: `"skills"`, `"traits"`, `"specializations"`,
    /// `"items"`, `"achievements"`.
    pub name: String,
    /// Total ids reported by `/v2/<kind>` (the upstream count we'd like to
    /// reach). Zero if unknown.
    pub total: u32,
    /// Rows currently present in the index.
    pub indexed: u32,
    /// Unix timestamp (seconds) of the last successful refresh, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed_at: Option<i64>,
    /// GW2 build number this kind was indexed against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_number: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct IndexStatus {
    pub kinds: Vec<KindStatus>,
}

/// Local search index over the GW2 reference corpus. Implementations are
/// free to choose any backing store (`SQLite` + FTS5, in-memory, future
/// Tantivy/Meilisearch); the service only sees this trait.
#[async_trait]
pub trait SearchIndex: Send + Sync + 'static {
    async fn search_skills(
        &self,
        q: &str,
        limit: u32,
        filter: SkillSearchFilter,
    ) -> Result<Vec<SkillRef>, SearchError>;

    async fn search_traits(
        &self,
        q: &str,
        limit: u32,
        filter: TraitSearchFilter,
    ) -> Result<Vec<TraitRef>, SearchError>;

    async fn search_specializations(
        &self,
        q: &str,
        limit: u32,
        filter: SpecSearchFilter,
    ) -> Result<Vec<SpecRef>, SearchError>;

    async fn search_items(
        &self,
        q: &str,
        limit: u32,
        filter: ItemSearchFilter,
    ) -> Result<Vec<ItemRef>, SearchError>;

    async fn search_achievements(
        &self,
        q: &str,
        limit: u32,
        filter: AchievementSearchFilter,
    ) -> Result<Vec<AchievementRef>, SearchError>;

    async fn upsert_skills(&self, skills: &[Skill], build: u32) -> Result<(), SearchError>;
    async fn upsert_traits(&self, traits: &[Trait], build: u32) -> Result<(), SearchError>;
    async fn upsert_specializations(
        &self,
        specs: &[Specialization],
        build: u32,
    ) -> Result<(), SearchError>;
    async fn upsert_items(&self, items: &[Item], build: u32) -> Result<(), SearchError>;
    async fn upsert_achievements(
        &self,
        achievements: &[Achievement],
        build: u32,
    ) -> Result<(), SearchError>;

    /// The build number stamped into the index meta table at the end of the
    /// last full refresh. `None` means the index has never finished a full
    /// refresh — the indexer should populate.
    async fn build_number(&self) -> Result<Option<u32>, SearchError>;
    async fn set_build_number(&self, build: u32) -> Result<(), SearchError>;

    async fn index_status(&self) -> Result<IndexStatus, SearchError>;
}
