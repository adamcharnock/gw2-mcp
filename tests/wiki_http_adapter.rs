//! Integration tests for [`HttpWiki`] using `wiremock`.

mod common;

use gw2_mcp::adapters::HttpWiki;
use gw2_mcp::domain::{SearchLimit, SearchQuery};
use gw2_mcp::ports::Wiki;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn search_sends_correct_params_and_parses_hits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/")) // we set base_url to "{server}/", path matches root
        .and(query_param("action", "query"))
        .and(query_param("list", "search"))
        .and(query_param("srsearch", "Dragon Bash"))
        .and(query_param("srlimit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "batchcomplete": "",
            "query": {
                "search": [
                    {
                        "ns": 0,
                        "title": "Dragon Bash",
                        "pageid": 12345,
                        "size": 5000,
                        "wordcount": 800,
                        "snippet": "<span class=\"searchmatch\">Dragon</span> Bash is a festival",
                        "timestamp": "2026-01-01T12:00:00Z"
                    }
                ],
                "searchinfo": { "totalhits": 1 }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpWiki::with_base_url(format!("{}/", server.uri())).unwrap();
    let q = SearchQuery::new("Dragon Bash").unwrap();
    let results = api.search(&q, SearchLimit::new(5).unwrap()).await.unwrap();

    assert_eq!(results.len(), 1);
    let hit = &results[0];
    assert_eq!(hit.title, "Dragon Bash");
    assert_eq!(hit.page_id, 12345);
    // snippet must have HTML stripped
    assert_eq!(hit.snippet, "Dragon Bash is a festival");
    // url is filled in by the service layer, not the adapter
    assert!(hit.url.is_empty());
    assert!(hit.extract.is_empty());
}

#[tokio::test]
async fn fetch_extract_returns_first_page_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("action", "query"))
        .and(query_param("prop", "extracts"))
        .and(query_param("titles", "Dragon Bash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "batchcomplete": "",
            "query": {
                "pages": {
                    "12345": {
                        "pageid": 12345,
                        "ns": 0,
                        "title": "Dragon Bash",
                        "extract": "Dragon Bash is an annual festival in Guild Wars 2."
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let api = HttpWiki::with_base_url(format!("{}/", server.uri())).unwrap();
    let extract = api.fetch_extract("Dragon Bash").await.unwrap();
    assert_eq!(
        extract,
        "Dragon Bash is an annual festival in Guild Wars 2."
    );
}

#[tokio::test]
async fn fetch_extract_handles_missing_extract_field() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "query": { "pages": { "1": { "pageid": 1, "title": "X" } } }
        })))
        .mount(&server)
        .await;

    let api = HttpWiki::with_base_url(format!("{}/", server.uri())).unwrap();
    let extract = api.fetch_extract("X").await.unwrap();
    assert_eq!(extract, "", "missing extract should return empty string");
}

#[tokio::test]
async fn search_5xx_propagates_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let api = HttpWiki::with_base_url(format!("{}/", server.uri())).unwrap();
    let q = SearchQuery::new("foo").unwrap();
    let err = api.search(&q, SearchLimit::default()).await.unwrap_err();
    assert!(
        format!("{err}").contains("500"),
        "expected 500 in error, got: {err}"
    );
}
