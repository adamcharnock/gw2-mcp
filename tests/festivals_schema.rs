//! Schema-validation tests for `data/festivals.yaml`. These run as part
//! of the normal `cargo test` pass and protect the runtime against
//! malformed YAML being checked in by the `refresh-festivals` skill.

use gw2_mcp::domain::festivals::{FestivalSchedule, SUPPORTED_SCHEMA_VERSION, embedded_yaml};

#[test]
fn embedded_festivals_yaml_parses() {
    FestivalSchedule::parse(embedded_yaml()).expect("data/festivals.yaml must parse cleanly");
}

#[test]
fn embedded_schedule_has_at_least_one_festival() {
    let s = FestivalSchedule::parse(embedded_yaml()).unwrap();
    assert!(
        !s.festivals.is_empty(),
        "data/festivals.yaml must declare at least one festival"
    );
}

#[test]
fn embedded_schedule_matches_supported_schema_version() {
    let s = FestivalSchedule::parse(embedded_yaml()).unwrap();
    assert_eq!(s.schema_version, SUPPORTED_SCHEMA_VERSION);
}

#[test]
fn embedded_last_updated_is_iso_date() {
    let s = FestivalSchedule::parse(embedded_yaml()).unwrap();
    chrono::NaiveDate::parse_from_str(&s.last_updated, "%Y-%m-%d")
        .expect("last_updated must be YYYY-MM-DD");
}

#[test]
fn every_festival_has_a_wiki_source_url() {
    let s = FestivalSchedule::parse(embedded_yaml()).unwrap();
    for f in &s.festivals {
        assert!(
            f.wiki_url.starts_with("https://wiki.guildwars2.com/wiki/"),
            "{}: wiki_url must point at wiki.guildwars2.com",
            f.name
        );
    }
}
