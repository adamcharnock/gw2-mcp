//! Wall-clock helpers for GW2's reset cadences:
//! - **Raids**: Monday 07:30 UTC (weekly).
//! - **Dungeons / daily achievements**: 00:00 UTC (daily).
//!
//! Both helpers take `now: DateTime<Utc>` and return the *next* reset
//! strictly after `now` (i.e. if `now` is exactly the reset boundary,
//! we return the boundary one cycle later). Pinning `now` instead of
//! reading the clock internally keeps the helpers pure and testable.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc, Weekday};

/// Next weekly raid reset strictly after `now`.
///
/// GW2 raid clears reset every Monday at 07:30 UTC. We compute the
/// candidate "Monday 07:30 UTC of this week" and advance by 7 days if
/// it's not strictly in the future.
#[must_use]
pub fn next_raid_reset(now: DateTime<Utc>) -> DateTime<Utc> {
    let monday = now.date_naive() - Duration::days(i64::from(weekday_offset(now.weekday())));
    let candidate = Utc
        .with_ymd_and_hms(monday.year(), monday.month(), monday.day(), 7, 30, 0)
        .single()
        .expect("Monday 07:30 UTC is always a valid timestamp");
    if candidate > now {
        candidate
    } else {
        candidate + Duration::days(7)
    }
}

/// Next daily reset strictly after `now`. Daily reset is 00:00 UTC, so
/// this is "tomorrow at midnight UTC" — or "today at midnight" if
/// `now` is exactly at midnight (we never return `now`).
#[must_use]
pub fn next_daily_reset(now: DateTime<Utc>) -> DateTime<Utc> {
    let today_midnight = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()
        .expect("midnight UTC is always valid");
    if today_midnight > now {
        today_midnight
    } else {
        today_midnight + Duration::days(1)
    }
}

/// Days back from this weekday to the most recent Monday (Mon→0,
/// Tue→1, …, Sun→6). Trivial but extracted for clarity.
fn weekday_offset(w: Weekday) -> u8 {
    match w {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn raid_reset_skips_past_boundary_in_current_week() {
        // Monday 06:00 UTC → reset is later today at 07:30.
        let now = dt(2026, 5, 11, 6, 0); // Monday
        assert_eq!(next_raid_reset(now), dt(2026, 5, 11, 7, 30));
    }

    #[test]
    fn raid_reset_rolls_forward_when_past_this_weeks_boundary() {
        // Monday 08:00 UTC → already past this week's 07:30 reset; next
        // is the following Monday 07:30.
        let now = dt(2026, 5, 11, 8, 0); // Monday after reset
        assert_eq!(next_raid_reset(now), dt(2026, 5, 18, 7, 30));
    }

    #[test]
    fn raid_reset_from_mid_week() {
        // Thursday → reset is next Monday 07:30.
        let now = dt(2026, 5, 14, 12, 0); // Thursday
        assert_eq!(next_raid_reset(now), dt(2026, 5, 18, 7, 30));
    }

    #[test]
    fn raid_reset_handles_sunday() {
        // Sunday 23:00 → next reset is Monday (tomorrow) 07:30.
        let now = dt(2026, 5, 10, 23, 0); // Sunday
        assert_eq!(next_raid_reset(now), dt(2026, 5, 11, 7, 30));
    }

    #[test]
    fn raid_reset_strictly_after_now_at_exact_boundary() {
        // Exactly 07:30 UTC on Monday → next is one week later.
        let now = dt(2026, 5, 11, 7, 30);
        assert_eq!(next_raid_reset(now), dt(2026, 5, 18, 7, 30));
    }

    #[test]
    fn daily_reset_is_tomorrow_midnight() {
        let now = dt(2026, 5, 11, 12, 0);
        assert_eq!(next_daily_reset(now), dt(2026, 5, 12, 0, 0));
    }

    #[test]
    fn daily_reset_strictly_after_now_at_exact_boundary() {
        let now = dt(2026, 5, 11, 0, 0);
        assert_eq!(next_daily_reset(now), dt(2026, 5, 12, 0, 0));
    }

    #[test]
    fn daily_reset_just_before_midnight() {
        let now = dt(2026, 5, 11, 23, 59);
        assert_eq!(next_daily_reset(now), dt(2026, 5, 12, 0, 0));
    }
}
