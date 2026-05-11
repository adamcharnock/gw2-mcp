//! Integration tests for the Snow Crows catalog adapter.
//!
//! Snow Crows publishes per-profession index pages
//! (`/builds/<category>/<profession>`), so the adapter fans out one HTTP
//! request per profession when no profession filter is supplied. Tests
//! use `path_regex` to match the family of profession paths under a
//! category with a single mock that records the total request count.
//!
//! Fixtures:
//! - `snowcrows_listing.html` — minimal hand-rolled raid index page with
//!   anchors for elementalist, guardian, necromancer, plus a wrong-
//!   category strike anchor and several profession-filter (2-segment)
//!   anchors that must be skipped. Served for every per-profession path
//!   under `/builds/raids/`; the parser filters to the requested
//!   profession, so each profession-call returns a different subset.
//! - `snowcrows_build.html` — real `/builds/raids/...` page; we only
//!   assert structural facts on it.

use std::time::Duration;

use gw2_mcp::adapters::SnowCrowsCatalog;
use gw2_mcp::domain::BuildSlug;
use gw2_mcp::ports::{BuildCatalog, CatalogError, CatalogFilter};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LISTING_HTML: &str = include_str!("fixtures/snowcrows_listing.html");
const BUILD_HTML: &str = include_str!("fixtures/snowcrows_build.html");

/// Number of GW2 professions Snow Crows pages exist for. The adapter
/// fans out one request per profession when no profession filter is
/// supplied; tests using a default filter expect this many GETs.
const NUM_PROFESSIONS: u64 = 9;

/// Mount one wildcard mock that responds with the raid-listing fixture
/// for any `/builds/raids/<profession>` path. Useful for the default-
/// filter tests that fan out across all 9 professions.
async fn mount_raids_all_professions(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/builds/raids/[a-z]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .mount(server)
        .await;
}

#[tokio::test]
async fn list_returns_builds_from_per_profession_pages() {
    let server = MockServer::start().await;
    mount_raids_all_professions(&server).await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let out = catalog.list(&CatalogFilter::default()).await.unwrap();

    assert!(
        !out.is_empty(),
        "list() should populate from the per-profession pages"
    );
    let slugs: Vec<&str> = out.iter().map(|b| b.slug.as_str()).collect();
    // Fixture has builds for ele, guardian, necromancer — fanning out
    // across 9 profs should surface at least these three.
    assert!(slugs.contains(&"raids/elementalist/power-tempest-spear"));
    assert!(slugs.contains(&"raids/guardian/power-dragonhunter-longbow"));
    assert!(slugs.contains(&"raids/necromancer/condition-scourge-pistol-torch"));
    assert!(
        slugs.iter().all(|s| s.starts_with("raids/")),
        "every entry must be from the raids category"
    );
    assert!(
        out.iter()
            .all(|b| b.source_url.contains("/builds/raids/") && b.source_url.starts_with("http")),
        "every entry needs an absolute source_url for attribution"
    );
}

#[tokio::test]
async fn list_with_profession_filter_fetches_only_that_profession() {
    let server = MockServer::start().await;
    // expect(1) confirms a profession filter narrows the fan-out from 9
    // requests down to 1.
    Mock::given(method("GET"))
        .and(path("/builds/raids/guardian"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .expect(1)
        .mount(&server)
        .await;

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
async fn list_filters_by_gamemode_stays_in_that_category() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/builds/raids/[a-z]+$"))
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
    // No mocks: if the adapter incorrectly tries to fetch anything,
    // wiremock returns 404 and the test would fail (since 404 currently
    // doesn't error — but the empty-result assertion catches the broader
    // regression).
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
async fn concurrent_calls_for_same_key_dedupe_to_one_fetch() {
    let server = MockServer::start().await;
    // The mock has a 100ms delay so the two list() calls actually
    // overlap rather than one completing before the other starts.
    // expect(1) asserts: only one of the two concurrent callers fetches
    // upstream; the other waits on the cache lock and reads the result.
    Mock::given(method("GET"))
        .and(path("/builds/raids/guardian"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(LISTING_HTML)
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let catalog = std::sync::Arc::new(SnowCrowsCatalog::with_base_url(server.uri()).unwrap());
    let filter = CatalogFilter {
        profession: Some("Guardian".to_owned()),
        ..Default::default()
    };
    let (a, b) = tokio::join!(catalog.list(&filter), catalog.list(&filter));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(a.unwrap().len(), b.unwrap().len());
}

#[tokio::test]
async fn list_uses_cache_within_ttl() {
    let server = MockServer::start().await;
    // First call fans out across all 9 professions → 9 GETs. Second
    // call hits the in-memory cache for all 9 → 0 additional GETs.
    Mock::given(method("GET"))
        .and(path_regex(r"^/builds/raids/[a-z]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .expect(NUM_PROFESSIONS)
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
    // cache_ttl = 0 → both list() calls re-fan out. 2 calls × 9 profs.
    Mock::given(method("GET"))
        .and(path_regex(r"^/builds/raids/[a-z]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LISTING_HTML))
        .expect(NUM_PROFESSIONS * 2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_options(server.uri(), Duration::from_secs(0)).unwrap();
    let _ = catalog.list(&CatalogFilter::default()).await.unwrap();
    let _ = catalog.list(&CatalogFilter::default()).await.unwrap();
}

#[tokio::test]
async fn list_caches_404_as_empty_per_profession() {
    let server = MockServer::start().await;
    // 404 is durable per (category, profession) — must cache. Test with
    // a single profession filter to keep the assertion focused.
    Mock::given(method("GET"))
        .and(path("/builds/raids/guardian"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri()).unwrap();
    let filter = CatalogFilter {
        profession: Some("Guardian".to_owned()),
        ..Default::default()
    };
    let out = catalog.list(&filter).await.unwrap();
    assert!(out.is_empty(), "404 should map to empty listing, not error");
    let _ = catalog.list(&filter).await.unwrap();
}

#[tokio::test]
async fn list_caches_403_under_cooldown_then_retries() {
    let server = MockServer::start().await;
    // 403 is transient. We cache the empty for `error_cooldown` so a
    // tight burst of calls doesn't hammer the upstream. After the
    // cooldown elapses, the next call refetches.
    //
    // Set cooldown to zero so the second call immediately refetches —
    // exactly two GETs in total.
    Mock::given(method("GET"))
        .and(path("/builds/raids/guardian"))
        .respond_with(ResponseTemplate::new(403))
        .expect(2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_base_url(server.uri())
        .unwrap()
        .with_error_cooldown(Duration::ZERO);
    let filter = CatalogFilter {
        profession: Some("Guardian".to_owned()),
        ..Default::default()
    };
    let first = catalog.list(&filter).await.unwrap();
    assert!(first.is_empty(), "403 should yield empty, not error");
    let second = catalog.list(&filter).await.unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn list_does_not_refetch_within_403_cooldown() {
    let server = MockServer::start().await;
    // With the default (long) cooldown, two list() calls in the same
    // session produce exactly ONE GET — the second hits the cooldown
    // cache rather than re-bombarding the upstream. This is the
    // anti-hammering guarantee.
    Mock::given(method("GET"))
        .and(path("/builds/raids/guardian"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    // Long cooldown ensures both calls fall inside the window.
    let catalog = SnowCrowsCatalog::with_base_url(server.uri())
        .unwrap()
        .with_error_cooldown(Duration::from_secs(60 * 60));
    let filter = CatalogFilter {
        profession: Some("Guardian".to_owned()),
        ..Default::default()
    };
    let _ = catalog.list(&filter).await.unwrap();
    let _ = catalog.list(&filter).await.unwrap();
}

#[tokio::test]
async fn list_with_non_default_gamemode_fetches_per_profession() {
    let server = MockServer::start().await;
    // Single-profession test path. gamemode=wvw + profession=elementalist
    // should produce exactly one GET against /builds/wvw/elementalist.
    Mock::given(method("GET"))
        .and(path("/builds/wvw/elementalist"))
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
        profession: Some("elementalist".to_owned()),
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
    // Both `open_world` (schema enum) and `open-world` (URL path) must
    // resolve to the same category. With profession=ranger filter we
    // expect one path; two calls (one per spelling) × cache TTL 0 = 2 GETs.
    Mock::given(method("GET"))
        .and(path("/builds/open-world/ranger"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html><body>
                <a href="/builds/open-world/ranger/condi-soulbeast"><h2>Condi Soulbeast</h2></a>
            </body></html>"#,
        ))
        .expect(2)
        .mount(&server)
        .await;

    let catalog = SnowCrowsCatalog::with_options(server.uri(), Duration::from_secs(0)).unwrap();
    for gm in ["open_world", "open-world"] {
        let filter = CatalogFilter {
            gamemode: Some(gm.to_owned()),
            profession: Some("ranger".to_owned()),
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
