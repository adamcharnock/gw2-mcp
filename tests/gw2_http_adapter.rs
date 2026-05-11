//! Integration tests for [`HttpGw2Api`] using `wiremock`.
//!
//! Verifies the wire format of every request and the parsing of every
//! supported response — the pieces the `FakeGw2Api` skips.

mod common;

use gw2_mcp::adapters::HttpGw2Api;
use gw2_mcp::domain::{
    AchievementId, CharacterName, CurrencyId, ItemId, SkillId, SpecializationId, TraitId,
};
use gw2_mcp::ports::{Gw2Api, Gw2ApiError};
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::valid_api_key;

const SKILLS_FIXTURE: &str = include_str!("fixtures/gw2_skills.json");
const TRAITS_FIXTURE: &str = include_str!("fixtures/gw2_traits.json");
const SPECS_FIXTURE: &str = include_str!("fixtures/gw2_specializations.json");
const BUILDTABS_FIXTURE: &str = include_str!("fixtures/buildtabs_sample.json");
const EQUIPMENTTABS_FIXTURE: &str = include_str!("fixtures/equipmenttabs_sample.json");

#[tokio::test]
async fn fetch_wallet_sends_bearer_and_parses_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .and(header(
            "authorization",
            format!("Bearer {}", valid_api_key().expose()).as_str(),
        ))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "value": 12345},
            {"id": 2, "value": 9999}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let entries = api.fetch_wallet(&valid_api_key()).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, CurrencyId::new(1).unwrap());
    assert_eq!(entries[0].value, 12345);
}

#[tokio::test]
async fn fetch_wallet_unauthorized_status_maps_to_unauthorized_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"text":"invalid key"}"#))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    assert!(
        matches!(err, Gw2ApiError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn fetch_wallet_5xx_maps_to_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    let pretty = format!("{err}");
    match err {
        Gw2ApiError::Upstream { status, message } => {
            assert_eq!(status, 503);
            assert!(message.contains("upstream down"));
        }
        other => panic!("expected Upstream error, got {other:?}"),
    }
    // The user-facing message should announce the API problem clearly.
    assert!(pretty.to_lowercase().contains("guild wars 2 api error"));
}

#[tokio::test]
async fn fetch_currency_ids_decodes_array_of_ints() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([1, 2, 3, 100])))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let ids = api.fetch_currency_ids().await.unwrap();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids[3], CurrencyId::new(100).unwrap());
}

#[tokio::test]
async fn fetch_currencies_passes_ids_param_and_decodes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .and(query_param("ids", "1,2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "Coin",  "description": "Coins.",  "icon": "https://x/coin.png",  "order": 101},
            {"id": 2, "name": "Karma", "description": "Karma.",  "icon": "https://x/karma.png", "order": 102}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_currencies(&[CurrencyId::new(1).unwrap(), CurrencyId::new(2).unwrap()])
        .await
        .unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map[&CurrencyId::new(1).unwrap()].name, "Coin");
    assert_eq!(map[&CurrencyId::new(2).unwrap()].name, "Karma");
}

#[tokio::test]
async fn fetch_currencies_empty_ids_short_circuits() {
    // No mock registered: any HTTP call would fail. We're asserting we
    // never make one when ids is empty.
    let server = MockServer::start().await;
    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api.fetch_currencies(&[]).await.unwrap();
    assert!(map.is_empty());
}

#[tokio::test]
async fn fetch_skills_decodes_real_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/skills"))
        .and(query_param("ids", "9137,5503"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SKILLS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_skills(&[SkillId::new(9137).unwrap(), SkillId::new(5503).unwrap()])
        .await
        .unwrap();
    assert_eq!(map.len(), 2);
    // Real skill 9137 = "Wave of Wrath" (Guardian greatsword auto).
    let skill = map.get(&SkillId::new(9137).unwrap()).unwrap();
    assert!(!skill.name.is_empty(), "name must round-trip");
    // Confirm extra fields preserved (e.g. `description` or `chat_link`).
    assert!(
        skill.extra.contains_key("description") || skill.extra.contains_key("chat_link"),
        "expected `description` or `chat_link` in extra; got keys: {:?}",
        skill.extra.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn fetch_traits_decodes_real_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/traits"))
        .and(query_param("ids", "648,214"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TRAITS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_traits(&[TraitId::new(648).unwrap(), TraitId::new(214).unwrap()])
        .await
        .unwrap();
    assert_eq!(map.len(), 2);
    // 648 = Zealot's Resolution.
    let t = map.get(&TraitId::new(648).unwrap()).unwrap();
    assert_eq!(t.name, "Zealot's Resolution");
}

#[tokio::test]
async fn fetch_items_decodes_real_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/items"))
        .and(query_param("ids", "95438"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 95438,
                "name": "Harrier's Marauder Hood",
                "type": "Armor",
                "rarity": "Ascended",
                "level": 80,
                "icon": "https://render.guildwars2.com/file/xxx.png",
                "details": {"type": "Helm", "weight_class": "Light"}
            }
        ])))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_items(&[ItemId::new(95438).unwrap()])
        .await
        .unwrap();
    assert_eq!(map.len(), 1);
    let item = map.get(&ItemId::new(95438).unwrap()).unwrap();
    assert_eq!(item.name, "Harrier's Marauder Hood");
    assert!(item.extra.contains_key("rarity"));
}

#[tokio::test]
async fn fetch_specializations_decodes_real_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/specializations"))
        .and(query_param("ids", "42,1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SPECS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_specializations(&[
            SpecializationId::new(42).unwrap(),
            SpecializationId::new(1).unwrap(),
        ])
        .await
        .unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(
        map.get(&SpecializationId::new(42).unwrap()).unwrap().name,
        "Zeal"
    );
}

#[tokio::test]
async fn fetch_buildtabs_returns_authed_array() {
    let server = MockServer::start().await;
    // Real GW2 path-encoding: space → %20, NOT + (form encoding). The earlier
    // `+` form passed the unit test but produced 400 "no such character"
    // against the live API.
    Mock::given(method("GET"))
        .and(path("/characters/Vesta%20Vey/buildtabs"))
        .and(query_param("tabs", "all"))
        .and(header(
            "authorization",
            format!("Bearer {}", valid_api_key().expose()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(BUILDTABS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let name = CharacterName::new("Vesta Vey").unwrap();
    let tabs = api.fetch_buildtabs(&valid_api_key(), &name).await.unwrap();
    // Real fixture has 3 build tabs.
    assert_eq!(tabs.len(), 3);
    assert_eq!(tabs[0]["build"]["profession"], "Guardian");
    assert_eq!(tabs[0]["build"]["specializations"][0]["id"], 42);
}

#[tokio::test]
async fn fetch_equipmenttabs_returns_authed_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters/Vesta%20Vey/equipmenttabs"))
        .and(query_param("tabs", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_string(EQUIPMENTTABS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let name = CharacterName::new("Vesta Vey").unwrap();
    let tabs = api
        .fetch_equipmenttabs(&valid_api_key(), &name)
        .await
        .unwrap();
    // Real fixture has 2 equipment tabs.
    assert_eq!(tabs.len(), 2);
    // First piece in tab 1 is the aquatic helm (real character data).
    assert_eq!(tabs[0]["equipment"][0]["slot"], "HelmAquatic");
}

#[tokio::test]
async fn fetch_buildtabs_translates_no_such_character_into_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters/Ghost/buildtabs"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"text":"no such character"}"#))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api
        .fetch_buildtabs(&valid_api_key(), &CharacterName::new("Ghost").unwrap())
        .await
        .unwrap_err();
    match err {
        Gw2ApiError::CharacterNotFound { name } => assert_eq!(name, "Ghost"),
        other => panic!("expected CharacterNotFound, got {other:?}"),
    }
    // And the rendered message must NOT contain the raw JSON noise.
    let pretty = format!(
        "{}",
        Gw2ApiError::CharacterNotFound {
            name: "Ghost".into()
        }
    );
    assert!(pretty.contains("Ghost"));
    assert!(
        !pretty.contains('{'),
        "rendered message must not echo raw JSON"
    );
}

#[tokio::test]
async fn fetch_429_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    assert!(matches!(err, Gw2ApiError::RateLimited(_)));
    let pretty = format!("{err}");
    assert!(pretty.to_lowercase().contains("rate"));
}

#[tokio::test]
async fn fetch_buildtabs_unauthorized_maps_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters/Hero/buildtabs"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid"))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api
        .fetch_buildtabs(&valid_api_key(), &CharacterName::new("Hero").unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, Gw2ApiError::Unauthorized));
}

#[tokio::test]
async fn fetch_403_with_requires_scope_emits_missing_scope_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"text":"requires scope wallet"}"#),
        )
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    match err {
        Gw2ApiError::MissingScope { needed } => assert_eq!(needed, "wallet"),
        other => panic!("expected MissingScope, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_403_without_scope_hint_still_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    assert!(matches!(err, Gw2ApiError::Unauthorized));
}

#[tokio::test]
async fn fetch_429_retries_once_on_short_retry_after() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));

    // First request: 429 with Retry-After: 1. Second: 200.
    let counter_a = counter.clone();
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(move |_req: &wiremock::Request| {
            let n = counter_a.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(429).insert_header("Retry-After", "1")
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": 1, "value": 100}
                ]))
            }
        })
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let started = std::time::Instant::now();
    let entries = api.fetch_wallet(&valid_api_key()).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(entries.len(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 2, "must have retried once");
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "must have honoured Retry-After: 1 (waited {elapsed:?})"
    );
}

#[tokio::test]
async fn fetch_429_returns_typed_error_after_single_retry_when_still_throttled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    match err {
        Gw2ApiError::RateLimited(Some(d)) => {
            assert_eq!(d.as_secs(), 1, "retry hint must be carried into the error");
        }
        Gw2ApiError::RateLimited(None) => panic!("expected typed retry-after value"),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_429_with_no_retry_after_does_not_retry_or_block() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(move |_req: &wiremock::Request| {
            c.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(429)
        })
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let started = std::time::Instant::now();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(matches!(err, Gw2ApiError::RateLimited(None)));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "must NOT retry without a Retry-After header"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "must not sleep when there's nothing to wait for"
    );
}

#[tokio::test]
async fn fetch_500_with_html_body_truncates_response() {
    let html = format!(
        "<!DOCTYPE html><html><body>{}</body></html>",
        "noisy ".repeat(2000)
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(503).set_body_string(html))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    match err {
        Gw2ApiError::Upstream { status, message } => {
            assert_eq!(status, 503);
            assert!(
                message.contains("HTML response"),
                "HTML body must collapse to the sentinel message; got: {message}"
            );
            assert!(
                message.len() < 200,
                "truncated HTML message must be short; got {} chars",
                message.len()
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Tier 6C: build number + bulk id list + achievements
// -----------------------------------------------------------------------

#[tokio::test]
async fn fetch_build_returns_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/build"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 123_456})))
        .mount(&server)
        .await;
    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    assert_eq!(api.fetch_build().await.unwrap(), 123_456);
}

#[tokio::test]
async fn fetch_all_skill_ids_decodes_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/skills"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([9137, 9138, 9139])))
        .mount(&server)
        .await;
    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let ids = api.fetch_all_skill_ids().await.unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], SkillId::new(9137).unwrap());
}

#[tokio::test]
async fn fetch_all_achievement_ids_decodes_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/achievements"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([1840, 283])))
        .mount(&server)
        .await;
    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let ids = api.fetch_all_achievement_ids().await.unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], AchievementId::new(1840).unwrap());
}

#[tokio::test]
async fn fetch_achievements_round_trips_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/achievements"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 1840,
                "name": "Daily Completionist",
                "description": "Complete 3 daily achievements.",
                "requirement": "Complete 3 daily achievements.",
                "type": "Default",
                "tiers": [{"count": 1, "points": 10}],
                "flags": ["Daily"]
            }
        ])))
        .mount(&server)
        .await;
    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let map = api
        .fetch_achievements(&[AchievementId::new(1840).unwrap()])
        .await
        .unwrap();
    let a = &map[&AchievementId::new(1840).unwrap()];
    assert_eq!(a.name, "Daily Completionist");
    assert!(a.extra.contains_key("tiers"));
    assert!(a.extra.contains_key("requirement"));
}

#[tokio::test]
async fn fetch_wallet_rejects_invalid_currency_id_in_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 0, "value": 1}])))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    assert!(
        matches!(err, Gw2ApiError::Decode(_)),
        "expected decode error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Tier 6A — account / progression / dailies wire tests.
// ---------------------------------------------------------------------------

const ACCOUNT_FIXTURE: &str = include_str!("fixtures/account_basic.json");
const ACHIEVEMENTS_FIXTURE: &str = include_str!("fixtures/account_achievements.json");
const MASTERIES_FIXTURE: &str = include_str!("fixtures/account_masteries.json");
const RAIDS_FIXTURE: &str = include_str!("fixtures/account_raids.json");
const DUNGEONS_FIXTURE: &str = include_str!("fixtures/account_dungeons.json");
const WIZARDS_VAULT_FIXTURE: &str = include_str!("fixtures/wizards_vault_daily.json");
const CHAR_LIST_FIXTURE: &str = include_str!("fixtures/account_characters_list.json");

#[tokio::test]
async fn fetch_account_sends_bearer_and_parses_full_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header(
            "authorization",
            format!("Bearer {}", valid_api_key().expose()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACCOUNT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let acc = api.fetch_account(&valid_api_key()).await.unwrap();
    assert_eq!(acc.name, "Snowflake.1234");
    assert_eq!(acc.fractal_level, Some(100));
    assert!(acc.access.iter().any(|s| s == "EndOfDragons"));
    assert!(acc.commander);
}

#[tokio::test]
async fn fetch_characters_list_returns_just_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAR_LIST_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let names = api.fetch_characters_list(&valid_api_key()).await.unwrap();
    assert_eq!(names.len(), 3);
    assert!(names.iter().any(|n| n == "Vesta Vey"));
}

#[tokio::test]
async fn fetch_account_achievements_parses_fixture() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/achievements"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACHIEVEMENTS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let entries = api
        .fetch_account_achievements(&valid_api_key())
        .await
        .unwrap();
    assert_eq!(entries.len(), 6);
    let entry_500 = entries.iter().find(|e| e.id == 500).unwrap();
    assert_eq!(entry_500.bits.as_ref().unwrap().len(), 7);
}

#[tokio::test]
async fn fetch_account_masteries_parses_fixture() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/masteries"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MASTERIES_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let masteries = api.fetch_account_masteries(&valid_api_key()).await.unwrap();
    assert_eq!(masteries.len(), 4);
    assert_eq!(masteries[1].level, 6);
}

#[tokio::test]
async fn fetch_account_raids_returns_string_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RAIDS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let raids = api.fetch_account_raids(&valid_api_key()).await.unwrap();
    assert!(raids.iter().any(|r| r == "vale_guardian"));
}

#[tokio::test]
async fn fetch_account_dungeons_returns_string_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/dungeons"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DUNGEONS_FIXTURE))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let dungeons = api.fetch_account_dungeons(&valid_api_key()).await.unwrap();
    assert_eq!(dungeons.len(), 2);
}

#[tokio::test]
async fn fetch_wizards_vault_daily_uses_correct_path_and_sends_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/daily"))
        .and(header(
            "authorization",
            format!("Bearer {}", valid_api_key().expose()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let t = api
        .fetch_wizards_vault_daily(&valid_api_key())
        .await
        .unwrap();
    assert_eq!(t.objectives.len(), 4);
    assert_eq!(t.objectives[0].title, "Complete an Event");
    assert_eq!(t.meta_reward_astral, 50);
    assert!(t.objectives[0].claimed);
}

#[tokio::test]
async fn fetch_wizards_vault_weekly_and_special_hit_their_paths() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/weekly"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/special"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let _w = api
        .fetch_wizards_vault_weekly(&valid_api_key())
        .await
        .unwrap();
    let _s = api
        .fetch_wizards_vault_special(&valid_api_key())
        .await
        .unwrap();
}

#[tokio::test]
async fn fetch_account_403_with_missing_scope_emits_typed_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/achievements"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string(r#"{"text":"requires scope progression"}"#),
        )
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api
        .fetch_account_achievements(&valid_api_key())
        .await
        .unwrap_err();
    match err {
        Gw2ApiError::MissingScope { needed } => assert_eq!(needed, "progression"),
        other => panic!("expected MissingScope, got {other:?}"),
    }
}
