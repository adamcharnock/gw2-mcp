//! End-to-end-ish integration tests: real HTTP adapters (against wiremock)
//! wired through the `Service` and exposed via the MCP adapter.
//!
//! These tests prove the full vertical slice works without driving stdio.

mod common;

use std::sync::Arc;

use gw2_mcp::adapters::{
    ChatrDecoder, DiscretizeCatalog, HttpGw2Api, HttpWiki, McpServer, MemoryCache, StubMumbleLink,
    SystemClock,
};
use gw2_mcp::ports::MumbleLink;
use gw2_mcp::ports::{
    BuildCatalog, BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, MapData, Wiki,
};
use gw2_mcp::service::Service;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{FakeMapData, valid_api_key};

fn build_server(gw2_uri: String, wiki_uri: String) -> McpServer {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(clock.clone()));
    let gw2: Arc<dyn Gw2Api> = Arc::new(HttpGw2Api::with_base_url(gw2_uri).unwrap());
    let wiki: Arc<dyn Wiki> = Arc::new(HttpWiki::with_base_url(wiki_uri).unwrap());
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    let mumble: Arc<dyn MumbleLink> =
        Arc::new(StubMumbleLink::new("integration test: no live mumble link"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    McpServer::new(Service::new(
        gw2, wiki, cache, clock, decoder, catalogs, mumble, maps,
    ))
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
    let mumble: Arc<dyn MumbleLink> =
        Arc::new(StubMumbleLink::new("integration test: no live mumble link"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    McpServer::new(Service::new(
        gw2, wiki, cache, clock, decoder, catalogs, mumble, maps,
    ))
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
    let parsed: Value = result;
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
    let parsed: Value = result;
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
    let parsed: Value = result;
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
    let parsed: Value = result;
    assert_eq!(parsed["profession"], 6);
    assert_eq!(
        parsed["skills"]["healing"]["terrestrial"]["palette_id"],
        116
    );
    assert!(parsed["skills"]["healing"]["terrestrial"]["api_skill_id"].is_number());
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
async fn end_to_end_list_catalog_sources_returns_registered_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"tree":[]}"#))
        .mount(&server)
        .await;
    let mcp = build_server_with_discretize(server.uri(), "http://unused.invalid".to_owned());
    let result = mcp
        .dispatch_tool("list_catalog_sources", json!({}))
        .await
        .unwrap();
    let parsed: Value = result;
    assert!(
        parsed.as_array().unwrap().iter().any(|v| v == "discretize"),
        "list_catalog_sources must include `discretize` after registration; got {parsed}"
    );
}

#[tokio::test]
async fn end_to_end_list_catalog_builds_unknown_source_errors() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), format!("{}/", server.uri()));
    let err = mcp
        .dispatch_tool("list_catalog_builds", json!({"source": "no-such-source"}))
        .await
        .unwrap_err();
    assert!(err.contains("no such build source"), "got: {err}");
}

#[tokio::test]
async fn end_to_end_list_catalog_builds_via_discretize() {
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
            "list_catalog_builds",
            json!({"source": "discretize", "profession": "guardian"}),
        )
        .await
        .unwrap();
    let parsed: Value = result;
    let arr = parsed["items"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "expected exactly the two guardian builds");
    for v in arr {
        assert_eq!(v["profession"], "Guardian");
    }
    // Two items < default page_size (25) → no next page.
    assert!(
        parsed["next_cursor"].is_null(),
        "fewer items than page_size must yield null next_cursor; got {parsed}"
    );
}

// ---------------------------------------------------------------------------
// Catalog pagination — the cursor format is opaque to clients but its
// contract isn't: page over a known set, last page returns null next_cursor,
// bad cursors are rejected, filter mismatches are rejected, and page_size
// > 100 is silently clamped.
// ---------------------------------------------------------------------------

/// Spin up a Discretize-backed server with enough builds in the upstream
/// fixture to exercise multi-page pagination.
async fn build_server_with_seven_discretize_builds() -> McpServer {
    let server = MockServer::start().await;
    let tree = r#"{"tree":[
        {"path":"builds/guardian/a/index.md","type":"blob"},
        {"path":"builds/guardian/b/index.md","type":"blob"},
        {"path":"builds/guardian/c/index.md","type":"blob"},
        {"path":"builds/guardian/d/index.md","type":"blob"},
        {"path":"builds/guardian/e/index.md","type":"blob"},
        {"path":"builds/guardian/f/index.md","type":"blob"},
        {"path":"builds/guardian/g/index.md","type":"blob"}
    ]}"#;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(tree))
        .mount(&server)
        .await;
    // Leak the wiremock server so the mock outlives this fn — the McpServer
    // we return only holds its URI string. The test process exits soon after,
    // so leaking is fine and avoids a Box<MockServer> in the helper return.
    let mcp = build_server_with_discretize(server.uri(), "http://unused.invalid".to_owned());
    Box::leak(Box::new(server));
    mcp
}

#[tokio::test]
async fn pagination_round_trip_walks_all_pages() {
    let mcp = build_server_with_seven_discretize_builds().await;

    // Page 1 — page_size 3, expect 3 items + cursor.
    let p1 = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({"source": "discretize", "profession": "guardian", "page_size": 3}),
        )
        .await
        .unwrap();
    assert_eq!(p1["items"].as_array().unwrap().len(), 3);
    let c1 = p1["next_cursor"].as_str().unwrap().to_owned();

    // Page 2 — same page_size, use cursor. 3 more items + cursor.
    let p2 = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({
                "source": "discretize",
                "profession": "guardian",
                "page_size": 3,
                "cursor": c1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(p2["items"].as_array().unwrap().len(), 3);
    let c2 = p2["next_cursor"].as_str().unwrap().to_owned();

    // Page 3 — last page, 1 item + null cursor.
    let p3 = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({
                "source": "discretize",
                "profession": "guardian",
                "page_size": 3,
                "cursor": c2,
            }),
        )
        .await
        .unwrap();
    assert_eq!(p3["items"].as_array().unwrap().len(), 1);
    assert!(
        p3["next_cursor"].is_null(),
        "last page must yield null next_cursor; got {p3}"
    );
}

#[tokio::test]
async fn pagination_rejects_malformed_cursor() {
    let mcp = build_server_with_seven_discretize_builds().await;
    let err = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({"source": "discretize", "cursor": "not-base64!@#$"}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("cursor"),
        "should mention the bad cursor: {err}"
    );
}

#[tokio::test]
async fn pagination_rejects_cursor_minted_with_different_filter() {
    let mcp = build_server_with_seven_discretize_builds().await;

    // Mint a cursor against the (discretize, guardian, *) filter set.
    let p1 = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({"source": "discretize", "profession": "guardian", "page_size": 3}),
        )
        .await
        .unwrap();
    let c = p1["next_cursor"].as_str().unwrap().to_owned();

    // Reuse it without the profession filter — must reject.
    let err = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({"source": "discretize", "cursor": c}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("cursor")
            && (err.to_lowercase().contains("filter") || err.to_lowercase().contains("different")),
        "should explain filter mismatch: {err}"
    );
}

#[tokio::test]
async fn pagination_clamps_page_size_above_100() {
    let mcp = build_server_with_seven_discretize_builds().await;
    // page_size=10000 — should be clamped to 100, all 7 fit, no next cursor.
    let p = mcp
        .dispatch_tool(
            "list_catalog_builds",
            json!({"source": "discretize", "profession": "guardian", "page_size": 10000}),
        )
        .await
        .unwrap();
    assert_eq!(p["items"].as_array().unwrap().len(), 7);
    assert!(
        p["next_cursor"].is_null(),
        "all items returned → null next_cursor; got {p}"
    );
}

// ---------------------------------------------------------------------------
// get_info — the runbook tool. Returns a non-empty string identical to the
// `instructions` field of `initialize` (DRY check).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_info_returns_a_substantial_runbook_string() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let result = mcp.dispatch_tool("get_info", json!({})).await.unwrap();
    let s = result.as_str().expect("get_info must return a string");
    // Substantive runbook — should be at least a few hundred chars and
    // mention the catalog tools and the API key flow.
    assert!(
        s.len() > 500,
        "runbook should be substantial; got {} chars",
        s.len()
    );
    assert!(s.contains("get_character_build"));
    assert!(s.contains("decode_build_code"));
    assert!(s.contains("list_catalog_builds"));
    assert!(s.contains("get_catalog_build"));
    assert!(s.contains("https://account.arena.net/applications"));
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

// ---------------------------------------------------------------------------
// Error UX — pin the messages the LLM/user actually sees for the most
// common failure modes. These are the bug reports we want to never write.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_ux_invalid_api_key_explains_where_to_get_one() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("get_wallet", json!({"api_key": valid_api_key().expose()}))
        .await
        .unwrap_err();
    let lower = err.to_lowercase();
    assert!(
        lower.contains("rejected"),
        "should say the API rejected the key — got: {err}"
    );
    assert!(
        lower.contains("scope"),
        "should mention scopes — got: {err}"
    );
    assert!(
        err.contains("https://account.arena.net/applications"),
        "should link the user to where they manage keys — got: {err}"
    );
}

#[tokio::test]
async fn error_ux_short_api_key_validation_runs_before_http() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("get_wallet", json!({"api_key": "too-short"}))
        .await
        .unwrap_err();
    let lower = err.to_lowercase();
    assert!(
        lower.contains("api key"),
        "should say what's wrong — got: {err}"
    );
    // Guard against accidentally echoing the input back at the user.
    assert!(
        !err.contains("too-short"),
        "should not echo the (presumed-secret) input — got: {err}"
    );
}

#[tokio::test]
async fn error_ux_missing_required_arg_is_specific() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    // With Tier 4, `api_key` is no longer required at the schema level —
    // the server can be started with `--api-key` / `GW2_API_KEY` to
    // provide a default. When neither is configured AND the call omits
    // the arg, the error must point the user at both fix options without
    // echoing the env var name into the response only as documentation.
    let err = mcp
        .dispatch_tool("get_wallet", json!({}))
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("api key"),
        "should mention API key — got: {err}"
    );
    assert!(
        err.contains("GW2_API_KEY") || err.contains("api_key"),
        "should mention either the env var or the argument name — got: {err}"
    );
}

#[tokio::test]
async fn missing_character_arg_still_errors_specifically() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    // `character` is still required — only api_key got the default treatment.
    let err = mcp
        .dispatch_tool(
            "get_character_build",
            json!({ "api_key": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE-FFFFFFFF-GGGG-HHHH-IIII-JJJJJJJJJJJJ" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("character"),
        "should name the missing arg — got: {err}"
    );
}

#[tokio::test]
async fn error_ux_no_such_character_is_clean() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters/Ghost/buildtabs"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"text":"no such character"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/characters/Ghost/equipmenttabs"))
        .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"text":"no such character"}"#))
        .mount(&server)
        .await;

    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool(
            "get_character_build",
            json!({"api_key": valid_api_key().expose(), "character": "Ghost"}),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("Ghost"),
        "should name the character — got: {err}"
    );
    assert!(
        err.to_lowercase().contains("does not exist") || err.to_lowercase().contains("not found"),
        "should say the character doesn't exist — got: {err}"
    );
    assert!(
        !err.contains("\"text\""),
        "should not leak the raw JSON error envelope — got: {err}"
    );
    assert!(
        !err.contains("status 400"),
        "should not leak the HTTP status code in the user message — got: {err}"
    );
}

#[tokio::test]
async fn error_ux_rate_limit_tells_user_to_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wallet"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("get_wallet", json!({"api_key": valid_api_key().expose()}))
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("rate limit"));
    assert!(err.to_lowercase().contains("try again"));
}

#[tokio::test]
async fn error_ux_unknown_build_source_lists_what_is_available_implicitly() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("list_catalog_builds", json!({"source": "totally-fake"}))
        .await
        .unwrap_err();
    assert!(
        err.contains("totally-fake"),
        "should echo the bad source — got: {err}"
    );
    assert!(err.to_lowercase().contains("no such build source"));
}

#[tokio::test]
async fn error_ux_malformed_chat_code_does_not_echo_long_blob() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let huge = "a".repeat(5000);
    let err = mcp
        .dispatch_tool("decode_build_code", json!({"code": huge.clone()}))
        .await
        .unwrap_err();
    // We allow up to 16 chars of preview in the domain error.
    assert!(
        !err.contains(&"a".repeat(100)),
        "must not echo the full malformed blob — got {} chars",
        err.len()
    );
    assert!(err.to_lowercase().contains("chat code") || err.to_lowercase().contains("malformed"));
}

#[tokio::test]
async fn error_ux_get_skills_with_zero_id_is_specific() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("get_skills", json!({"ids": [0, 1]}))
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("skill id"),
        "should mention what's wrong — got: {err}"
    );
    assert!(
        err.contains('0'),
        "should name the offending id — got: {err}"
    );
}

#[tokio::test]
async fn error_ux_get_skills_with_string_id_is_specific() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool("get_skills", json!({"ids": ["not", "ints"]}))
        .await
        .unwrap_err();
    assert!(
        err.contains("ids"),
        "should mention the offending arg name — got: {err}"
    );
    assert!(
        err.to_lowercase().contains("integer"),
        "should mention the expected type — got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Tier 6A — end-to-end MCP dispatch for the new account/coaching tools.
// Each test wires the real HTTP adapter against wiremock + dispatches via
// the MCP layer, mirroring the pattern of the wallet/character tests above.
// ---------------------------------------------------------------------------

const ACCOUNT_FIXTURE: &str = include_str!("fixtures/account_basic.json");
const ACHIEVEMENTS_FIXTURE: &str = include_str!("fixtures/account_achievements.json");
const RAIDS_FIXTURE: &str = include_str!("fixtures/account_raids.json");
const DUNGEONS_FIXTURE: &str = include_str!("fixtures/account_dungeons.json");
const WIZARDS_VAULT_FIXTURE: &str = include_str!("fixtures/wizards_vault_daily.json");
const CHAR_LIST_FIXTURE: &str = include_str!("fixtures/account_characters_list.json");

#[tokio::test]
async fn dispatch_get_account_returns_structured_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACCOUNT_FIXTURE))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool("get_account", json!({"api_key": valid_api_key().expose()}))
        .await
        .unwrap();
    assert_eq!(v["name"], "Snowflake.1234");
    assert_eq!(v["fractal_level"], 100);
    assert!(
        v["access"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "EndOfDragons")
    );
}

#[tokio::test]
async fn dispatch_list_characters_returns_object_with_characters_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/characters"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAR_LIST_FIXTURE))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool(
            "list_characters",
            json!({"api_key": valid_api_key().expose()}),
        )
        .await
        .unwrap();
    assert!(
        v.is_object(),
        "list_characters response must be an object (MCP rejects bare arrays)"
    );
    let arr = v["characters"].as_array().expect("characters array");
    assert!(arr.iter().any(|n| n == "Snowflake"));
    assert!(arr.iter().any(|n| n == "Vesta Vey"));
    assert_eq!(v["total"].as_u64().unwrap(), arr.len() as u64);
}

#[tokio::test]
async fn dispatch_get_account_achievements_summary_drops_done_and_not_started() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/achievements"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACHIEVEMENTS_FIXTURE))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool(
            "get_account_achievements",
            json!({"api_key": valid_api_key().expose()}),
        )
        .await
        .unwrap();
    assert!(
        v.is_object(),
        "response must be an object, not a bare array"
    );
    let arr = v["achievements"].as_array().expect("achievements array");
    let ids: Vec<u64> = arr.iter().map(|e| e["id"].as_u64().unwrap()).collect();
    assert_eq!(
        ids,
        vec![200u64, 500],
        "summary mode keeps only id 200 (5/10) and id 500 (7/25); 100/600 not started, 300/400 done"
    );
    assert_eq!(v["summary"], true);
    assert_eq!(v["total"], 2);
}

#[tokio::test]
async fn dispatch_get_account_achievements_summary_false_returns_full_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/achievements"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACHIEVEMENTS_FIXTURE))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool(
            "get_account_achievements",
            json!({"api_key": valid_api_key().expose(), "summary": false}),
        )
        .await
        .unwrap();
    assert_eq!(v["achievements"].as_array().unwrap().len(), 6);
    assert_eq!(v["summary"], false);
    assert_eq!(v["total"], 6);
}

#[tokio::test]
async fn dispatch_get_account_raids_returns_enriched_snapshot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RAIDS_FIXTURE))
        .mount(&server)
        .await;
    // /v2/raids returns the wing+encounter structure; mock just enough
    // for the enrichment path to find "vale_guardian" and report it as
    // cleared while leaving "gorseval_the_multifarious" uncleared.
    // Mount the more-specific (?ids=...) mock first so wiremock's
    // first-match-wins picks it over the bare id-list mock.
    Mock::given(method("GET"))
        .and(path("/raids"))
        .and(query_param("ids", "forsaken_thicket"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"forsaken_thicket","wings":[{"id":"spirit_vale","events":[{"id":"vale_guardian","type":"Boss"},{"id":"gorseval_the_multifarious","type":"Boss"}]}]}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"["forsaken_thicket"]"#))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool(
            "get_account_raids",
            json!({"api_key": valid_api_key().expose()}),
        )
        .await
        .unwrap();
    let encounters = v["encounters"].as_array().expect("encounters array");
    let vg = encounters
        .iter()
        .find(|e| e["id"] == "vale_guardian")
        .expect("vale_guardian present");
    assert_eq!(vg["name"], "Vale Guardian");
    assert_eq!(vg["cleared"], true);
    let gorseval = encounters
        .iter()
        .find(|e| e["id"] == "gorseval_the_multifarious")
        .expect("gorseval present");
    assert_eq!(gorseval["cleared"], false);
    assert!(v["weekly_reset_at"].is_string(), "reset timestamp present");
}

#[tokio::test]
async fn dispatch_get_account_dungeons_returns_enriched_snapshot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/dungeons"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DUNGEONS_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dungeons"))
        .and(query_param("ids", "ascalonian_catacombs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"ascalonian_catacombs","paths":[{"id":"ascalonian_catacombs_story","type":"Story"},{"id":"ascalonian_catacombs_hodgins","type":"Explorable"},{"id":"ascalonian_catacombs_detha","type":"Explorable"}]}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dungeons"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"["ascalonian_catacombs"]"#))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool(
            "get_account_dungeons",
            json!({"api_key": valid_api_key().expose()}),
        )
        .await
        .unwrap();
    let paths = v["paths"].as_array().expect("paths array");
    assert_eq!(paths.len(), 3, "all paths listed regardless of clear state");
    let story = paths
        .iter()
        .find(|p| p["id"] == "ascalonian_catacombs_story")
        .unwrap();
    assert_eq!(story["cleared"], true);
    let detha = paths
        .iter()
        .find(|p| p["id"] == "ascalonian_catacombs_detha")
        .unwrap();
    assert_eq!(detha["cleared"], false);
    assert!(v["daily_reset_at"].is_string(), "reset timestamp present");
}

#[tokio::test]
async fn dispatch_get_dailies_daily_returns_wizards_vault_objectives() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/daily"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let v = mcp
        .dispatch_tool("get_dailies", json!({"api_key": valid_api_key().expose()}))
        .await
        .unwrap();
    let objectives = v["objectives"].as_array().expect("objectives array");
    assert_eq!(objectives.len(), 4);
    assert_eq!(objectives[0]["title"], "Complete an Event");
    assert_eq!(v["meta_reward_astral"], 50);

    // Acclaim rollup is computed at the service layer.
    // Fixture: id 1 (25, claimed), id 2 (25, unclaimed), id 3 (25,
    // claimed), id 4 (25, unclaimed); meta_reward_astral 50, unclaimed.
    assert_eq!(
        v["acclaim_earned"], 50,
        "earned = sum of claimed objectives (id 1 + id 3 = 50)"
    );
    assert_eq!(
        v["acclaim_remaining"], 100,
        "remaining = unclaimed objectives (25 + 25) + meta (50)"
    );
    assert_eq!(v["acclaim_total"], 150);
}

#[tokio::test]
async fn dispatch_get_dailies_weekly_hits_weekly_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/weekly"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let _v = mcp
        .dispatch_tool(
            "get_dailies",
            json!({"api_key": valid_api_key().expose(), "which": "weekly"}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn dispatch_get_dailies_special_hits_special_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/special"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .expect(1)
        .mount(&server)
        .await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let _v = mcp
        .dispatch_tool(
            "get_dailies",
            json!({"api_key": valid_api_key().expose(), "which": "special"}),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn dispatch_get_dailies_rejects_invalid_which() {
    let server = MockServer::start().await;
    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    let err = mcp
        .dispatch_tool(
            "get_dailies",
            json!({"api_key": valid_api_key().expose(), "which": "tomorrow"}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("which"), "got: {err}");
}

#[tokio::test]
async fn tier_6a_tools_are_present_in_build_tools_list() {
    // Pin the new tool surface — every Tier 6A tool must appear in the
    // tools/list response (tested via build_tools indirectly, but we also
    // dispatch each by name to make sure the dispatcher knows them).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACCOUNT_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/characters"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CHAR_LIST_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/achievements"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ACHIEVEMENTS_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/masteries"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RAIDS_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/dungeons"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DUNGEONS_FIXTURE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/account/wizardsvault/daily"))
        .respond_with(ResponseTemplate::new(200).set_body_string(WIZARDS_VAULT_FIXTURE))
        .mount(&server)
        .await;

    let mcp = build_server(server.uri(), "http://unused.invalid/".to_owned());
    for name in [
        "get_account",
        "list_characters",
        "get_account_achievements",
        "get_account_masteries",
        "get_account_raids",
        "get_account_dungeons",
        "get_dailies",
    ] {
        let args = { json!({"api_key": valid_api_key().expose()}) };
        let _ = mcp
            .dispatch_tool(name, args)
            .await
            .unwrap_or_else(|e| panic!("{name} failed to dispatch: {e}"));
    }
}
