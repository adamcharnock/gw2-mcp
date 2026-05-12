//! Wiki "Event timer" widget data shapes + derived schedule response.
//!
//! Source: <https://wiki.guildwars2.com/index.php?title=Widget:Event_timer/data.json&action=raw>
//!
//! Wire layout (verified live against widget version `v5.1`):
//! ```json
//! {
//!   "config": { "version": "v5.1", ... },
//!   "events": {
//!     "t":       { ...template, empty fields, ignored... },
//!     "core-dn": {
//!       "category": "Core Tyria",
//!       "name": "Day and night",
//!       "segments": {
//!         "1": {"name": "Day", "link": "...", "bg": [...]},
//!         "2": {"name": "Dusk", ...},
//!         ...
//!       },
//!       "sequences": {
//!         "pattern": [{"r": 1, "d": 70}, {"r": 2, "d": 5}, ...],
//!         "partial": [...]
//!       }
//!     },
//!     "core-wb": { ... },
//!     ...
//!   }
//! }
//! ```
//!
//! - Events live in an inner `events` object keyed by slug (`core-dn`,
//!   `hot-vb`, `festival-db`, …).
//! - `segments` is a **map** keyed by stringified integers (`"0"`, `"1"`,
//!   `"2"`, …), not an array. `pattern.r` is the key into this map.
//! - The recurring cycle lives at `sequences.pattern`; for the handful
//!   of events that don't have one (e.g. hard world bosses), the day's
//!   schedule lives at `sequences.partial` and we fall back to it.
//! - A template entry under key `"t"` carries empty fields and is
//!   filtered out at parse time.
//!
//! Two prior bugs in this parser inspired the loud doc comment: the
//! events were assumed to be at the top level (drove a zero-result
//! tool), and `segments` was assumed to be an array indexed by
//! `pattern.r - 1`. The shape above is what the widget actually
//! publishes.
//!
//! `pattern` is the recurring cycle: each entry says "play segment `r`
//! for `d` minutes". `r` is **1-based** into `segments[]`; `r == 0`
//! denotes a gap with no active segment.
//!
//! Some events (notably "Hard world bosses") publish their schedule in
//! `sequences.partial` instead of (or alongside) `pattern`. We treat
//! `sequences.partial` as a fallback when `pattern` is empty.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level wire shape. The JSON layout intersperses one `config`
/// object with a flat map of event slugs → event definitions, so we
/// deserialize the whole thing as a generic map and then split out the
/// known `config` key.
#[derive(Debug, Clone, Deserialize)]
pub struct EventScheduleRaw {
    pub config: EventScheduleConfig,
    /// Event slug → definition. Excludes the placeholder `"t"` template
    /// entry and the `"config"` object.
    pub events: BTreeMap<String, EventDefinition>,
}

impl EventScheduleRaw {
    /// Parse the raw JSON body served by the wiki widget.
    pub fn from_json_value(v: serde_json::Value) -> Result<Self, String> {
        let serde_json::Value::Object(mut map) = v else {
            return Err("top-level event timer JSON must be an object".to_owned());
        };
        let config_val = map
            .remove("config")
            .ok_or_else(|| "missing top-level `config` object".to_owned())?;
        let config: EventScheduleConfig =
            serde_json::from_value(config_val).map_err(|e| format!("decode `config`: {e}"))?;

        let events_val = map
            .remove("events")
            .ok_or_else(|| "missing top-level `events` object".to_owned())?;
        let serde_json::Value::Object(event_map) = events_val else {
            return Err("`events` must be an object keyed by slug".to_owned());
        };

        let mut events = BTreeMap::new();
        for (slug, val) in event_map {
            // Skip the placeholder template entry shipped under key "t".
            if slug == "t" {
                continue;
            }
            let def: EventDefinition = match serde_json::from_value(val) {
                Ok(d) => d,
                Err(e) => {
                    // Drop unparseable entries but keep going. Logged at
                    // warn (not debug) so a future widget schema change
                    // surfaces in default-level logs instead of vanishing.
                    tracing::warn!(
                        slug = slug.as_str(),
                        error = %e,
                        "skipping unparseable event timer entry"
                    );
                    continue;
                }
            };
            if def.name.trim().is_empty() {
                continue;
            }
            events.insert(slug, def);
        }
        Ok(Self { config, events })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventScheduleConfig {
    /// Widget version (e.g. `"v5.1"`). Surfaced to the LLM so a stale
    /// version after a wiki refresh is observable.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventDefinition {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub link: Option<String>,
    /// Segment map keyed by stringified integer (`"0"`, `"1"`, …). The
    /// widget uses `pattern.r` as a key into this map; `r == 0`
    /// conventionally references a blank/gap segment.
    #[serde(default)]
    pub segments: BTreeMap<String, SegmentDefinition>,
    #[serde(default)]
    pub sequences: EventSequences,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventSequences {
    /// Recurring cycle. Walked first; if empty, the walker falls back
    /// to `partial`.
    #[serde(default)]
    pub pattern: Vec<PatternSlot>,
    /// The "day's schedule" for non-cyclic events (hard world bosses)
    /// — and a leading-fragment of the current cycle for cyclic ones.
    /// Only used when `pattern` is empty.
    #[serde(default)]
    pub partial: Vec<PatternSlot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SegmentDefinition {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub chatlink: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PatternSlot {
    /// 1-based index into `segments[]`. `r == 0` denotes a gap with no
    /// active segment (rare; only on events with explicit gaps in the
    /// timeline).
    pub r: u32,
    /// Duration of this slot in minutes.
    pub d: u32,
}

// ---------------------------------------------------------------------------
// Derived response shapes — what `get_event_schedule` returns to MCP callers.
// ---------------------------------------------------------------------------

/// Top-level response from `get_event_schedule`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventScheduleResponse {
    pub events: Vec<EventOccurrence>,
    pub generated_at: DateTime<Utc>,
    pub source_url: String,
    pub widget_version: String,
    /// Echo of the filters that were actually applied. Lets the LLM
    /// reason about why a given event is (or isn't) in the result —
    /// most usefully: whether `player_access` came from the caller
    /// explicitly or was derived from `/v2/account`.
    pub filters_applied: EventFiltersSummary,
}

/// Filters echoed back in the response so the caller can see what
/// shaped the result. Mirrors the `RouteFiltersSummary` pattern used
/// by `plan_route`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventFiltersSummary {
    pub within_minutes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Sorted `snake_case` expansion names. `None` ≡ no access filter
    /// applied (no key was available and no list was passed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_access: Option<Vec<String>>,
    /// `"explicit"` if the caller passed a list, `"auto"` if it was
    /// derived from `/v2/account`, `None` if no filter was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_access_source: Option<String>,
    /// Festivals the caller declared to be active. Festival-gated
    /// events (category `"Special Events"`) are excluded unless their
    /// festival appears here — the LLM is expected to call
    /// `get_active_festivals` and confirm with the user before passing
    /// anything in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_festivals: Vec<String>,
}

/// One row per event matching the filter.
///
/// Note: there is no `next_segment_starts_at` field — by construction
/// it would always equal `current_segment_ends_at` (the boundary
/// instant is shared). The minute deltas remain split so the LLM
/// doesn't need to subtract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventOccurrence {
    pub event_name: String,
    pub category: String,
    /// Name of the segment that is active right now. Surfaced as
    /// `"(idle)"` when the cycle is in an explicit gap slot (`r == 0`)
    /// — most commonly "Hard world bosses" between bosses. The LLM
    /// should interpret `"(idle)"` as "nothing scheduled at the moment;
    /// the `next_segment_*` fields tell you what's coming."
    pub current_segment: String,
    /// Total length of the current segment (or gap), in minutes. Lets
    /// the LLM say "5 minutes left of 30" instead of just "5 minutes
    /// left" — useful for "should I head to Tarir now?" planning.
    pub current_segment_total_minutes: u32,
    pub current_segment_ends_at: DateTime<Utc>,
    pub current_segment_ends_in_minutes: i64,
    pub next_segment_name: String,
    pub next_segment_starts_in_minutes: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    {
      "config": { "version": "v5.1" },
      "events": {
        "t": { "category": "", "name": "", "segments": {}, "sequences": {"pattern": []} },
        "core-dn": {
          "category": "Core Tyria",
          "name": "Day and night",
          "segments": {
            "1": {"name": "Day"},
            "2": {"name": "Dusk"},
            "3": {"name": "Night"},
            "4": {"name": "Dawn"}
          },
          "sequences": {
            "pattern": [
              {"r": 1, "d": 70}, {"r": 2, "d": 5}, {"r": 3, "d": 40}, {"r": 4, "d": 5}
            ]
          }
        }
      }
    }
    "#;

    #[test]
    fn parses_sample_widget_json() {
        let v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let raw = EventScheduleRaw::from_json_value(v).unwrap();
        assert_eq!(raw.config.version, "v5.1");
        assert_eq!(raw.events.len(), 1, "template `t` entry must be filtered");
        let dn = raw.events.get("core-dn").unwrap();
        assert_eq!(dn.name, "Day and night");
        assert_eq!(dn.segments.len(), 4);
        assert_eq!(dn.sequences.pattern.len(), 4);
        assert_eq!(dn.sequences.pattern[0].r, 1);
        assert_eq!(dn.sequences.pattern[0].d, 70);
        assert_eq!(dn.segments.get("1").unwrap().name, "Day");
    }

    #[test]
    fn rejects_non_object_top_level() {
        let v: serde_json::Value = serde_json::from_str("[]").unwrap();
        assert!(EventScheduleRaw::from_json_value(v).is_err());
    }
}
