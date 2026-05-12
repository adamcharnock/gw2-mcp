//! Festival schedule data — loaded from a YAML file maintained out-of-band
//! by the `refresh-festivals` Claude skill. We compile the YAML into the
//! binary with `include_str!` so the runtime never reads from disk and
//! the schedule is observable through git history.
//!
//! The data is intentionally approximate — see the file header in
//! `data/festivals.yaml` for the caveats.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Bumping this is a deliberate format-incompatibility signal. The
/// runtime refuses to load a schedule whose `schema_version` doesn't
/// match.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum FestivalScheduleError {
    #[error("could not parse festivals YAML: {0}")]
    Parse(String),

    #[error(
        "festivals.yaml schema_version = {got}, but this binary supports = {supported}. \
         Run the refresh-festivals skill to regenerate."
    )]
    UnsupportedSchemaVersion { got: u32, supported: u32 },

    #[error("festival entry {index} is malformed: {detail}")]
    MalformedEntry { index: usize, detail: String },
}

/// Wire-shape of `data/festivals.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct FestivalSchedule {
    pub schema_version: u32,
    /// `YYYY-MM-DD`. Set by the skill on every refresh; used to flag
    /// staleness in the runtime tool response.
    pub last_updated: String,
    #[serde(default)]
    pub sources: Vec<String>,
    pub festivals: Vec<FestivalEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FestivalEntry {
    pub name: String,
    /// `MM-DD`, year-agnostic.
    pub typical_start: String,
    /// `MM-DD`, year-agnostic. May wrap the year boundary (e.g. Wintersday
    /// runs Dec 12 → Jan 02) — in that case `end < start` lexicographically
    /// and runtime code unwraps it.
    pub typical_end: String,
    pub wiki_url: String,
}

impl FestivalSchedule {
    /// Parse + validate. Validation rejects: `schema_version` mismatch,
    /// malformed `MM-DD` strings, empty names, non-wiki URLs.
    pub fn parse(yaml: &str) -> Result<Self, FestivalScheduleError> {
        let s: Self = serde_yaml_bw::from_str(yaml)
            .map_err(|e| FestivalScheduleError::Parse(e.to_string()))?;
        if s.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(FestivalScheduleError::UnsupportedSchemaVersion {
                got: s.schema_version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }
        for (i, f) in s.festivals.iter().enumerate() {
            if f.name.trim().is_empty() {
                return Err(FestivalScheduleError::MalformedEntry {
                    index: i,
                    detail: "name is empty".to_owned(),
                });
            }
            check_mmdd(&f.typical_start).map_err(|d| FestivalScheduleError::MalformedEntry {
                index: i,
                detail: format!("typical_start: {d}"),
            })?;
            check_mmdd(&f.typical_end).map_err(|d| FestivalScheduleError::MalformedEntry {
                index: i,
                detail: format!("typical_end: {d}"),
            })?;
            if !f.wiki_url.starts_with("https://wiki.guildwars2.com/wiki/") {
                return Err(FestivalScheduleError::MalformedEntry {
                    index: i,
                    detail: "wiki_url must start with https://wiki.guildwars2.com/wiki/".into(),
                });
            }
        }
        Ok(s)
    }
}

/// Validate a `MM-DD` literal. We use string ops rather than chrono
/// because chrono can't parse a date without a year and we explicitly
/// want year-agnostic literals.
fn check_mmdd(s: &str) -> Result<(u32, u32), String> {
    let (m_str, d_str) = s
        .split_once('-')
        .ok_or_else(|| format!("expected MM-DD, got `{s}`"))?;
    if m_str.len() != 2 || d_str.len() != 2 {
        return Err(format!("expected MM-DD with two-digit fields, got `{s}`"));
    }
    let month: u32 = m_str
        .parse()
        .map_err(|_| format!("month is not a number: `{m_str}`"))?;
    let day: u32 = d_str
        .parse()
        .map_err(|_| format!("day is not a number: `{d_str}`"))?;
    if !(1..=12).contains(&month) {
        return Err(format!("month {month} out of range [1, 12]"));
    }
    let max_day = days_in_month(month);
    if !(1..=max_day).contains(&day) {
        return Err(format!(
            "day {day} out of range [1, {max_day}] for month {month}"
        ));
    }
    Ok((month, day))
}

/// Maximum day count for `MM-DD` validation. We allow Feb 29 to keep
/// the schedule leap-year-friendly without having to pick a reference
/// year.
const fn days_in_month(m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 29,
        _ => 0,
    }
}

/// Parse the embedded festivals YAML. Public so the schema test in
/// `tests/festivals_schema.rs` can re-validate it.
#[must_use]
pub fn embedded_yaml() -> &'static str {
    include_str!("../../data/festivals.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_yaml_parses_cleanly() {
        let schedule = FestivalSchedule::parse(embedded_yaml()).unwrap();
        assert_eq!(schedule.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert!(
            !schedule.festivals.is_empty(),
            "embedded YAML must declare at least one festival"
        );
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let yaml = r#"
schema_version: 99
last_updated: "2026-05-12"
festivals: []
"#;
        let err = FestivalSchedule::parse(yaml).unwrap_err();
        assert!(matches!(
            err,
            FestivalScheduleError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_bad_mmdd() {
        let yaml = r#"
schema_version: 1
last_updated: "2026-05-12"
festivals:
  - name: "Test"
    typical_start: "13-01"
    typical_end: "01-02"
    wiki_url: https://wiki.guildwars2.com/wiki/Test
"#;
        let err = FestivalSchedule::parse(yaml).unwrap_err();
        assert!(matches!(err, FestivalScheduleError::MalformedEntry { .. }));
    }

    #[test]
    fn accepts_feb_29() {
        check_mmdd("02-29").unwrap();
    }

    #[test]
    fn rejects_feb_30() {
        assert!(check_mmdd("02-30").is_err());
    }
}
