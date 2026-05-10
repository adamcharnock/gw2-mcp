//! Integration tests for [`HttpGw2Api`] using `wiremock`.
//!
//! Verifies the wire format of every request and the parsing of every
//! supported response — the pieces the `FakeGw2Api` skips.

mod common;

use gw2_mcp::adapters::HttpGw2Api;
use gw2_mcp::domain::CurrencyId;
use gw2_mcp::ports::{Gw2Api, Gw2ApiError};
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::valid_api_key;

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
async fn fetch_wallet_5xx_maps_to_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let api = HttpGw2Api::with_base_url(server.uri()).unwrap();
    let err = api.fetch_wallet(&valid_api_key()).await.unwrap_err();
    match err {
        Gw2ApiError::Status { status, body } => {
            assert_eq!(status, 503);
            assert!(body.contains("upstream down"));
        }
        other => panic!("expected Status error, got {other:?}"),
    }
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
