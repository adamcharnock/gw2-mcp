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
//! `segments` is a map keyed by stringified integer (`"0"`, `"1"`, …);
//! `pattern.r` is the key. `r == 0` typically points at a blank/gap
//! segment with an empty name — we surface that as an empty
//! `current_segment` string. Out-of-bounds `r` (segment missing) is
//! also surfaced as empty.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use tracing::warn;

use super::{Service, minutes_between};
use crate::adapters::DEFAULT_EVENT_SCHEDULE_URL;
use crate::domain::{
    EventDefinition, EventFiltersSummary, EventOccurrence, EventScheduleRaw, EventScheduleResponse,
    Expansion, PatternSlot,
};

/// Raw widget cache TTL. The widget changes only on game patches; one
/// day is plenty of slack and keeps a typical session on one HTTP fetch.
const EVENT_SCHEDULE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

// Cache key carries a version suffix so the previous (broken-shape)
// entries don't get reparsed by the corrected parser. Bump this on any
// future widget-shape change that requires a forced refresh.
const EVENT_SCHEDULE_CACHE_KEY: &str = "event_schedule:raw:v2";

/// Filters consumed by [`Service::get_event_schedule`]. Built by the
/// MCP dispatcher from per-call args (plus an auto-fetched `/v2/account`
/// for `player_access` when the caller doesn't pass one).
#[derive(Debug, Clone, Default)]
pub struct EventScheduleFilters {
    pub within_minutes: u32,
    pub category: Option<String>,
    /// Allowed expansions. `None` means "no filter" — the auto-fetch
    /// path falls back to this on missing key / network failure so the
    /// tool still works unauthenticated.
    pub player_access: Option<HashSet<Expansion>>,
    /// `"explicit"` if the caller passed `player_access`, `"auto"` if
    /// it was derived from `/v2/account`, `None` if no filter applied.
    pub player_access_source: Option<&'static str>,
    /// Festivals the caller asserts to be currently active. Empty ≡
    /// "no festival events at all" — the LLM has to opt-in by listing
    /// what it believes (after confirming with the user) is running.
    pub active_festivals: HashSet<String>,
}

impl Service {
    /// Compute the per-event "current segment + next segment" view.
    ///
    /// `within_minutes` filters events whose next segment hasn't started
    /// yet but begins within this window. Events currently running are
    /// always included. `category` (case-insensitive equality) further
    /// narrows by `category` field. `player_access` excludes events
    /// from expansions the caller doesn't own. `active_festivals` is
    /// the only way for `"Special Events"` rows to appear at all.
    pub async fn get_event_schedule(
        &self,
        filters: EventScheduleFilters,
    ) -> Result<EventScheduleResponse, super::ServiceError> {
        let raw = self.fetch_event_schedule_raw().await?;
        let now = self.clock.now();
        Ok(build_schedule_response(
            &raw,
            now,
            &filters,
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
    let mut events_map = serde_json::Map::new();
    for (slug, def) in &raw.events {
        let mut segments = serde_json::Map::new();
        for (key, s) in &def.segments {
            segments.insert(
                key.clone(),
                serde_json::json!({
                    "name": s.name,
                    "link": s.link,
                    "chatlink": s.chatlink,
                }),
            );
        }
        let pattern: Vec<_> = def
            .sequences
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
        events_map.insert(
            slug.clone(),
            serde_json::json!({
                "category": def.category,
                "name": def.name,
                "link": def.link,
                "segments": segments,
                "sequences": { "pattern": pattern, "partial": partial },
            }),
        );
    }
    serde_json::json!({
        "config": { "version": raw.config.version },
        "events": events_map,
    })
}

fn build_schedule_response(
    raw: &EventScheduleRaw,
    now: DateTime<Utc>,
    filters: &EventScheduleFilters,
    source_url: &str,
) -> EventScheduleResponse {
    // Lower-case the active-festival set once so per-event matching
    // is a single hash probe.
    let active_lower: HashSet<String> = filters
        .active_festivals
        .iter()
        .map(|s| s.to_lowercase())
        .collect();

    let mut events: Vec<EventOccurrence> = Vec::new();
    for (slug, def) in &raw.events {
        if let Some(filter) = &filters.category
            && !def.category.eq_ignore_ascii_case(filter)
        {
            continue;
        }
        if !passes_player_access(def, filters.player_access.as_ref()) {
            continue;
        }
        if def.category == SPECIAL_EVENTS_CATEGORY && !is_active_festival(slug, def, &active_lower)
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
            if starts_in <= i64::from(filters.within_minutes) {
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
        filters_applied: EventFiltersSummary {
            within_minutes: filters.within_minutes,
            category: filters.category.clone(),
            player_access: filters.player_access.as_ref().map(|set| {
                let mut names: Vec<String> = set
                    .iter()
                    .map(|exp| {
                        serde_json::to_value(exp)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    })
                    .collect();
                names.sort();
                names
            }),
            player_access_source: filters.player_access_source.map(str::to_owned),
            active_festivals: {
                let mut v: Vec<String> = filters.active_festivals.iter().cloned().collect();
                v.sort();
                v
            },
        },
    }
}

const SPECIAL_EVENTS_CATEGORY: &str = "Special Events";

/// Map a widget `category` string to the `Expansion` the player needs
/// to own. `"Core Tyria"` / `"Living World Season 2"` map to always-
/// granted expansions, so they pass the filter trivially. `"Public
/// Instances"` is intentionally not gated (Eye of the North is
/// core-tier and Convergence opens at `SotO`; the widget doesn't
/// distinguish, so the conservative call is to surface both and let
/// the player decide). `"Special Events"` is gated via the
/// `active_festivals` filter instead, so it returns `None` here.
fn category_to_expansion(category: &str) -> Option<Expansion> {
    match category {
        "Core Tyria" => Some(Expansion::Core),
        "Living World Season 2" => Some(Expansion::LivingWorldSeason2),
        "Heart of Thorns" => Some(Expansion::HeartOfThorns),
        "Living World Season 3" => Some(Expansion::LivingWorldSeason3),
        "Path of Fire" => Some(Expansion::PathOfFire),
        "Living World Season 4" => Some(Expansion::LivingWorldSeason4),
        "The Icebrood Saga" => Some(Expansion::IcebroodSaga),
        "End of Dragons" => Some(Expansion::EndOfDragons),
        "Secrets of the Obscure" => Some(Expansion::SecretsOfTheObscure),
        "Janthir Wilds" => Some(Expansion::JanthirWilds),
        "Visions of Eternity" => Some(Expansion::Castora),
        _ => None,
    }
}

fn passes_player_access(def: &EventDefinition, allowed: Option<&HashSet<Expansion>>) -> bool {
    let Some(allowed) = allowed else {
        return true;
    };
    if def.category == SPECIAL_EVENTS_CATEGORY {
        // Festival events are gated by the separate active_festivals
        // filter — the Festival expansion grant from /v2/account is
        // always present and doesn't gate by itself.
        return true;
    }
    let Some(req) = category_to_expansion(&def.category) else {
        // Unknown category (e.g. "Public Instances", or any new
        // category the widget adds). Default to permissive — better
        // to surface a row the player can't enter than to hide a
        // newly-added category until we update this match.
        return true;
    };
    let always_ok = matches!(
        req,
        Expansion::Core
            | Expansion::Festival
            | Expansion::LivingWorldSeason1
            | Expansion::LivingWorldSeason2
    );
    always_ok || allowed.contains(&req)
}

/// Slug-keyed mapping from widget event slug → canonical festival
/// name as published in `data/festivals.yaml`. Maintained here (not
/// in the YAML) because the widget's slugs are the authoritative
/// identifier for the cycle row; the YAML is the authoritative
/// schedule. Keep both in sync when a new festival event ships.
fn festival_name_for_slug(slug: &str) -> Option<&'static str> {
    match slug {
        // Labyrinthine Cliffs is the hub map for Festival of the Four Winds.
        "festival-lc" => Some("Festival of the Four Winds"),
        "festival-db" => Some("Dragon Bash"),
        "festival-ha" => Some("Halloween"),
        _ => None,
    }
}

fn is_active_festival(slug: &str, def: &EventDefinition, active_lower: &HashSet<String>) -> bool {
    if active_lower.is_empty() {
        return false;
    }
    if let Some(name) = festival_name_for_slug(slug)
        && active_lower.contains(&name.to_lowercase())
    {
        return true;
    }
    // Also accept the literal event name (so "Labyrinthine Cliffs"
    // matches `festival-lc` without forcing the LLM to know the
    // canonical festival name).
    active_lower.contains(&def.name.to_lowercase())
}

/// Walk a single event's pattern and produce its current-segment +
/// next-segment view at `now`. Prefers `sequences.pattern` (the
/// recurring cycle); falls back to `sequences.partial` for events
/// that publish only a non-cyclic daily schedule (hard world bosses).
fn walk_event(def: &EventDefinition, now: DateTime<Utc>) -> Option<EventOccurrence> {
    let slots: &[PatternSlot] = if def.sequences.pattern.is_empty() {
        &def.sequences.partial
    } else {
        &def.sequences.pattern
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

/// Resolve a `pattern.r` value to a segment name by looking up
/// `r.to_string()` in the segments map. Missing keys (typical for
/// `r == 0`, which the widget uses for blank gaps) yield an empty
/// string — the walker exposes that as "currently a gap".
fn segment_name(def: &EventDefinition, r: u32) -> String {
    let key = r.to_string();
    def.segments
        .get(&key)
        .map(|s| s.name.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventScheduleConfig, EventSequences, PatternSlot, SegmentDefinition};
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    fn seg(name: &str) -> SegmentDefinition {
        SegmentDefinition {
            name: name.to_owned(),
            link: None,
            chatlink: None,
        }
    }

    fn segments_map(pairs: &[(u32, &str)]) -> BTreeMap<String, SegmentDefinition> {
        pairs.iter().map(|(k, n)| (k.to_string(), seg(n))).collect()
    }

    fn def_day_and_night() -> EventDefinition {
        EventDefinition {
            category: "Core Tyria".to_owned(),
            name: "Day and night".to_owned(),
            link: None,
            segments: segments_map(&[(1, "Day"), (2, "Dusk"), (3, "Night"), (4, "Dawn")]),
            sequences: EventSequences {
                pattern: vec![
                    PatternSlot { r: 1, d: 70 },
                    PatternSlot { r: 2, d: 5 },
                    PatternSlot { r: 3, d: 40 },
                    PatternSlot { r: 4, d: 5 },
                ],
                partial: vec![],
            },
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
            segments: segments_map(&[(1, "A"), (2, "B"), (3, "C")]),
            sequences: EventSequences {
                pattern: vec![
                    PatternSlot { r: 1, d: 90 },
                    PatternSlot { r: 2, d: 90 },
                    PatternSlot { r: 3, d: 90 },
                ],
                partial: vec![],
            },
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
            // Only segment "1" defined; r=0 falls through to empty.
            segments: segments_map(&[(1, "Tequatl")]),
            sequences: EventSequences {
                pattern: vec![
                    PatternSlot { r: 1, d: 30 }, // Tequatl
                    PatternSlot { r: 0, d: 30 }, // gap (no segment "0" registered)
                ],
                partial: vec![],
            },
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
            segments: segments_map(&[(1, "Only")]),
            sequences: EventSequences {
                pattern: vec![],
                partial: vec![PatternSlot { r: 1, d: 60 }],
            },
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let occ = walk_event(&def, now).unwrap();
        assert_eq!(occ.current_segment, "Only");
        assert_eq!(occ.current_segment_ends_in_minutes, 60);
    }

    fn filters(within_minutes: u32) -> EventScheduleFilters {
        EventScheduleFilters {
            within_minutes,
            category: None,
            player_access: None,
            player_access_source: None,
            active_festivals: HashSet::new(),
        }
    }

    fn raw_with(slug: &str, def: EventDefinition) -> EventScheduleRaw {
        let mut events = BTreeMap::new();
        events.insert(slug.to_owned(), def);
        EventScheduleRaw {
            config: EventScheduleConfig {
                version: "test".into(),
            },
            events,
        }
    }

    fn def_festival_halloween() -> EventDefinition {
        EventDefinition {
            category: "Special Events".to_owned(),
            name: "Halloween".to_owned(),
            link: None,
            segments: segments_map(&[(1, "Mad King Says")]),
            sequences: EventSequences {
                pattern: vec![PatternSlot { r: 1, d: 60 }],
                partial: vec![],
            },
        }
    }

    #[test]
    fn within_minutes_filter_excludes_distant_events() {
        let raw = raw_with("dn", def_day_and_night());
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let r60 = build_schedule_response(&raw, now, &filters(60), "u");
        assert!(r60.events.is_empty(), "next starts in 70 min > 60");
        let r90 = build_schedule_response(&raw, now, &filters(90), "u");
        assert_eq!(r90.events.len(), 1);
    }

    #[test]
    fn category_filter_is_case_insensitive() {
        let raw = raw_with("dn", def_day_and_night());
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let mut f = filters(1440);
        f.category = Some("CORE tyria".into());
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert_eq!(resp.events.len(), 1);
        f.category = Some("Maguuma".into());
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert!(resp.events.is_empty());
    }

    #[test]
    fn player_access_filter_excludes_unowned_expansions() {
        // Build a HoT event (`hot-vb`) + a Core event (`core-dn`); pass
        // only Core access. HoT row must drop, Core must pass.
        let mut events = BTreeMap::new();
        events.insert("core-dn".to_owned(), def_day_and_night());
        events.insert(
            "hot-vb".to_owned(),
            EventDefinition {
                category: "Heart of Thorns".to_owned(),
                name: "Verdant Brink".to_owned(),
                link: None,
                segments: segments_map(&[(1, "Daytime")]),
                sequences: EventSequences {
                    pattern: vec![PatternSlot { r: 1, d: 60 }],
                    partial: vec![],
                },
            },
        );
        let raw = EventScheduleRaw {
            config: EventScheduleConfig {
                version: "test".into(),
            },
            events,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let mut f = filters(1440);
        f.player_access = Some(HashSet::from([Expansion::Core]));
        f.player_access_source = Some("explicit");
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].event_name, "Day and night");
        assert_eq!(
            resp.filters_applied.player_access.as_deref(),
            Some(["core".to_owned()].as_slice())
        );
        assert_eq!(
            resp.filters_applied.player_access_source.as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn special_events_excluded_when_active_festivals_empty() {
        let raw = raw_with("festival-ha", def_festival_halloween());
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let resp = build_schedule_response(&raw, now, &filters(1440), "u");
        assert!(
            resp.events.is_empty(),
            "festival events must be excluded when active_festivals is empty"
        );
    }

    #[test]
    fn special_events_included_when_festival_active() {
        let raw = raw_with("festival-ha", def_festival_halloween());
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let mut f = filters(1440);
        f.active_festivals = HashSet::from(["Halloween".to_owned()]);
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].event_name, "Halloween");
        assert_eq!(
            resp.filters_applied.active_festivals,
            vec!["Halloween".to_owned()]
        );
    }

    #[test]
    fn festival_filter_accepts_hub_map_name() {
        // `festival-lc` resolves to "Festival of the Four Winds", but
        // the LLM may pass the literal event name "Labyrinthine Cliffs".
        let def = EventDefinition {
            category: "Special Events".to_owned(),
            name: "Labyrinthine Cliffs".to_owned(),
            link: None,
            segments: segments_map(&[(1, "Boss Rush")]),
            sequences: EventSequences {
                pattern: vec![PatternSlot { r: 1, d: 60 }],
                partial: vec![],
            },
        };
        let raw = raw_with("festival-lc", def);
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let mut f = filters(1440);
        f.active_festivals = HashSet::from(["Labyrinthine Cliffs".to_owned()]);
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert_eq!(resp.events.len(), 1);
    }

    #[test]
    fn player_access_does_not_gate_special_events() {
        // Passing `player_access = [Core]` shouldn't hide festival events
        // when active_festivals is populated — the festival filter owns
        // the gate.
        let raw = raw_with("festival-ha", def_festival_halloween());
        let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
        let mut f = filters(1440);
        f.player_access = Some(HashSet::from([Expansion::Core]));
        f.active_festivals = HashSet::from(["Halloween".to_owned()]);
        let resp = build_schedule_response(&raw, now, &f, "u");
        assert_eq!(resp.events.len(), 1);
    }
}
