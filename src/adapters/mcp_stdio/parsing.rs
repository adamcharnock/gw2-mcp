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
    Domain(crate::domain::DomainError),
    Service(crate::service::ServiceError),
    Encode(serde_json::Error),
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
            Self::Domain(e) => write!(f, "validation error: {e}"),
            Self::Service(e) => write!(f, "{e}"),
            Self::Encode(e) => write!(f, "failed to encode response: {e}"),
        }
    }
}
