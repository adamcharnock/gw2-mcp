//! Integration tests for the `MetaBattle` catalog adapter, using real
//! captured `metabattle.com/wiki/api.php` responses as fixtures.

use gw2_mcp::adapters::MetaBattleCatalog;
use gw2_mcp::domain::BuildSlug;
use gw2_mcp::ports::{BuildCatalog, CatalogError, CatalogFilter};
use pretty_assertions::assert_eq;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIST_FIXTURE: &str = include_str!("fixtures/metabattle_list.json");
const PARSE_FIXTURE: &str = include_str!("fixtures/metabattle_parse.json");

#[tokio::test]
async fn list_extracts_summaries_from_categorymembers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.php"))
        .and(query_param("list", "categorymembers"))
        .and(query_param("cmtitle", "Category:Meta_builds"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LIST_FIXTURE))
        .mount(&server)
        .await;

    let catalog = MetaBattleCatalog::with_base_url(format!("{}/api.php", server.uri())).unwrap();
    let summaries = catalog.list(&CatalogFilter::default()).await.unwrap();

    // Real fixture has 10 members, all of the form "Build:<Profession> - <Name>".
    assert_eq!(summaries.len(), 10);
    for s in &summaries {
        // Slugs are now sanitised to lowercase `<profession>/<build>` form
        // so they round-trip through `BuildSlug` validation.
        assert!(
            s.slug.contains('/'),
            "slug must be `<profession>/<build>`: got {:?}",
            s.slug
        );
        assert!(
            BuildSlug::new(&s.slug).is_ok(),
            "slug must satisfy BuildSlug validation: {:?}",
            s.slug
        );
        assert!(!s.profession.is_empty());
        assert_eq!(s.rating.as_deref(), Some("Meta"));
        assert_eq!(s.source, "metabattle");
        assert!(s.source_url.starts_with("https://metabattle.com/wiki/"));
    }
    // Spot-check known entry from the captured fixture.
    assert!(
        summaries
            .iter()
            .any(|s| s.title == "Power Berserker" && s.profession == "Berserker"),
        "fixture should contain Berserker - Power Berserker"
    );
}

#[tokio::test]
async fn list_filters_by_profession() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LIST_FIXTURE))
        .mount(&server)
        .await;

    let catalog = MetaBattleCatalog::with_base_url(format!("{}/api.php", server.uri())).unwrap();
    let only_berserker = catalog
        .list(&CatalogFilter {
            profession: Some("berserker".to_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!only_berserker.is_empty());
    for s in &only_berserker {
        assert_eq!(s.profession, "Berserker");
    }
}

#[tokio::test]
async fn fetch_returns_wikitext_under_description() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.php"))
        .and(query_param("action", "parse"))
        .respond_with(ResponseTemplate::new(200).set_body_string(PARSE_FIXTURE))
        .mount(&server)
        .await;

    let catalog = MetaBattleCatalog::with_base_url(format!("{}/api.php", server.uri())).unwrap();
    let slug = BuildSlug::new("berserker/power_berserker").unwrap();
    let detail = catalog.fetch(&slug).await.unwrap();

    // Title is whatever MediaWiki returned in the parse fixture.
    assert!(detail.summary.title.starts_with("Build:"));
    // Profession comes out of the {{Build}} infobox in the wikitext.
    assert!(
        detail.summary.profession.eq_ignore_ascii_case("warrior"),
        "expected 'warrior', got {:?}",
        detail.summary.profession
    );
    assert!(
        detail.description.contains("{{Build"),
        "wikitext must include the Build infobox"
    );
}

#[tokio::test]
async fn fetch_returns_not_found_when_mediawiki_returns_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.php"))
        .and(query_param("action", "parse"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"error": {"code": "missingtitle", "info": "page not found"}}"#,
            ),
        )
        .mount(&server)
        .await;

    let catalog = MetaBattleCatalog::with_base_url(format!("{}/api.php", server.uri())).unwrap();
    let slug = BuildSlug::new("warrior/nope").unwrap();
    let err = catalog.fetch(&slug).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound { .. }));
}
