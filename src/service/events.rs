//! Event schedule service. Walks the wiki widget's `pattern` (or
//! `sequences.partial` fallback) for each event, anchored at UTC
//! midnight, and reports current + next segment with both ISO 8601
//! timestamps and minute-deltas.
//!
//! ## Cycle anchoring
//!
//! Empirically verified: the widget's JavaScript reference time for the
//! schedule is **UTC midnight** of the current day, not an absolute
//! epoch. So `position_in_cycle = minutes_since_utc_midnight %
//! cycle_total`. This works for every event whose cycle total divides
//! 1440 evenly (e.g. 120, 60, 30) and is correct for the 270-min
//! "World bosses" cycle too — that cycle is documented as repeating
//! continuously, and the wiki's published schedule is consistent with
//! starting it at UTC midnight every day.
//!
//! ## Indexing
//!
//! `pattern[i].r` is **1-based** into `segments[]`. `r == 0` means
//! "gap with no active segment" — surfaced as an empty
//! `current_segment` string.

use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use tracing::warn;

use super::{Service, minutes_between};
use crate::adapters::DEFAULT_EVENT_SCHEDULE_URL;
use crate::domain::{
    EventDefinition, EventOccurrence, EventScheduleRaw, EventScheduleResponse, PatternSlot,
};

/// Raw widget cache TTL. The widget changes only on game patches; one
/// day is plenty of slack and keeps a typical session on one HTTP fetch.
const EVENT_SCHEDULE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

const EVENT_SCHEDULE_CACHE_KEY: &str = "event_schedule:raw";

impl Service {
    /// Compute the per-event "current segment + next segment" view.
    ///
    /// `within_minutes` filters events whose next segment hasn't started
    /// yet but begins within this window. Events that are *currently*
    /// running are always included. `category` (case-insensitive
    /// equality) further filters by `category` field on the event.
    pub async fn get_event_schedule(
        &self,
        within_minutes: u32,
        category: Option<&str>,
    ) -> Result<EventScheduleResponse, super::ServiceError> {
        let raw = self.fetch_event_schedule_raw().await?;
        let now = self.clock.now();
        Ok(build_schedule_response(
            &raw,
            now,
            within_minutes,
            category,
            DEFAULT_EVENT_SCHEDULE_URL,
        ))
    }

    async fn fetch_event_schedule_raw(&self) -> Result<EventScheduleRaw, super::ServiceError> {
        if let Some(json) = self.cache.get(EVENT_SCHEDULE_CACHE_KEY).await {
            match serde_json::from_str::<serde_json::Value>(&json)
                .map_err(|e| e.to_string())
                .and_then(EventScheduleRaw::from_json_value)
            {
                Ok(raw) => return Ok(raw),
                Err(e) => {
                    warn!(error = %e, "event schedule cache entry was unreadable; refetching");
                }
            }
        }
        let raw = self.event_schedule.fetch_raw().await?;
        // Cache the body verbatim. The raw JSON ride-throughs the cache
        // because re-serialising the parsed shape into the same JSON
        // would round-trip everything we just deserialised.
        if let Ok(json) = serde_json::to_string(&raw_as_value(&raw)) {
            self.cache
                .set(EVENT_SCHEDULE_CACHE_KEY, json, EVENT_SCHEDULE_TTL)
                .await;
        }
        Ok(raw)
    }
}

/// Re-serialise the parsed widget back into the wire-equivalent JSON so
/// the cache round-trips via `from_json_value`. We only persist the
/// fields the parser inspects (config + each event's serialisable
/// fields); anything we didn't model is dropped, which is fine because
/// we only ever read what we modelled.
fn raw_as_value(raw: &EventScheduleRaw) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "config".to_owned(),
        serde_json::json!({ "version": raw.config.version }),
    );
    for (slug, def) in &raw.events {
        let segments: Vec<_> = def
            .segments
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "link": s.link,
                    "chatlink": s.chatlink,
                })
            })
            .collect();
        let pattern: Vec<_> = def
            .pattern
            .iter()
            .map(|p| serde_json::json!({ "r": p.r, "d": p.d }))
            .collect();
        let partial: Vec<_> = def
            .sequences
            .partial
            .iter()
            .map(|p| serde_json::json!({ "r": p.r, "d": p.d }))
            .collect();
        out.insert(
            slug.clone(),
            serde_json::json!({
                "category": def.category,
                "name": def.name,
                "link": def.link,
                "segments": segments,
                "pattern": pattern,
                "sequences": { "partial": partial },
            }),
        );
    }
    serde_json::Value::Object(out)
}

fn build_schedule_response(
    raw: &EventScheduleRaw,
    now: DateTime<Utc>,
    within_minutes: u32,
    category: Option<&str>,
    source_url: &str,
) -> EventScheduleResponse {
    let mut events: Vec<EventOccurrence> = Vec::new();
    for def in raw.events.values() {
        if let Some(filter) = category
            && !def.category.eq_ignore_ascii_case(filter)
        {
            continue;
        }
        if let Some(occ) = walk_event(def, now) {
            let starts_in = occ.next_segment_starts_in_minutes;
            // Always include events that are currently running (or
            // gapped); also include events whose next segment is within
            // the requested window. Negative starts_in shouldn't happen
            // (the walker reports the *next* segment) but we treat <=
            // window as inclusive for safety.
            if starts_in <= i64::from(within_minutes) {
                events.push(occ);
            }
        }
    }
    events.sort_by(|a, b| {
        a.next_segment_starts_in_minutes
            .cmp(&b.next_segment_starts_in_minutes)
            .then_with(|| a.event_name.cmp(&b.event_name))
    });
    EventScheduleResponse {
        events,
        generated_at: now,
        source_url: source_url.to_owned(),
        widget_version: raw.config.version.clone(),
    }
}

/// Walk a single event's pattern and produce its current-segment +
/// next-segment view at `now`. Returns `None` for events with neither
/// `pattern` nor `sequences.partial` populated.
fn walk_event(def: &EventDefinition, now: DateTime<Utc>) -> Option<EventOccurrence> {
    let slots: &[PatternSlot] = if def.pattern.is_empty() {
        &def.sequences.partial
    } else {
        &def.pattern
    };
    if slots.is_empty() {
        return None;
    }
    let cycle_total: u32 = slots.iter().map(|s| s.d).sum();
    if cycle_total == 0 {
        return None;
    }

    let minutes_since_midnight = now.hour() * 60 + now.minute();
    let position = minutes_since_midnight % cycle_total;

    // Locate the current slot + minutes-into-it.
    let mut acc: u32 = 0;
    let mut current_idx: usize = 0;
    let mut minutes_into_current: u32 = 0;
    for (i, slot) in slots.iter().enumerate() {
        if position < acc + slot.d {
            current_idx = i;
            minutes_into_current = position - acc;
            break;
        }
        acc += slot.d;
    }
    let current_slot = slots[current_idx];
    let next_idx = (current_idx + 1) % slots.len();
    let next_slot = slots[next_idx];

    let current_remaining = i64::from(current_slot.d) - i64::from(minutes_into_current);
    let ends_at = now + chrono::Duration::minutes(current_remaining);
    let starts_at = ends_at;

    let current_segment = segment_name(def, current_slot.r);
    let next_segment_name = segment_name(def, next_slot.r);

    Some(EventOccurrence {
        event_name: def.name.clone(),
        category: def.category.clone(),
        current_segment,
        current_segment_ends_at: ends_at,
        current_segment_ends_in_minutes: minutes_between(now, ends_at),
        next_segment_name,
        next_segment_starts_at: starts_at,
        next_segment_starts_in_minutes: minutes_between(now, starts_at),
    })
}

/// Resolve a `pattern.r` value to a segment name. `r == 0` is a gap;
/// otherwise `segments[r - 1]`. Out-of-bounds yields an empty string.
fn segment_name(def: &EventDefinition, r: u32) -> String {
    if r == 0 {
        return String::new();
    }
    let idx = (r - 1) as usize;
    def.segments
        .get(idx)
        .map(|s| s.name.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventScheduleConfig, EventSequences, PatternSlot, SegmentDefinition};
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    fn def_day_and_night() -> EventDefinition {
        EventDefinition {
            category: "Core Tyria".to_owned(),
            name: "Day and night".to_owned(),
            link: None,
            segments: ["Day", "Dusk", "Night", "Dawn"]
                .iter()
                .map(|n| SegmentDefinition {
                    name: (*n).to_owned(),
                    link: None,
                    chatlink: None,
                })
                .collect(),
            pattern: vec![
                PatternSlot { r: 1, d: 70 },
                PatternSlot { r: 2, d: 5 },
                PatternSlot { r: 3, d: 40 },
                PatternSlot { r: 4, d: 5 },
            ],
            sequences: EventSequences::default(),
        }
    }

    #[test]
    fn cycle_walker_at_midnight_picks_first_segment() {
        let def = def_day_and_night();
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Day");
        assert_eq!(occ.current_segment_ends_in_minutes, 70);
        assert_eq!(occ.next_segment_name, "Dusk");
        assert_eq!(occ.next_segment_starts_in_minutes, 70);
    }

    #[test]
    fn cycle_walker_mid_segment_reports_remaining() {
        let def = def_day_and_night();
        // 30 minutes into the cycle → Day with 40 minutes remaining.
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 30, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Day");
        assert_eq!(occ.current_segment_ends_in_minutes, 40);
        assert_eq!(occ.next_segment_name, "Dusk");
    }

    #[test]
    fn cycle_walker_at_segment_boundary_starts_next() {
        let def = def_day_and_night();
        // Exactly 70 minutes in → start of Dusk.
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 1, 10, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Dusk");
        assert_eq!(occ.current_segment_ends_in_minutes, 5);
        assert_eq!(occ.next_segment_name, "Night");
    }

    #[test]
    fn cycle_walker_wraps_at_120_min() {
        let def = def_day_and_night();
        // 2 hours after midnight = position 0 again → Day.
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 2, 0, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Day");
        assert_eq!(occ.current_segment_ends_in_minutes, 70);
    }

    #[test]
    fn cycle_walker_handles_270_min_cycle() {
        // Synthetic 270-min cycle: 3 segments at 90 min each.
        let def = EventDefinition {
            category: "Core Tyria".to_owned(),
            name: "World bosses (synthetic)".to_owned(),
            link: None,
            segments: ["A", "B", "C"]
                .iter()
                .map(|n| SegmentDefinition {
                    name: (*n).to_owned(),
                    link: None,
                    chatlink: None,
                })
                .collect(),
            pattern: vec![
                PatternSlot { r: 1, d: 90 },
                PatternSlot { r: 2, d: 90 },
                PatternSlot { r: 3, d: 90 },
            ],
            sequences: EventSequences::default(),
        };
        // 150 minutes after midnight: position = 150 % 270 = 150.
        // Walk: A occupies [0,90), B occupies [90,180). So B is current.
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 2, 30, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "B");
        assert_eq!(occ.current_segment_ends_in_minutes, 30);
        assert_eq!(occ.next_segment_name, "C");
    }

    #[test]
    fn gap_slot_resolves_to_empty_name() {
        let def = EventDefinition {
            category: "Core Tyria".to_owned(),
            name: "Hard world bosses (synthetic)".to_owned(),
            link: None,
            segments: vec![SegmentDefinition {
                name: "Tequatl".to_owned(),
                link: None,
                chatlink: None,
            }],
            pattern: vec![
                PatternSlot { r: 1, d: 30 }, // Tequatl
                PatternSlot { r: 0, d: 30 }, // gap
            ],
            sequences: EventSequences::default(),
        };
        // 40 min after midnight → 10 min into the gap.
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 40, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "");
        assert_eq!(occ.next_segment_name, "Tequatl");
        assert_eq!(occ.next_segment_starts_in_minutes, 20);
    }

    #[test]
    fn empty_pattern_falls_back_to_sequences_partial() {
        let def = EventDefinition {
            category: "Core Tyria".to_owned(),
            name: "From-partial".to_owned(),
            link: None,
            segments: vec![SegmentDefinition {
                name: "Only".to_owned(),
                link: None,
                chatlink: None,
            }],
            pattern: vec![],
            sequences: EventSequences {
                partial: vec![PatternSlot { r: 1, d: 60 }],
            },
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Only");
        assert_eq!(occ.current_segment_ends_in_minutes, 60);
    }

    #[test]
    fn within_minutes_filter_excludes_distant_events() {
        let mut events = BTreeMap::new();
        // Both events have a "next segment in 70 min" at UTC midnight.
        events.insert("dn".to_owned(), def_day_and_night());
        let raw = EventScheduleRaw {
            config: EventScheduleConfig {
                version: "test".into(),
            },
            events,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let r60 = build_schedule_response(&raw, now, 60, None, "u");
        assert!(r60.events.is_empty(), "next starts in 70 min > 60");
        let r90 = build_schedule_response(&raw, now, 90, None, "u");
        assert_eq!(r90.events.len(), 1);
    }

    #[test]
    fn category_filter_is_case_insensitive() {
        let mut events = BTreeMap::new();
        events.insert("dn".to_owned(), def_day_and_night());
        let raw = EventScheduleRaw {
            config: EventScheduleConfig {
                version: "test".into(),
            },
            events,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let resp = build_schedule_response(&raw, now, 1440, Some("CORE tyria"), "u");
        assert_eq!(resp.events.len(), 1);
        let resp = build_schedule_response(&raw, now, 1440, Some("Maguuma"), "u");
        assert!(resp.events.is_empty());
    }
}
