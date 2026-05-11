//! Integration tests for the Snow Crows catalog adapter.
//!
//! Two fixtures:
//! - `snowcrows_listing.html` — minimal hand-rolled raid index page used
//!   to exercise the listing path, filters, and TTL cache.
//! - `snowcrows_build.html` — a real `snowcrows.com/builds/raids/...`
//!   page; we only assert on stable structural facts.
//!
//! Wiremock is used because the catalog hits HTTP for both list and fetch.

use std::time::Duration;

use gw2_mcp::adapters::SnowCrowsCatalog;
use gw2_mcp::domain::BuildSlug;
use gw2_mcp::ports::{BuildCatalog, CatalogError, CatalogFilter};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LISTING_HTML: &str = include_str!("fixtures/snowcrows_listing.html");
const BUILD_HTML: &str = include_str!("fixtures/snowcrows_build.html");

/// Mount `/builds/raids` with the listing fixture. `List()` with no filter
/// only fetches the canonical `raids` category — other categories are
/// only hit when an explicit `gamemode` filter is passed, so most tests
/// only need this one mock.
async fn mount_raids(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .mount(server)
        .await;
}

#[tokio::test]
async fn list_returns_builds_from_listing_page() {
    let server = MockServer::start().await;
    mount_raids(&server).await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let out = catalog.list(&CatalogFilter::default()).await.unwrap();

    assert!(
        !out.is_empty(),
        "list() should now populate from the index page"
    );
    let slugs: Vec<&str> = out.iter().map(|b| b.slug.as_str()).collect();
    assert!(slugs.contains(&"raids/elementalist/power-tempest-spear"));
    assert!(slugs.iter().all(|s| s.starts_with("raids/")));
    // source_url must be absolute and point back to Snow Crows for attribution.
    assert!(
        out.iter()
            .all(|b| b.source_url.contains("/builds/raids/") && b.source_url.starts_with("http")),
        "every entry needs a working source_url for attribution"
    );
}

#[tokio::test]
async fn list_filters_by_profession() {
    let server = MockServer::start().await;
    mount_raids(&server).await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let filter = CatalogFilter {
        profession: Some("Guardian".to_owned()),
        ..Default::default()
    };
    let out = catalog.list(&filter).await.unwrap();

    assert!(!out.is_empty());
    assert!(
        out.iter().all(|b| b.profession == "Guardian"),
        "every result must be Guardian; got {:?}",
        out.iter().map(|b| &b.profession).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn list_filters_by_gamemode_skips_unknown_category_calls() {
    let server = MockServer::start().await;
    // Only mount /builds/raids. If list() incorrectly visits other
    // categories when gamemode=raids, wiremock will 404 and we'd see a
    // transport error — so this test doubles as a regression on the
    // filter logic.
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let filter = CatalogFilter {
        gamemode: Some("raids".to_owned()),
        ..Default::default()
    };
    let out = catalog.list(&filter).await.unwrap();
    assert!(!out.is_empty());
    assert!(out.iter().all(|b| b.gamemode == "raids"));
}

#[tokio::test]
async fn list_returns_empty_for_unknown_gamemode() {
    let server = MockServer::start().await;
    mount_raids(&server).await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let filter = CatalogFilter {
        gamemode: Some("not-a-real-mode".to_owned()),
        ..Default::default()
    };
    let out = catalog.list(&filter).await.unwrap();
    assert!(
        out.is_empty(),
        "unknown gamemode should return empty without HTTP"
    );
}

#[tokio::test]
async fn list_uses_cache_within_ttl() {
    let server = MockServer::start().await;
    // expect(1) asserts wiremock receives exactly 1 GET. The second
    // list() call must hit the in-memory cache.
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .expect(1)
        .mount(&server)
        .await;

    let catalog =
        SnowCrowsCatalog::with_options(server.uri(), Duration::from_secs(60 * 60)).unwrap();
    let first = catalog.list(&CatalogFilter::default()).await.unwrap();
    let second = catalog.list(&CatalogFilter::default()).await.unwrap();
    assert_eq!(first.len(), second.len(), "cached result must be stable");
}

#[tokio::test]
async fn list_refetches_when_ttl_zero() {
    let server = MockServer::start().await;
    // cache_ttl = 0 → every call refetches. Two calls → exactly 2 GETs.
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .expect(2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_options(server.uri(), Duration::from_secs(0)).unwrap();
    let _ = catalog.list(&CatalogFilter::default()).await.unwrap();
    let _ = catalog.list(&CatalogFilter::default()).await.unwrap();
}

#[tokio::test]
async fn list_caches_404_as_empty() {
    let server = MockServer::start().await;
    // 404 is durable (category renamed/removed upstream) — must cache.
    // expect(1) verifies the second list() call hits cache, not network.
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let out = catalog.list(&CatalogFilter::default()).await.unwrap();
    assert!(out.is_empty(), "404 should map to empty listing, not error");
    let _ = catalog.list(&CatalogFilter::default()).await.unwrap();
}

#[tokio::test]
async fn list_does_not_cache_403_so_next_call_retries() {
    let server = MockServer::start().await;
    // 403 is transient (Cloudflare burst limit). We must NOT cache —
    // otherwise a single rate-limit blip would lock the user out for the
    // entire TTL. Two calls → two GETs (both 403, both yield empty).
    Mock::given(method("GET"))
        .and(path("/builds/raids"))
        .respond_with(ResponseTemplate::new(403))
        .expect(2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let first = catalog.list(&CatalogFilter::default()).await.unwrap();
    assert!(first.is_empty(), "403 should yield empty, not error");
    let second = catalog.list(&CatalogFilter::default()).await.unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn list_with_non_default_gamemode_fetches_that_category() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/builds/wvw"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html><body>
                <a href="/builds/wvw/elementalist/power-catalyst"><h2>Power Catalyst</h2></a>
            </body></html>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let filter = CatalogFilter {
        gamemode: Some("wvw".to_owned()),
        ..Default::default()
    };
    let out = catalog.list(&filter).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].slug, "wvw/elementalist/power-catalyst");
    assert_eq!(out[0].gamemode, "wvw");
}

#[tokio::test]
async fn list_accepts_open_world_with_underscore_or_hyphen() {
    let server = MockServer::start().await;
    // The MCP schema's gamemode enum uses `open_world`; the URL path uses
    // `open-world`. Both spellings must resolve to the same category.
    Mock::given(method("GET"))
        .and(path("/builds/open-world"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html><body>
                <a href="/builds/open-world/ranger/condi-soulbeast"><h2>Condi Soulbeast</h2></a>
            </body></html>"#,
        ))
        // Two list() calls, one per spelling, both must reach this mock.
        .expect(2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_options(server.uri(), Duration::from_secs(0)).unwrap();
    for gm in ["open_world", "open-world"] {
        let filter = CatalogFilter {
            gamemode: Some(gm.to_owned()),
            ..Default::default()
        };
        let out = catalog.list(&filter).await.unwrap();
        assert_eq!(out.len(), 1, "spelling '{gm}' should match the URL path");
    }
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
    let slug =
        BuildSlug::new("raids/elementalist/celestial-alacrity-tempest-scepter-warhorn").unwrap();
    let detail = catalog.fetch(&slug).await.unwrap();

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
    assert!(
        detail.description.len() > 500,
        "scraped description should be substantial; got {} bytes",
        detail.description.len()
    );
    assert!(
        detail.description.to_lowercase().contains("tempest"),
        "expected 'tempest' to appear in the scraped description"
    );
}

#[tokio::test]
async fn fetch_rejects_malformed_slug() {
    let server = MockServer::start().await;
    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let slug = BuildSlug::new("only/two-parts").unwrap();
    let err = catalog.fetch(&slug).await.unwrap_err();
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
    let slug = BuildSlug::new("raids/elementalist/missing").unwrap();
    let err = catalog.fetch(&slug).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound { .. }));
}
