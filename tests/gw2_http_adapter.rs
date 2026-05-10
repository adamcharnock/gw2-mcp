//! Integration tests for [`HttpGw2Api`] using `wiremock`.
//!
//! Verifies the wire format of every request and the parsing of every
//! supported response — the pieces the `FakeGw2Api` skips.

mod common;

use gw2_mcp::adapters::HttpGw2Api;
use gw2_mcp::domain::{CharacterName, CurrencyId, ItemId, SkillId, SpecializationId, TraitId};
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
    assert!(matches!(err, Gw2ApiError::RateLimited));
    let pretty = format!("{err}");
    assert!(pretty.to_lowercase().contains("rate limit"));
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
