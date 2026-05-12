//! Regression tests for the `Event_timer` widget parser against a real
//! widget snapshot.
//!
//! The widget JSON nests events under an `events` object (not the top
//! level — a previous version of the parser assumed the latter and
//! silently dropped all 43 entries, returning an empty schedule).
//! Pinning a real-shaped fixture catches a recurrence at `cargo test`
//! time instead of via a "tool returns []" bug report.

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
