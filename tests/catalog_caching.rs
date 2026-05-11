//! Verify catalog call routing through `Service`.
//!
//! Caching for catalog calls is now the adapter's responsibility (Snow
//! Crows runs its own per-(category, profession) cache with rate-limit
//! cooldowns; `MetaBattle` and Discretize hit cheap CDN-backed upstreams
//! and don't cache locally). The service layer is a thin router: it
//! looks up the source in the registry and forwards. These tests pin
//! that contract — every call hits the adapter, source lookup is
//! correctly typed, and errors propagate without poisoning anything.

mod common;

use std::sync::Arc;

use gw2_mcp::domain::BuildSlug;
use gw2_mcp::ports::{CatalogFilter, CatalogRegistry};

use crate::common::{
    FakeCatalog, FakeGw2Api, FakeWiki, TestCache, TestClock, build_detail,
    build_service_with_catalogs, build_summary,
};

#[tokio::test]
async fn list_forwards_every_call_to_adapter() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let cat = FakeCatalog::new("fake");
    cat.set_list(vec![build_summary("guardian/x", "Guardian")]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc =
        build_service_with_catalogs(FakeGw2Api::new(), FakeWiki::new(), cache, clock, registry);

    svc.list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    svc.list_catalog_builds("fake", CatalogFilter::default())
        .await
        .unwrap();
    // No service-layer cache → each call reaches the adapter. Adapters
    // that need caching (Snow Crows) own it themselves.
    assert_eq!(
        cat.list_calls(),
        2,
        "service must forward every list call to the adapter"
    );
}

#[tokio::test]
async fn list_separates_filters() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let cat = FakeCatalog::new("fake");
    cat.set_list(vec![]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc =
        build_service_with_catalogs(FakeGw2Api::new(), FakeWiki::new(), cache, clock, registry);

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
        "every distinct call reaches the adapter (no service-level dedup)"
    );
}

#[tokio::test]
async fn fetch_forwards_every_call_to_adapter() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let cat = FakeCatalog::new("fake");
    cat.set_fetch(build_detail("guardian/x", "Guardian"));
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc =
        build_service_with_catalogs(FakeGw2Api::new(), FakeWiki::new(), cache, clock, registry);

    let slug = BuildSlug::new("guardian/x").unwrap();
    svc.get_catalog_build("fake", &slug).await.unwrap();
    svc.get_catalog_build("fake", &slug).await.unwrap();
    assert_eq!(
        cat.fetch_calls(),
        2,
        "service must forward every fetch call to the adapter"
    );
}

#[tokio::test]
async fn fetch_propagates_errors_without_poisoning() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    // No fetch_response set → FakeCatalog returns NotFound.
    let cat = FakeCatalog::new("fake");
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc =
        build_service_with_catalogs(FakeGw2Api::new(), FakeWiki::new(), cache, clock, registry);

    let slug = BuildSlug::new("missing").unwrap();
    let _ = svc.get_catalog_build("fake", &slug).await;
    let _ = svc.get_catalog_build("fake", &slug).await;
    assert_eq!(
        cat.fetch_calls(),
        2,
        "errors must not affect subsequent calls — every call retries the adapter"
    );
}

#[tokio::test]
async fn unknown_source_does_not_call_anything() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let cat = FakeCatalog::new("fake");
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let svc =
        build_service_with_catalogs(FakeGw2Api::new(), FakeWiki::new(), cache, clock, registry);

    let slug = BuildSlug::new("anything").unwrap();
    let err = svc
        .get_catalog_build("not-registered", &slug)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("no such build source"));
    assert_eq!(cat.fetch_calls(), 0);
}
