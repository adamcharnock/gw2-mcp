//! Verify that `Service` caches catalog list/fetch responses with
//! `WIKI_TTL` semantics — same TTL math as wiki search.
//!
//! Each test uses `FakeCatalog` so we can count upstream invocations
//! exactly; the in-memory `TestCache` shares its clock with the test.

mod common;

use std::sync::Arc;
use std::time::Duration;

use gw2_mcp::ports::{CatalogFilter, CatalogRegistry};
use gw2_mcp::service::WIKI_TTL;

use crate::common::{
    FakeCatalog, FakeGw2Api, FakeWiki, TestCache, TestClock, build_detail,
    build_service_with_catalogs, build_summary,
};

#[tokio::test]
async fn catalog_list_caches_within_ttl() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let cat = FakeCatalog::new("fake");
    cat.set_list(vec![build_summary("guardian/x", "Guardian")]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock, registry);

    let first = svc
        .list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(cat.list_calls(), 1);

    let second = svc
        .list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(cat.list_calls(), 1, "second call must be cached");
}

#[tokio::test]
async fn catalog_list_cache_separates_by_filter() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let cat = FakeCatalog::new("fake");
    cat.set_list(vec![]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock, registry);

    svc.list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    svc.list_catalog_builds(
        "fake",
        CatalogFilter {
            profession: Some("guardian".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    svc.list_catalog_builds(
        "fake",
        CatalogFilter {
            profession: Some("guardian".to_owned()),
            limit: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        cat.list_calls(),
        3,
        "three different filters must trigger three upstream calls"
    );
}

#[tokio::test]
async fn catalog_list_refetches_after_ttl_expiry() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let cat = FakeCatalog::new("fake");
    cat.set_list(vec![build_summary("guardian/x", "Guardian")]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock.clone(), registry);

    svc.list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    clock.advance(WIKI_TTL + Duration::from_secs(1));
    svc.list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    assert_eq!(cat.list_calls(), 2, "expired cache must trigger refetch");
}

#[tokio::test]
async fn catalog_fetch_caches_per_slug() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let cat = FakeCatalog::new("fake");
    cat.set_fetch(build_detail("guardian/x", "Guardian"));
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock, registry);

    svc.get_catalog_build("fake", "guardian/x").await.unwrap();
    svc.get_catalog_build("fake", "guardian/x").await.unwrap();
    assert_eq!(
        cat.fetch_calls(),
        1,
        "second fetch of same slug must cache hit"
    );

    // Different slug — new upstream call.
    cat.set_fetch(build_detail("guardian/y", "Guardian"));
    svc.get_catalog_build("fake", "guardian/y").await.unwrap();
    assert_eq!(cat.fetch_calls(), 2);
}

#[tokio::test]
async fn catalog_fetch_does_not_cache_errors() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    // No fetch_response set → FakeCatalog returns NotFound.
    let cat = FakeCatalog::new("fake");
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock, registry);

    let _ = svc.get_catalog_build("fake", "missing").await;
    let _ = svc.get_catalog_build("fake", "missing").await;
    assert_eq!(
        cat.fetch_calls(),
        2,
        "errors must not poison the cache — second call should retry"
    );
}

#[tokio::test]
async fn catalog_unknown_source_does_not_call_anything() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let cat = FakeCatalog::new("fake");
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc = build_service_with_catalogs(gw2, wiki, cache, clock, registry);

    let err = svc
        .get_catalog_build("not-registered", "anything")
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no such build source"));
    assert_eq!(cat.fetch_calls(), 0);
}
