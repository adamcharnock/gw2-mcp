//! Argument-parsing helpers + cursor codec + `CallError`.
//!
//! These were sprinkled across `mod.rs` between the dispatch impls; with
//! ~30 tools each parsing a handful of fields, the helpers add up. Moving
//! them here lets the parent module focus on routing.
//!
//! Visibility: `pub(super)` on everything `mod.rs` references; the
//! private items (`SEARCH_*` constants used only by the search-arg
//! helpers) stay file-local.

use crate::ports::CatalogFilter;
use crate::service::TabSelector;

/// Parse a `LocationRef` from MCP args. The MCP schema is a `oneOf`:
/// `{coords:[x,y], map_id?: int}` | `{poi_name: str, map_id: int}` |
/// `{here: true}`. We recognise each shape by its discriminating key
/// rather than requiring the LLM to set a `kind` field — schemas with
/// implicit discrimination are friendlier for tool-calling models.
pub(super) fn parse_location_ref(
    v: Option<&serde_json::Value>,
    field: &'static str,
) -> Result<crate::service::LocationRef, CallError> {
    let v = v.ok_or(CallError::MissingArg(field))?;
    let Some(obj) = v.as_object() else {
        return Err(CallError::BadArg {
            name: field,
            expected: "object: {coords:[x,y]}, {poi_name, map_id}, or {here:true}",
        });
    };

    if obj.get("here").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(crate::service::LocationRef::Here);
    }

    if let Some(arr) = obj.get("coords").and_then(serde_json::Value::as_array) {
        if arr.len() != 2 {
            return Err(CallError::BadArg {
                name: field,
                expected: "coords must be a 2-element [x, y] array of numbers",
            });
        }
        let x = arr[0].as_f64().ok_or(CallError::BadArg {
            name: field,
            expected: "coords[0] must be a number",
        })?;
        let y = arr[1].as_f64().ok_or(CallError::BadArg {
            name: field,
            expected: "coords[1] must be a number",
        })?;
        let map_id = obj
            .get("map_id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        return Ok(crate::service::LocationRef::Coords {
            coord: (x, y),
            map_id,
        });
    }

    if let Some(name) = obj.get("poi_name").and_then(|v| v.as_str()) {
        let map_id = obj
            .get("map_id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(CallError::BadArg {
                name: field,
                expected: "poi_name requires an accompanying integer map_id",
            })?;
        return Ok(crate::service::LocationRef::NamedPoi {
            map_id,
            name: name.to_owned(),
        });
    }

    Err(CallError::BadArg {
        name: field,
        expected: "object: {coords:[x,y]}, {poi_name, map_id}, or {here:true}",
    })
}

/// Parse the optional `prefer` arg for `plan_route`.
pub(super) fn parse_route_preference(
    v: Option<&serde_json::Value>,
) -> Result<crate::service::RoutePreference, CallError> {
    use crate::service::RoutePreference;
    let Some(v) = v else {
        return Ok(RoutePreference::default());
    };
    if v.is_null() {
        return Ok(RoutePreference::default());
    }
    let Some(s) = v.as_str() else {
        return Err(CallError::BadArg {
            name: "prefer",
            expected: "string: \"shortest\" | \"walking\" | \"gates\"",
        });
    };
    Ok(match s {
        "shortest" | "" => RoutePreference::Shortest,
        "walking" => RoutePreference::Walking,
        "gates" => RoutePreference::Gates,
        _ => {
            return Err(CallError::BadArg {
                name: "prefer",
                expected: "string: \"shortest\" | \"walking\" | \"gates\"",
            });
        }
    })
}

/// Parse the optional `exclude_connections` array for `plan_route`.
pub(super) fn parse_excluded_connections(
    v: Option<&serde_json::Value>,
) -> Result<std::collections::HashSet<crate::domain::ConnectionType>, CallError> {
    use crate::domain::ConnectionType;
    let mut out = std::collections::HashSet::new();
    let Some(v) = v else {
        return Ok(out);
    };
    if v.is_null() {
        return Ok(out);
    }
    let Some(arr) = v.as_array() else {
        return Err(CallError::BadArg {
            name: "exclude_connections",
            expected: "array of strings",
        });
    };
    for entry in arr {
        let Some(s) = entry.as_str() else {
            return Err(CallError::BadArg {
                name: "exclude_connections",
                expected: "array of strings",
            });
        };
        let parsed = match s {
            "physical" => ConnectionType::Physical,
            "asura_gate" => ConnectionType::AsuraGate,
            "story_gate" => ConnectionType::StoryGate,
            "guild_hall" => ConnectionType::GuildHall,
            _ => {
                return Err(CallError::BadArg {
                    name: "exclude_connections",
                    expected: "values from: physical | asura_gate | story_gate | guild_hall",
                });
            }
        };
        out.insert(parsed);
    }
    Ok(out)
}

/// Parse the optional `player_access` array. Returns:
/// - `Ok(None)` — argument absent / null (the dispatcher should
///   attempt auto-fetch from /v2/account).
/// - `Ok(Some(set))` — argument present, possibly empty (caller
///   explicitly opted out of access filtering by passing `[]`).
pub(super) fn parse_player_access(
    v: Option<&serde_json::Value>,
) -> Result<Option<std::collections::HashSet<crate::domain::Expansion>>, CallError> {
    use crate::domain::Expansion;
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(arr) = v.as_array() else {
        return Err(CallError::BadArg {
            name: "player_access",
            expected: "array of expansion strings (snake_case enum values)",
        });
    };
    let mut out = std::collections::HashSet::new();
    for entry in arr {
        let Some(s) = entry.as_str() else {
            return Err(CallError::BadArg {
                name: "player_access",
                expected: "array of expansion strings",
            });
        };
        let parsed = match s {
            "core" => Expansion::Core,
            "living_world_season1" => Expansion::LivingWorldSeason1,
            "living_world_season2" => Expansion::LivingWorldSeason2,
            "heart_of_thorns" => Expansion::HeartOfThorns,
            "living_world_season3" => Expansion::LivingWorldSeason3,
            "path_of_fire" => Expansion::PathOfFire,
            "living_world_season4" => Expansion::LivingWorldSeason4,
            "icebrood_saga" => Expansion::IcebroodSaga,
            "end_of_dragons" => Expansion::EndOfDragons,
            "secrets_of_the_obscure" => Expansion::SecretsOfTheObscure,
            "janthir_wilds" => Expansion::JanthirWilds,
            "castora" => Expansion::Castora,
            "festival" => Expansion::Festival,
            _ => {
                return Err(CallError::BadArg {
                    name: "player_access",
                    expected: "snake_case expansion names (core, heart_of_thorns, path_of_fire, end_of_dragons, secrets_of_the_obscure, janthir_wilds, castora, ...)",
                });
            }
        };
        out.insert(parsed);
    }
    Ok(Some(out))
}

/// Parse a `MapRef` from MCP args — used by `plan_route` for both
/// `from` and `to`. The MCP schema is a `oneOf`:
/// `{id: int}` | `{name: str}` | `{here: true}`. As with
/// [`parse_location_ref`], we recognise each shape by its
/// discriminating key.
pub(super) fn parse_map_ref(
    v: Option<&serde_json::Value>,
    field: &'static str,
) -> Result<crate::service::MapRef, CallError> {
    let v = v.ok_or(CallError::MissingArg(field))?;
    let Some(obj) = v.as_object() else {
        return Err(CallError::BadArg {
            name: field,
            expected: "object: {id: int}, {name: string}, or {here: true}",
        });
    };
    if obj.get("here").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(crate::service::MapRef::Here);
    }
    if let Some(id) = obj.get("id").and_then(serde_json::Value::as_u64) {
        let id = u32::try_from(id).map_err(|_| CallError::BadArg {
            name: field,
            expected: "id must fit in u32",
        })?;
        return Ok(crate::service::MapRef::Id(id));
    }
    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CallError::BadArg {
                name: field,
                expected: "name must be a non-empty string",
            });
        }
        return Ok(crate::service::MapRef::Name(trimmed.to_owned()));
    }
    Err(CallError::BadArg {
        name: field,
        expected: "object: {id: int}, {name: string}, or {here: true}",
    })
}

pub(super) fn parse_nearby_filter(
    v: Option<&serde_json::Value>,
) -> Result<crate::service::NearbyFilter, CallError> {
    use crate::service::NearbyFilter;
    let Some(s) = v.and_then(|v| v.as_str()) else {
        return Ok(NearbyFilter::Any);
    };
    Ok(match s {
        "waypoint" => NearbyFilter::Waypoint,
        "poi" => NearbyFilter::Poi,
        "vista" => NearbyFilter::Vista,
        "hero_point" => NearbyFilter::HeroPoint,
        "task" => NearbyFilter::Task,
        "any" | "" => NearbyFilter::Any,
        _ => {
            return Err(CallError::BadArg {
                name: "filter",
                expected: "one of: waypoint | poi | vista | hero_point | task | any",
            });
        }
    })
}

const SEARCH_DEFAULT_LIMIT: u32 = 10;
const SEARCH_MAX_LIMIT: u32 = 50;
const SEARCH_MIN_QUERY_LEN: usize = 2;

pub(super) fn parse_search_query(args: &serde_json::Value) -> Result<String, CallError> {
    let raw = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or(CallError::MissingArg("query"))?;
    let trimmed = raw.trim();
    if trimmed.chars().count() < SEARCH_MIN_QUERY_LEN {
        return Err(CallError::BadArg {
            name: "query",
            expected: "at least 2 characters",
        });
    }
    Ok(trimmed.to_owned())
}

pub(super) fn parse_search_limit(args: &serde_json::Value) -> u32 {
    args.get("limit")
        .and_then(serde_json::Value::as_u64)
        .map_or(SEARCH_DEFAULT_LIMIT, |n| {
            u32::try_from(n)
                .unwrap_or(SEARCH_MAX_LIMIT)
                .min(SEARCH_MAX_LIMIT)
        })
        .max(1)
}

pub(super) fn parse_optional_str(args: &serde_json::Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(super) fn parse_optional_u32(args: &serde_json::Value, name: &str) -> Option<u32> {
    args.get(name)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Parse `within_minutes` (for `get_event_schedule`). Defaults to 60.
/// Clamped to [1, 1440] — the LLM has no business asking for a
/// 0-minute window, and 1440 (one full day) is the cycle ceiling.
pub(super) fn parse_within_minutes(args: &serde_json::Value) -> u32 {
    args.get("within_minutes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(60)
        .clamp(1, 1440)
}

/// Parse the optional `active_festivals` array. Returns an empty set
/// when omitted or null — the dispatcher treats that as "no festival
/// events at all", so the LLM has to opt-in. Non-array values are
/// rejected with `BadArg`.
pub(super) fn parse_active_festivals(
    v: Option<&serde_json::Value>,
) -> Result<std::collections::HashSet<String>, CallError> {
    let mut out = std::collections::HashSet::new();
    let Some(v) = v else { return Ok(out) };
    if v.is_null() {
        return Ok(out);
    }
    let Some(arr) = v.as_array() else {
        return Err(CallError::BadArg {
            name: "active_festivals",
            expected: "array of festival names",
        });
    };
    for entry in arr {
        let Some(s) = entry.as_str() else {
            return Err(CallError::BadArg {
                name: "active_festivals",
                expected: "array of festival names (strings)",
            });
        };
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            out.insert(trimmed.to_owned());
        }
    }
    Ok(out)
}

pub(super) const DEFAULT_PAGE_SIZE: u32 = 25;
pub(super) const MAX_PAGE_SIZE: u32 = 100;

/// Opaque cursor payload — base64(json). Format kept tiny so cursors stay
/// short on the wire and are easy to debug by hand-decoding when something
/// goes wrong in the field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct CursorPayload {
    /// SHA-256 (first 16 hex chars) of the canonical (source, profession,
    /// gamemode) triple. A cursor minted with one filter set is rejected if
    /// the filter changes mid-paginate; that prevents "page 2 of a different
    /// listing than page 1" surprises.
    #[serde(rename = "h")]
    pub(super) filter_hash: String,
    /// Zero-based offset into the (cached, deterministic) listing.
    #[serde(rename = "o")]
    pub(super) offset: u32,
}

pub(super) fn catalog_filter_hash(source: &str, filter: &CatalogFilter) -> String {
    use sha2::{Digest, Sha256};
    let prof = filter.profession.as_deref().unwrap_or("*");
    let mode = filter.gamemode.as_deref().unwrap_or("*");
    // Limit is deliberately excluded from the hash because MCP-layer
    // pagination no longer exposes it; `CatalogFilter::limit` is always None
    // when minted by `handle_list_catalog_builds`.
    let canon = format!("{source}|{prof}|{mode}");
    let digest = Sha256::digest(canon.as_bytes());
    hex::encode(&digest[..8])
}

pub(super) fn encode_cursor(payload: &CursorPayload) -> String {
    use base64::Engine as _;
    let json = serde_json::to_vec(payload).expect("cursor payload always serialises");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

pub(super) fn decode_cursor(raw: &str) -> Result<CursorPayload, &'static str> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| "cursor is not valid base64url")?;
    serde_json::from_slice::<CursorPayload>(&bytes).map_err(|_| "cursor payload is malformed")
}

/// Parse an optional array of positive ints into typed ids.
pub(super) fn parse_id_array<Id, F>(
    args: &serde_json::Value,
    name: &'static str,
    ctor: F,
) -> Result<Vec<Id>, CallError>
where
    F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
{
    match args.get(name) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_i64()
                    .ok_or(CallError::BadArg {
                        name,
                        expected: "array of positive integers",
                    })
                    .and_then(|n| ctor(n).map_err(CallError::Domain))
            })
            .collect(),
        Some(_) => Err(CallError::BadArg {
            name,
            expected: "array",
        }),
    }
}

pub(super) fn parse_required_id_array<Id, F>(
    args: &serde_json::Value,
    name: &'static str,
    ctor: F,
) -> Result<Vec<Id>, CallError>
where
    F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
{
    let ids = parse_id_array(args, name, ctor)?;
    if ids.is_empty() {
        return Err(CallError::BadArg {
            name,
            expected: "non-empty array of positive integers",
        });
    }
    Ok(ids)
}

/// Parse the optional `summary` flag. Defaults to `true` because summary
/// mode drops ~70% of payload bytes (facts, icon URLs) — the right default
/// for an LLM-facing API.
pub(super) fn parse_summary(args: &serde_json::Value) -> bool {
    args.get("summary")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Parse the optional `tab` selector for `get_character_build`. Accepts
/// `"active"` (default), `"all"`, or a numeric string `"1"`/`"2"`/...
pub(super) fn parse_tab_selector(args: &serde_json::Value) -> Result<TabSelector, CallError> {
    let Some(v) = args.get("tab") else {
        return Ok(TabSelector::default());
    };
    match v {
        serde_json::Value::Null => Ok(TabSelector::default()),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.eq_ignore_ascii_case("active") || s.is_empty() {
                Ok(TabSelector::Active)
            } else if s.eq_ignore_ascii_case("all") {
                Ok(TabSelector::All)
            } else if let Ok(n) = s.parse::<u8>() {
                Ok(TabSelector::Index(n))
            } else {
                Err(CallError::BadArg {
                    name: "tab",
                    expected: "\"active\", \"all\", or a numeric tab index (e.g. \"1\")",
                })
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64()
                && let Ok(idx) = u8::try_from(u)
            {
                Ok(TabSelector::Index(idx))
            } else {
                Err(CallError::BadArg {
                    name: "tab",
                    expected: "tab index in 0..=255",
                })
            }
        }
        _ => Err(CallError::BadArg {
            name: "tab",
            expected: "string or integer",
        }),
    }
}

#[derive(Debug)]
pub(super) enum CallError {
    MissingArg(&'static str),
    BadArg {
        name: &'static str,
        expected: &'static str,
    },
    /// Pagination cursor was rejected — either malformed base64 / json, or
    /// minted with a different filter set than the request now has. The
    /// payload is opaque to callers, so the message must be self-explanatory.
    BadCursor {
        reason: &'static str,
    },
    /// No API key resolved from arg or service default.
    NoApiKey,
    /// Tool-specific annotation of `Gw2ApiError::MissingScope`. The bare
    /// service-layer error says "missing scope: progression" without
    /// naming which tool triggered it, so the LLM has to infer from
    /// context. Wrapping at the dispatcher lets us prepend the tool
    /// name so the message is actionable on its own.
    MissingScopeAt {
        tool: &'static str,
        needed: String,
    },
    /// Same idea for `Unauthorized` (403 without a parseable scope hint).
    UnauthorizedAt {
        tool: &'static str,
    },
    Domain(crate::domain::DomainError),
    Service(crate::service::ServiceError),
    Encode(serde_json::Error),
}

/// Wrap a `ServiceError` so that scope-related failures carry the
/// originating tool name. Other variants pass through unchanged.
///
/// Use at each authed tool handler:
/// ```ignore
/// .map_err(|e| annotate_endpoint(e, "get_account_masteries"))
/// ```
pub(super) fn annotate_endpoint(
    err: crate::service::ServiceError,
    tool: &'static str,
) -> CallError {
    use crate::ports::Gw2ApiError;
    use crate::service::ServiceError;
    match err {
        ServiceError::Gw2(Gw2ApiError::MissingScope { needed }) => {
            CallError::MissingScopeAt { tool, needed }
        }
        ServiceError::Gw2(Gw2ApiError::Unauthorized) => CallError::UnauthorizedAt { tool },
        other => CallError::Service(other),
    }
}

impl From<serde_json::Error> for CallError {
    fn from(value: serde_json::Error) -> Self {
        Self::Encode(value)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingArg(name) => write!(f, "missing required argument '{name}'"),
            Self::BadArg { name, expected } => {
                write!(f, "argument '{name}' has wrong type, expected {expected}")
            }
            Self::BadCursor { reason } => {
                write!(f, "invalid pagination cursor: {reason}")
            }
            Self::NoApiKey => write!(
                f,
                "no Guild Wars 2 API key available — set GW2_API_KEY in the environment or pass \
                 the `api_key` argument. Generate a key at \
                 https://account.arena.net/applications."
            ),
            Self::MissingScopeAt { tool, needed } => write!(
                f,
                "{tool} requires the '{needed}' scope, which this API key doesn't have. \
                 Generate a new key at https://account.arena.net/applications with that scope \
                 checked, or update the existing key's permissions."
            ),
            Self::UnauthorizedAt { tool } => write!(
                f,
                "{tool}: the Guild Wars 2 API rejected this key. Verify the key at \
                 https://account.arena.net/applications and check it has the scopes that \
                 {tool} needs."
            ),
            Self::Domain(e) => write!(f, "validation error: {e}"),
            Self::Service(e) => write!(f, "{e}"),
            Self::Encode(e) => write!(f, "failed to encode response: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::Gw2ApiError;
    use crate::service::ServiceError;

    #[test]
    fn annotate_endpoint_wraps_missing_scope_with_tool_name() {
        let err = ServiceError::Gw2(Gw2ApiError::MissingScope {
            needed: "progression".to_owned(),
        });
        let wrapped = annotate_endpoint(err, "get_account_masteries");
        let rendered = wrapped.to_string();
        assert!(
            rendered.contains("get_account_masteries"),
            "tool name missing from rendered error: {rendered}"
        );
        assert!(
            rendered.contains("progression"),
            "scope name missing from rendered error: {rendered}"
        );
    }

    #[test]
    fn annotate_endpoint_wraps_unauthorized_with_tool_name() {
        let wrapped = annotate_endpoint(ServiceError::Gw2(Gw2ApiError::Unauthorized), "get_wallet");
        let rendered = wrapped.to_string();
        assert!(
            rendered.contains("get_wallet"),
            "tool name missing from unauthorized message: {rendered}"
        );
    }

    #[test]
    fn annotate_endpoint_passes_other_errors_through() {
        let err = ServiceError::Gw2(Gw2ApiError::Decode("garbage".to_owned()));
        let wrapped = annotate_endpoint(err, "get_account");
        // Renders verbatim — no tool-name prefix, no scope rewriting.
        let rendered = wrapped.to_string();
        assert!(
            rendered.contains("garbage"),
            "underlying error must show through: {rendered}"
        );
        assert!(
            !rendered.starts_with("get_account"),
            "decode errors must not get the scope-style prefix: {rendered}"
        );
    }
}
