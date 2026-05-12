//! Active-festival computation. The runtime loads the embedded
//! `data/festivals.yaml`, then asks: "for `today.month()/day()`,
//! which festivals overlap?" and "for the rest, when do they next
//! start?".
//!
//! Year-wrap handling: a festival with `typical_end < typical_start`
//! (lexicographic, as `MM-DD`) wraps the year boundary — e.g.
//! Wintersday: 12-12 → 01-02. Live-detection treats those as live for
//! dates in `[start..=12-31] ∪ [01-01..=end]`.

use std::sync::OnceLock;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Service;
use crate::domain::festivals::{FestivalSchedule, embedded_yaml};

/// Top-level response from `get_active_festivals`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActiveFestivalsResponse {
    pub live: Vec<FestivalStatus>,
    pub upcoming: Vec<FestivalStatus>,
    pub schedule_version: u32,
    pub schedule_last_updated: String,
    /// Whole days from `schedule_last_updated` to now. Positive means
    /// "the schedule is N days stale". Negative would mean the file is
    /// dated in the future — surfaced as-is so we don't hide clock bugs.
    pub schedule_last_updated_days_ago: i64,
    /// Always `true`. Made an explicit field so the LLM can quote the
    /// disclaimer back to the user verbatim.
    pub approximate: bool,
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FestivalStatus {
    pub name: String,
    pub typical_start: String,
    pub typical_end: String,
    /// Populated when the festival is currently live. Whole days until
    /// the end of this year's window (year-wrap unwrapped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_in_days: Option<i64>,
    /// Populated when the festival is upcoming. Whole days until the
    /// next start (year-wrap unwrapped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_in_days: Option<i64>,
    pub wiki_url: String,
}

const APPROXIMATE_NOTE: &str = "Festival dates are approximate — derived from \
prior-year wiki occurrences, not official ArenaNet announcements. For \
authoritative start/end times, consult the GW2 news page.";

/// Parse + cache the embedded YAML on first access. Errors here would
/// have surfaced at `cargo test` via the embedded-YAML schema test, so
/// runtime panics here mean someone shipped a binary against an
/// invalid checked-in YAML — fail loud.
fn schedule() -> &'static FestivalSchedule {
    static SCHED: OnceLock<FestivalSchedule> = OnceLock::new();
    SCHED.get_or_init(|| {
        FestivalSchedule::parse(embedded_yaml())
            .expect("embedded festivals.yaml must parse — schema test should have caught this")
    })
}

impl Service {
    /// Compute the live + upcoming festival lists at `now`.
    pub fn get_active_festivals(&self) -> ActiveFestivalsResponse {
        let now = self.clock.now();
        build_response(schedule(), now)
    }
}

fn build_response(sched: &FestivalSchedule, now: DateTime<Utc>) -> ActiveFestivalsResponse {
    let today = now.date_naive();
    let mut live: Vec<FestivalStatus> = Vec::new();
    let mut upcoming: Vec<FestivalStatus> = Vec::new();

    for f in &sched.festivals {
        let Some((start_m, start_d)) = mmdd(&f.typical_start) else {
            continue;
        };
        let Some((end_m, end_d)) = mmdd(&f.typical_end) else {
            continue;
        };

        if is_live(today, (start_m, start_d), (end_m, end_d)) {
            let end_date = next_occurrence(today, end_m, end_d, /* allow_today: */ true);
            let ends_in_days = (end_date - today).num_days();
            live.push(FestivalStatus {
                name: f.name.clone(),
                typical_start: f.typical_start.clone(),
                typical_end: f.typical_end.clone(),
                ends_in_days: Some(ends_in_days),
                starts_in_days: None,
                wiki_url: f.wiki_url.clone(),
            });
        } else {
            let start_date =
                next_occurrence(today, start_m, start_d, /* allow_today: */ false);
            let starts_in_days = (start_date - today).num_days();
            upcoming.push(FestivalStatus {
                name: f.name.clone(),
                typical_start: f.typical_start.clone(),
                typical_end: f.typical_end.clone(),
                ends_in_days: None,
                starts_in_days: Some(starts_in_days),
                wiki_url: f.wiki_url.clone(),
            });
        }
    }

    upcoming.sort_by_key(|f| f.starts_in_days.unwrap_or(i64::MAX));

    let last_updated = sched.last_updated.clone();
    let schedule_last_updated_days_ago = NaiveDate::parse_from_str(&last_updated, "%Y-%m-%d")
        .map(|d| (today - d).num_days())
        .unwrap_or(0);

    ActiveFestivalsResponse {
        live,
        upcoming,
        schedule_version: sched.schema_version,
        schedule_last_updated: last_updated,
        schedule_last_updated_days_ago,
        approximate: true,
        note: APPROXIMATE_NOTE,
    }
}

/// `MM-DD` → `(m, d)`. Returns `None` for malformed values; the schema
/// test guarantees every embedded entry is well-formed, so this only
/// matters defensively.
fn mmdd(s: &str) -> Option<(u32, u32)> {
    let (m, d) = s.split_once('-')?;
    Some((m.parse().ok()?, d.parse().ok()?))
}

/// Does `today` fall inside `[start..=end]` (year-agnostic)? Handles
/// the year-wrap case where `end < start`.
fn is_live(today: NaiveDate, start: (u32, u32), end: (u32, u32)) -> bool {
    let cur = (today.month(), today.day());
    if cmp_md(start, end) <= std::cmp::Ordering::Equal {
        cmp_md(start, cur) <= std::cmp::Ordering::Equal
            && cmp_md(cur, end) <= std::cmp::Ordering::Equal
    } else {
        // Wraps year boundary.
        cmp_md(start, cur) <= std::cmp::Ordering::Equal
            || cmp_md(cur, end) <= std::cmp::Ordering::Equal
    }
}

fn cmp_md(a: (u32, u32), b: (u32, u32)) -> std::cmp::Ordering {
    a.cmp(&b)
}

/// The first calendar date on or after `today` that lands on
/// `(month, day)`. If `allow_today` is `true` and today already
/// matches, `today` itself is returned; otherwise we skip to next
/// year. Feb 29 in a non-leap year resolves to Feb 28.
fn next_occurrence(today: NaiveDate, month: u32, day: u32, allow_today: bool) -> NaiveDate {
    fn make_safe(year: i32, month: u32, day: u32) -> NaiveDate {
        if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
            return d;
        }
        // Feb 29 in a non-leap year; fall back to the 28th.
        NaiveDate::from_ymd_opt(year, month, day.saturating_sub(1))
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, month, 28).unwrap())
    }
    let candidate = make_safe(today.year(), month, day);
    let on_or_after = if allow_today {
        candidate >= today
    } else {
        candidate > today
    };
    if on_or_after {
        candidate
    } else {
        let next_year = today.year() + 1;
        make_safe(next_year, month, day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::festivals::FestivalEntry;
    use chrono::TimeZone;

    fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap()
    }

    fn schedule_with(entries: Vec<FestivalEntry>) -> FestivalSchedule {
        FestivalSchedule {
            schema_version: 1,
            last_updated: "2026-05-12".to_owned(),
            sources: Vec::new(),
            festivals: entries,
        }
    }

    fn entry(name: &str, start: &str, end: &str) -> FestivalEntry {
        FestivalEntry {
            name: name.to_owned(),
            typical_start: start.to_owned(),
            typical_end: end.to_owned(),
            wiki_url: format!("https://wiki.guildwars2.com/wiki/{name}"),
        }
    }

    #[test]
    fn live_festival_mid_window_reports_days_until_end() {
        let sched = schedule_with(vec![entry("Dragon Bash", "06-07", "06-28")]);
        // 2026-06-15 — well inside the window.
        let resp = build_response(&sched, ts(2026, 6, 15));
        assert_eq!(resp.live.len(), 1);
        assert!(resp.upcoming.is_empty());
        let f = &resp.live[0];
        assert_eq!(f.name, "Dragon Bash");
        assert_eq!(f.ends_in_days, Some(13));
    }

    #[test]
    fn upcoming_festival_reports_days_until_start() {
        let sched = schedule_with(vec![entry("Dragon Bash", "06-07", "06-28")]);
        // 2026-05-30: 8 days until Jun 07.
        let resp = build_response(&sched, ts(2026, 5, 30));
        assert_eq!(resp.upcoming.len(), 1);
        assert!(resp.live.is_empty());
        assert_eq!(resp.upcoming[0].starts_in_days, Some(8));
    }

    #[test]
    fn wintersday_year_wrap_dec_30_is_live() {
        let sched = schedule_with(vec![entry("Wintersday", "12-12", "01-02")]);
        let resp = build_response(&sched, ts(2026, 12, 30));
        assert_eq!(resp.live.len(), 1, "Dec 30 is inside Wintersday window");
        // End is Jan 02 of next year → 3 days from Dec 30.
        assert_eq!(resp.live[0].ends_in_days, Some(3));
    }

    #[test]
    fn wintersday_year_wrap_jan_1_is_live() {
        let sched = schedule_with(vec![entry("Wintersday", "12-12", "01-02")]);
        let resp = build_response(&sched, ts(2027, 1, 1));
        assert_eq!(resp.live.len(), 1, "Jan 1 is inside Wintersday window");
        assert_eq!(resp.live[0].ends_in_days, Some(1));
    }

    #[test]
    fn upcoming_sorted_by_days_until() {
        let sched = schedule_with(vec![
            entry("Lunar New Year", "01-25", "02-08"),
            entry("Halloween", "10-13", "11-03"),
            entry("Dragon Bash", "06-07", "06-28"),
        ]);
        // 2026-05-12 — well before all three.
        let resp = build_response(&sched, ts(2026, 5, 12));
        assert!(resp.live.is_empty());
        assert_eq!(resp.upcoming.len(), 3);
        let names: Vec<&str> = resp.upcoming.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Dragon Bash", "Halloween", "Lunar New Year"]);
    }

    #[test]
    fn approximate_flag_always_true() {
        let sched = schedule_with(vec![entry("Halloween", "10-13", "11-03")]);
        let resp = build_response(&sched, ts(2026, 5, 12));
        assert!(resp.approximate);
        assert!(resp.note.contains("approximate"));
    }

    #[test]
    fn schedule_last_updated_days_ago_is_correct() {
        let sched = schedule_with(vec![entry("Halloween", "10-13", "11-03")]);
        // last_updated is 2026-05-12; now is 2026-06-01 → 20 days.
        let resp = build_response(&sched, ts(2026, 6, 1));
        assert_eq!(resp.schedule_last_updated_days_ago, 20);
    }

    #[test]
    fn feb_29_in_non_leap_year_resolves_to_feb_28() {
        // 2026 is not a leap year. A festival starting Feb 29 falls
        // back to Feb 28.
        let sched = schedule_with(vec![entry("Synthetic", "02-29", "03-05")]);
        let resp = build_response(&sched, ts(2026, 2, 25));
        // Feb 29 → Feb 28 in 2026; that's 3 days from Feb 25.
        assert_eq!(resp.upcoming[0].starts_in_days, Some(3));
    }
}
