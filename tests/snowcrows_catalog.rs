//! Integration tests for the Snow Crows catalog adapter.
//!
//! Fixture is a real `snowcrows.com/builds/raids/...` page; we only assert
//! on stable, structural facts (title element, presence of meaningful
//! body text, attribution URL) — not the brittle full text content.

use gw2_mcp::adapters::SnowCrowsCatalog;
use gw2_mcp::ports::{BuildCatalog, CatalogError, CatalogFilter};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BUILD_HTML: &str = include_str!("fixtures/snowcrows_build.html");

#[tokio::test]
async fn list_returns_empty_by_design() {
    let server = MockServer::start().await;
    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let out = catalog.list(&CatalogFilter::default()).await.unwrap();
    assert!(
        out.is_empty(),
        "snowcrows.list must return empty (we don't bulk-scrape)"
    );
}

#[tokio::test]
async fn fetch_parses_real_build_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/builds/raids/elementalist/celestial-alacrity-tempest-scepter-warhorn",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(BUILD_HTML))
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let detail = catalog
        .fetch("raids/elementalist/celestial-alacrity-tempest-scepter-warhorn")
        .await
        .unwrap();

    assert_eq!(detail.summary.profession, "Elementalist");
    assert_eq!(detail.summary.gamemode, "raids");
    assert_eq!(detail.summary.source, "snowcrows");
    assert!(
        detail
            .summary
            .source_url
            .contains("/builds/raids/elementalist/"),
        "source_url must point back to snow crows for attribution"
    );
    // The page contains real prose; the scraped body should not be empty.
    assert!(
        detail.description.len() > 500,
        "scraped description should be substantial; got {} bytes",
        detail.description.len()
    );
    // The build's elite spec name should appear in the text.
    assert!(
        detail.description.to_lowercase().contains("tempest"),
        "expected 'tempest' to appear in the scraped description"
    );
}

#[tokio::test]
async fn fetch_rejects_malformed_slug() {
    let server = MockServer::start().await;
    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let err = catalog.fetch("only/two-parts").await.unwrap_err();
    assert!(matches!(err, CatalogError::Parse { .. }));
}

#[tokio::test]
async fn fetch_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/builds/raids/elementalist/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let err = catalog
        .fetch("raids/elementalist/missing")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound { .. }));
}
