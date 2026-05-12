//! Regression tests for the `Event_timer` widget parser against a real
//! widget snapshot.
//!
//! The widget JSON nests events under an `events` object (not the top
//! level — a previous version of the parser assumed the latter and
//! silently dropped all 43 entries, returning an empty schedule).
//! Pinning a real-shaped fixture catches a recurrence at `cargo test`
//! time instead of via a "tool returns []" bug report.

mod common;

use chrono::{TimeZone, Utc};
use gw2_mcp::domain::EventScheduleRaw;

const WIDGET_SNAPSHOT: &str = include_str!("fixtures/event_schedule/widget_v5.1.json");

#[test]
fn parses_real_widget_snapshot_into_many_events() {
    let v: serde_json::Value = serde_json::from_str(WIDGET_SNAPSHOT).unwrap();
    let raw = EventScheduleRaw::from_json_value(v).expect("parse");
    assert_eq!(raw.config.version, "v5.1");
    // Live snapshot at capture time had 43 inner entries (one template
    // plus 42 real events). The exact count can drift with widget
    // updates, but it should never collapse to 0 or 1.
    assert!(
        raw.events.len() >= 20,
        "expected at least 20 events, got {}",
        raw.events.len()
    );
}

#[test]
fn snapshot_covers_diverse_categories() {
    let v: serde_json::Value = serde_json::from_str(WIDGET_SNAPSHOT).unwrap();
    let raw = EventScheduleRaw::from_json_value(v).unwrap();
    let cats: std::collections::BTreeSet<&str> =
        raw.events.values().map(|e| e.category.as_str()).collect();
    // Three buckets the LLM is most likely to filter by; if any of
    // them is missing, the widget shape changed in a meaningful way
    // and we should re-verify before shipping.
    for expected in ["Core Tyria", "Heart of Thorns", "End of Dragons"] {
        assert!(
            cats.contains(expected),
            "expected category `{expected}` in snapshot; got {cats:?}"
        );
    }
}

#[test]
fn snapshot_events_have_walkable_patterns() {
    let v: serde_json::Value = serde_json::from_str(WIDGET_SNAPSHOT).unwrap();
    let raw = EventScheduleRaw::from_json_value(v).unwrap();
    // The walker needs either `pattern` or `sequences.partial`
    // populated. If most events had neither, the schedule tool would
    // return empty no matter what time it was called.
    let walkable = raw
        .events
        .values()
        .filter(|e| !e.sequences.pattern.is_empty() || !e.sequences.partial.is_empty())
        .count();
    assert!(
        walkable >= raw.events.len() / 2,
        "expected at least half of events to be walkable; got {walkable}/{}",
        raw.events.len()
    );
}

// --- Walker-on-fixture tests -----------------------------------------
//
// The parser tests above prove the JSON deserialises; these prove the
// cycle walker produces sensible output when fed the real snapshot.
// Catches regressions where the parser shape and the walker drift
// (e.g. the walker still assumes a top-level `pattern` field after we
// move it under `sequences.pattern`).

#[tokio::test]
async fn walker_finds_day_and_night_at_utc_midnight() {
    // At UTC midnight on any day, the "Day and night" cycle is at
    // position 0 of its [Day:70, Dusk:5, Night:40, Dawn:5] pattern.
    let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
    let resp = walk_snapshot_at(now, None).await;

    let dn = resp
        .events
        .iter()
        .find(|e| e.event_name == "Day and night")
        .expect("Day and night must appear with within_minutes=1440");
    assert_eq!(dn.current_segment, "Day");
    assert_eq!(dn.current_segment_ends_in_minutes, 70);
    assert_eq!(dn.next_segment_name, "Dusk");
    assert_eq!(dn.next_segment_starts_in_minutes, 70);
    // next_segment_starts_at was dropped as redundant — by
    // construction it always equals current_segment_ends_at.
    assert_eq!(dn.current_segment_total_minutes, 70);
}

#[tokio::test]
async fn walker_invariants_hold_for_all_snapshot_events() {
    // Pick a non-midnight time so any "midnight is special-case" bugs
    // surface. 07:23 UTC is arbitrary but reproducible.
    let now = Utc.with_ymd_and_hms(2026, 5, 12, 7, 23, 0).unwrap();
    let resp = walk_snapshot_at(now, None).await;

    assert!(
        resp.events.len() >= 20,
        "expected at least 20 walkable events at 07:23 with 1440-min window; got {}",
        resp.events.len()
    );

    // Per-event invariants: end-of-current and start-of-next must
    // match (the walker reports the same boundary on both sides); the
    // minute delta must agree with itself; and at least one of
    // current/next must have a name (otherwise the cycle is "gap →
    // gap" which would be a meaningless row to surface).
    //
    // We deliberately do NOT assert that `next_segment_name` is
    // anything specific — "Hard world bosses" alternates real bosses
    // with explicit gap slots (`r == 0`), so the gap (now surfaced as
    // `"(idle)"`) is sometimes the next thing on the schedule.
    for occ in &resp.events {
        assert_eq!(
            occ.current_segment_ends_in_minutes, occ.next_segment_starts_in_minutes,
            "{}: minute deltas disagree (next must coincide with end-of-current)",
            occ.event_name
        );
        assert!(
            occ.next_segment_starts_in_minutes >= 0,
            "{}: next segment can't be in the past",
            occ.event_name
        );
        assert!(
            !occ.current_segment.is_empty() && !occ.next_segment_name.is_empty(),
            "{}: both current and next segments must be populated (gap slots are surfaced as `(idle)`)",
            occ.event_name
        );
        assert!(
            occ.current_segment_total_minutes >= 1,
            "{}: current_segment_total_minutes should be >= 1",
            occ.event_name
        );
    }
}

#[tokio::test]
async fn walker_excludes_festival_events_by_default() {
    let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
    let resp = walk_snapshot_at(now, None).await;
    let festival_rows: Vec<_> = resp
        .events
        .iter()
        .filter(|e| e.category == "Special Events")
        .collect();
    assert!(
        festival_rows.is_empty(),
        "festival events must be excluded when active_festivals is empty; got: {festival_rows:#?}"
    );
}

#[tokio::test]
async fn walker_includes_festival_events_when_opted_in() {
    let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
    let resp = walk_snapshot_at(
        now,
        Some(vec!["Halloween".to_owned(), "Dragon Bash".to_owned()]),
    )
    .await;
    let names: std::collections::BTreeSet<&str> = resp
        .events
        .iter()
        .filter(|e| e.category == "Special Events")
        .map(|e| e.event_name.as_str())
        .collect();
    // Snapshot carries Halloween + Dragon Bash; Labyrinthine Cliffs
    // stays gated because the LLM didn't opt in to Festival of the
    // Four Winds.
    assert!(names.contains("Halloween"), "got: {names:?}");
    assert!(names.contains("Dragon Bash"), "got: {names:?}");
    assert!(!names.contains("Labyrinthine Cliffs"), "got: {names:?}");
}

#[tokio::test]
async fn cache_round_trip_preserves_full_event_set() {
    // The service caches the *raw* widget JSON keyed by EVENT_SCHEDULE_CACHE_KEY.
    // Pulled out of cache, it's reparsed by EventScheduleRaw::from_json_value,
    // so the encoder (raw_as_value) and decoder (from_json_value) have to
    // produce a round-trip. If they ever drift, the second call here will
    // either hit the warn-then-refetch path (call count > 1) or return a
    // truncated event set.
    use gw2_mcp::ports::{
        BuildCodeDecoder, Cache, CatalogRegistry, Clock, EventSchedule, Gw2Api, MapData,
        MumbleLink, Wiki,
    };
    use std::sync::Arc;

    let v: serde_json::Value = serde_json::from_str(WIDGET_SNAPSHOT).unwrap();
    let raw = EventScheduleRaw::from_json_value(v).expect("parse");
    let expected_event_count = raw.events.len();

    let fake = Arc::new(common::FakeEventSchedule::empty());
    fake.set_raw(raw);

    let now = Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap();
    let clock = common::TestClock::at(now);
    let cache = common::TestCache::new(clock.clone());
    let gw2 = common::FakeGw2Api::new();
    let wiki = common::FakeWiki::new();

    let gw2_arc: Arc<dyn Gw2Api> = gw2;
    let wiki_arc: Arc<dyn Wiki> = wiki;
    let cache_arc: Arc<dyn Cache> = cache;
    let clock_arc: Arc<dyn Clock> = clock;
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(gw2_mcp::adapters::ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    let mumble: Arc<dyn MumbleLink> = Arc::new(gw2_mcp::adapters::StubMumbleLink::new("test"));
    let maps: Arc<dyn MapData> = Arc::new(common::FakeMapData::new());
    let events: Arc<dyn EventSchedule> = fake.clone();

    let service = gw2_mcp::service::Service::new(
        gw2_arc, wiki_arc, cache_arc, clock_arc, decoder, catalogs, mumble, maps, events,
    );

    let make_filters = || gw2_mcp::service::EventScheduleFilters {
        within_minutes: 1440,
        category: None,
        player_access: None,
        player_access_source: None,
        active_festivals: ["Halloween", "Dragon Bash", "Festival of the Four Winds"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };

    let first = service.get_event_schedule(make_filters()).await.unwrap();
    assert_eq!(fake.calls(), 1, "first call must hit the adapter");

    let second = service.get_event_schedule(make_filters()).await.unwrap();
    assert_eq!(
        fake.calls(),
        1,
        "second call must be served from cache (no extra fetch_raw)"
    );

    // Both must observe the same event count — if the encoder dropped
    // fields the decoder cares about, the second call would either
    // refetch (caught above) or return a smaller set.
    assert!(
        !first.events.is_empty(),
        "first call should produce events; got empty"
    );
    assert_eq!(
        first.events.len(),
        second.events.len(),
        "cache round-trip changed visible event count"
    );

    // Widget version is read from `config.version` — verify the
    // encoder preserves it across the round-trip.
    assert_eq!(first.widget_version, second.widget_version);
    assert!(
        !second.widget_version.is_empty(),
        "widget version must survive the cache round-trip"
    );

    // Same active filters → same event set after the round-trip.
    let first_names: Vec<&str> = first.events.iter().map(|e| e.event_name.as_str()).collect();
    let second_names: Vec<&str> = second
        .events
        .iter()
        .map(|e| e.event_name.as_str())
        .collect();
    assert_eq!(first_names, second_names);

    // Sanity-check vs. raw input — we should be observing close to the
    // full snapshot (minus events with no walkable cycle, if any).
    assert!(
        first.events.len() >= expected_event_count / 2,
        "round-tripped event set lost too many events: {} vs {} in snapshot",
        first.events.len(),
        expected_event_count
    );
}

async fn walk_snapshot_at(
    now: chrono::DateTime<Utc>,
    active_festivals: Option<Vec<String>>,
) -> gw2_mcp::domain::EventScheduleResponse {
    use gw2_mcp::ports::{BuildCodeDecoder, Cache, Clock, EventSchedule, Gw2Api, MapData, Wiki};
    use gw2_mcp::ports::{CatalogRegistry, MumbleLink};
    use std::sync::Arc;

    let v: serde_json::Value = serde_json::from_str(WIDGET_SNAPSHOT).unwrap();
    let raw = EventScheduleRaw::from_json_value(v).expect("parse");

    let fake = Arc::new(common::FakeEventSchedule::empty());
    fake.set_raw(raw);

    let clock = common::TestClock::at(now);
    let cache = common::TestCache::new(clock.clone());
    let gw2 = common::FakeGw2Api::new();
    let wiki = common::FakeWiki::new();

    let gw2_arc: Arc<dyn Gw2Api> = gw2;
    let wiki_arc: Arc<dyn Wiki> = wiki;
    let cache_arc: Arc<dyn Cache> = cache;
    let clock_arc: Arc<dyn Clock> = clock;
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(gw2_mcp::adapters::ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    let mumble: Arc<dyn MumbleLink> = Arc::new(gw2_mcp::adapters::StubMumbleLink::new("test"));
    let maps: Arc<dyn MapData> = Arc::new(common::FakeMapData::new());
    let events: Arc<dyn EventSchedule> = fake.clone();

    let service = gw2_mcp::service::Service::new(
        gw2_arc, wiki_arc, cache_arc, clock_arc, decoder, catalogs, mumble, maps, events,
    );

    let filters = gw2_mcp::service::EventScheduleFilters {
        within_minutes: 1440,
        category: None,
        player_access: None,
        player_access_source: None,
        active_festivals: active_festivals.into_iter().flatten().collect(),
    };
    service.get_event_schedule(filters).await.expect("walk ok")
}
