//! End-to-end-ish integration tests: real HTTP adapters (against wiremock)
//! wired through the `Service` and exposed via the MCP adapter.
//!
//! These tests prove the full vertical slice works without driving stdio.

mod common;

use std::sync::Arc;

use gw2_mcp::adapters::{
    ChatrDecoder, DiscretizeCatalog, HttpGw2Api, HttpWiki, McpServer, MemoryCache, SystemClock,
};
use gw2_mcp::ports::{BuildCatalog, BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, Wiki};
use gw2_mcp::service::Service;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::valid_api_key;

fn build_server(gw2_uri: String, wiki_uri: String) -> McpServer {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(clock.clone()));
    let gw2: Arc<dyn Gw2Api> = Arc::new(HttpGw2Api::with_base_url(gw2_uri).unwrap());
    let wiki: Arc<dyn Wiki> = Arc::new(HttpWiki::with_base_url(wiki_uri).unwrap());
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    McpServer::new(Service::new(gw2, wiki, cache, clock, decoder, catalogs))
}

/// Build server with one Discretize catalog wired in (mock at `gh_uri`).
fn build_server_with_discretize(gh_api: String, gh_raw: String) -> McpServer {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(clock.clone()));
    let gw2: Arc<dyn Gw2Api> =
        Arc::new(HttpGw2Api::with_base_url("http://unused.invalid".to_owned()).unwrap());
    let wiki: Arc<dyn Wiki> =
        Arc::new(HttpWiki::with_base_url("http://unused.invalid/".to_owned()).unwrap());
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let discretize: Arc<dyn BuildCatalog> =
        Arc::new(DiscretizeCatalog::with_bases(gh_api, gh_raw).unwrap());
    let catalogs = Arc::new(CatalogRegistry::new().with(discretize));
    McpServer::new(Service::new(gw2, wiki, cache, clock, decoder, catalogs))
}

#[tokio::test]
async fn end_to_end_wiki_search_returns_json_with_url_and_extract() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "query": {
                "search": [{
                    "ns": 0,
                    "title": "Dragon Bash",
                    "pageid": 12345,
                    "size": 5000,
                    "wordcount": 800,
                    "snippet": "<span class=\"searchmatch\">Dragon</span> Bash",
                    "timestamp": "2026-01-01T00:00:00Z"
                }]
            }
        })))
        .mount(&server)
        .await;

    // Extract responses share the same handler — we'll let wiremock match on
    // *any* request. wiremock returns the first registered match; the search
    // body above is shaped so the extract field will be missing, which the
    // service treats as "no extract".
    Mock::given(method("GET"))
        .and(path("/extract-stub")) // never matched, but registered to keep server alive
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mcp = build_server(
        "http://unused.invalid".to_owned(),
        format!("{}/", server.uri()),
    );

    let result = mcp
        .dispatch_tool("wiki_search", json!({ "query": "Dragon Bash", "limit": 1 }))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let results = parsed["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Dragon Bash");
    assert!(
        results[0]["url"].as_str().unwrap().contains("Dragon"),
        "service must populate url"
    );
}

#[tokio::test]
async fn end_to_end_wiki_search_rejects_empty_query() {
    let server = MockServer::start().await;
    let mcp = build_server(
        "http://unused.invalid".to_owned(),
        format!("{}/", server.uri()),
    );

    let err = mcp
        .dispatch_tool("wiki_search", json!({ "query": "" }))
        .await
        .unwrap_err();
    assert!(
        err.contains("empty"),
        "expected validation error, got: {err}"
    );
}

#[tokio::test]
async fn end_to_end_get_currencies_with_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "Coin", "description": "Coins.", "icon": "https://x/coin.png", "order": 101}
        ])))
        .mount(&server)
        .await;

    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let result = mcp
        .dispatch_tool("get_currencies", json!({ "ids": [1] }))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["1"]["name"], "Coin");
}

#[tokio::test]
async fn end_to_end_get_wallet_with_invalid_key_returns_error() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());

    let err = mcp
        .dispatch_tool("get_wallet", json!({ "api_key": "too-short" }))
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("api"), "got: {err}");
}

#[tokio::test]
async fn end_to_end_get_wallet_with_valid_key_returns_json() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "value": 12345}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "Coin", "description": "Coins.", "icon": "https://x.png", "order": 1}
        ])))
        .mount(&server)
        .await;

    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let result = mcp
        .dispatch_tool("get_wallet", json!({ "api_key": valid_api_key().expose() }))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["entries"][0]["value"], 12345);
    assert_eq!(parsed["total_currencies"], 1);
}

#[tokio::test]
async fn end_to_end_unknown_tool_errors() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let err = mcp
        .dispatch_tool("not_a_tool", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("unknown tool"));
}

#[tokio::test]
async fn end_to_end_currencies_resource_returns_full_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([1, 2])))
        .up_to_n_times(1) // first call: list of ids
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/currencies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "Coin",  "description": "Coins.", "icon": "https://x.png", "order": 1},
            {"id": 2, "name": "Karma", "description": "Karma.", "icon": "https://y.png", "order": 2}
        ])))
        .mount(&server)
        .await;

    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let body = mcp
        .read_resource_for_test("gw2://currencies")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["1"]["name"], "Coin");
    assert_eq!(parsed["2"]["name"], "Karma");
}

#[tokio::test]
async fn end_to_end_decode_build_code_returns_structured_json() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let result = mcp
        .dispatch_tool(
            "decode_build_code",
            json!({"code": "[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]"}),
        )
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["profession"], 6);
    assert_eq!(parsed["skills"]["healing"]["terrestrial"], 116);
}

#[tokio::test]
async fn end_to_end_decode_build_code_rejects_malformed() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let err = mcp
        .dispatch_tool("decode_build_code", json!({"code": "not bracketed"}))
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("malformed") || err.to_lowercase().contains("validation"));
}

#[tokio::test]
async fn end_to_end_get_skills_requires_ids() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let err = mcp
        .dispatch_tool("get_skills", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("ids"), "expected ids-required error: {err}");
}

#[tokio::test]
async fn end_to_end_list_build_sources_returns_registered_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"tree":[]}"#))
        .mount(&server)
        .await;
    let mcp = build_server_with_discretize(server.uri(), "http://unused.invalid".to_owned());
    let result = mcp
        .dispatch_tool("list_build_sources", json!({}))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert!(
        parsed.as_array().unwrap().iter().any(|v| v == "discretize"),
        "list_build_sources must include `discretize` after registration; got {parsed}"
    );
}

#[tokio::test]
async fn end_to_end_list_recommended_builds_unknown_source_errors() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let err = mcp
        .dispatch_tool(
            "list_recommended_builds",
            json!({"source": "no-such-source"}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("no such build source"), "got: {err}");
}

#[tokio::test]
async fn end_to_end_list_recommended_builds_via_discretize() {
    let server = MockServer::start().await;
    // Stripped tree fixture so we don't depend on the full one in this crate.
    let small_tree = r#"{"tree":[
        {"path":"builds/guardian/power-dragonhunter/index.md","type":"blob"},
        {"path":"builds/guardian/condi-firebrand/index.md","type":"blob"},
        {"path":"builds/elementalist/power-tempest/index.md","type":"blob"},
        {"path":"README.md","type":"blob"}
    ]}"#;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(small_tree))
        .mount(&server)
        .await;

    let mcp = build_server_with_discretize(server.uri(), "http://unused.invalid".to_owned());
    let result = mcp
        .dispatch_tool(
            "list_recommended_builds",
            json!({"source": "discretize", "profession": "guardian"}),
        )
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2, "expected exactly the two guardian builds");
    for v in arr {
        assert_eq!(v["profession"], "Guardian");
    }
}

#[tokio::test]
async fn end_to_end_unknown_resource_uri_errors() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .read_resource_for_test("gw2://does-not-exist")
        .await
        .unwrap_err();
    assert!(err.contains("not_found") || err.contains("not found"));
}
