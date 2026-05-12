//! Wiki "Event timer" widget data shapes + derived schedule response.
//!
//! Source: <https://wiki.guildwars2.com/index.php?title=Widget:Event_timer/data.json&action=raw>
//!
//! Wire layout (verified live, widget version `v5.1`):
//! ```json
//! {
//!   "config": { "version": "v5.1", ... },
//!   "core-dn": {
//!     "category": "Core Tyria",
//!     "name": "Day and night",
//!     "segments": [{"name": "Day", "link": "...", "bg": [...]}, ...],
//!     "pattern": [{"r": 1, "d": 70}, {"r": 2, "d": 5}, ...]
//!   },
//!   "core-wb": { ... },
//!   ...
//! }
//! ```
//!
//! Events are top-level keys (slugs like `core-dn`), not an array. A
//! template entry under key `"t"` carries empty fields and is filtered
//! out at parse time.
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
    /// Parse the raw JSON body served by the wiki widget. The widget JSON
    /// is one big object with `config` and event slugs intermixed at the
    /// top level; this helper handles the split.
    pub fn from_json_value(v: serde_json::Value) -> Result<Self, String> {
        let serde_json::Value::Object(mut map) = v else {
            return Err("top-level event timer JSON must be an object".to_owned());
        };
        let config_val = map
            .remove("config")
            .ok_or_else(|| "missing top-level `config` object".to_owned())?;
        let config: EventScheduleConfig =
            serde_json::from_value(config_val).map_err(|e| format!("decode `config`: {e}"))?;

        let mut events = BTreeMap::new();
        for (slug, val) in map {
            // Skip the placeholder template entry shipped under key "t".
            if slug == "t" {
                continue;
            }
            let def: EventDefinition = match serde_json::from_value(val) {
                Ok(d) => d,
                Err(e) => {
                    // Soft-fail: drop unparseable entries, keep going.
                    // The widget occasionally ships malformed test rows.
                    tracing::debug!(
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
    #[serde(default)]
    pub segments: Vec<SegmentDefinition>,
    #[serde(default)]
    pub pattern: Vec<PatternSlot>,
    /// Some events (Hard world bosses, etc.) publish the 24h schedule
    /// here when `pattern` is empty. Same `{r, d}` shape.
    #[serde(default)]
    pub sequences: EventSequences,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventSequences {
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
}

/// One row per event matching the filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventOccurrence {
    pub event_name: String,
    pub category: String,
    /// Name of the segment that is active right now. Empty string when
    /// the cycle is in a gap (`r == 0`); the LLM should interpret an
    /// empty string as "nothing scheduled at the moment".
    pub current_segment: String,
    pub current_segment_ends_at: DateTime<Utc>,
    pub current_segment_ends_in_minutes: i64,
    pub next_segment_name: String,
    pub next_segment_starts_at: DateTime<Utc>,
    pub next_segment_starts_in_minutes: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    {
      "config": { "version": "v5.1" },
      "t": { "category": "", "name": "", "segments": [], "pattern": [] },
      "core-dn": {
        "category": "Core Tyria",
        "name": "Day and night",
        "segments": [
          {"name": "Day"}, {"name": "Dusk"}, {"name": "Night"}, {"name": "Dawn"}
        ],
        "pattern": [
          {"r": 1, "d": 70}, {"r": 2, "d": 5}, {"r": 3, "d": 40}, {"r": 4, "d": 5}
        ]
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
        assert_eq!(dn.pattern.len(), 4);
        assert_eq!(dn.pattern[0].r, 1);
        assert_eq!(dn.pattern[0].d, 70);
    }

    #[test]
    fn rejects_non_object_top_level() {
        let v: serde_json::Value = serde_json::from_str("[]").unwrap();
        assert!(EventScheduleRaw::from_json_value(v).is_err());
    }
}
