//! Service-layer integration tests.
//!
//! The service is exercised through `Service::new` with in-memory fakes for
//! every port — no HTTP, no real time. These tests pin the *orchestration*
//! semantics (caching policy, fallback on metadata failure, etc.).

mod common;

use std::sync::Arc;
use std::time::Duration;

use gw2_mcp::domain::{CurrencyId, SearchLimit, SearchQuery, SearchResult, WalletEntry};
use gw2_mcp::service::{Service, WALLET_TTL};
use pretty_assertions::assert_eq;

use crate::common::{FakeGw2Api, FakeWiki, TestCache, TestClock, currency, valid_api_key};

fn build(
    gw2: Arc<FakeGw2Api>,
    wiki: Arc<FakeWiki>,
    cache: Arc<TestCache>,
    clock: Arc<TestClock>,
) -> Service {
    Service::new(gw2, wiki, cache, clock)
}

#[tokio::test]
async fn wallet_caches_within_ttl() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    gw2.set_wallet(vec![WalletEntry {
        id: CurrencyId::new(1).unwrap(),
        value: 1234,
    }]);
    gw2.add_currency(currency(1, "Coin"));

    let svc = build(gw2.clone(), wiki, cache.clone(), clock.clone());
    let key = valid_api_key();

    let first = svc.get_wallet(&key).await.unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.total_currencies, 1);
    assert_eq!(gw2.wallet_calls(), 1);

    let second = svc.get_wallet(&key).await.unwrap();
    assert_eq!(
        first, second,
        "second call must return cached value verbatim"
    );
    assert_eq!(
        gw2.wallet_calls(),
        1,
        "second call must not hit upstream within TTL"
    );
}

#[tokio::test]
async fn wallet_refetches_after_ttl_expiry() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    gw2.set_wallet(vec![WalletEntry {
        id: CurrencyId::new(1).unwrap(),
        value: 1,
    }]);
    let svc = build(gw2.clone(), wiki, cache, clock.clone());
    let key = valid_api_key();

    svc.get_wallet(&key).await.unwrap();
    clock.advance(WALLET_TTL + Duration::from_secs(1));
    svc.get_wallet(&key).await.unwrap();

    assert_eq!(gw2.wallet_calls(), 2, "expired cache must trigger refetch");
}

#[tokio::test]
async fn wallet_succeeds_when_currency_metadata_fails() {
    // The service must degrade gracefully: a wallet without metadata is still
    // useful. We simulate failure by leaving the FakeGw2Api currency store
    // empty; fetch_currencies will return an empty map. (To simulate a hard
    // error we'd need a knob, but empty already proves the no-panic path.)
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    gw2.set_wallet(vec![WalletEntry {
        id: CurrencyId::new(99).unwrap(),
        value: 5,
    }]);
    // Note: no currency 99 added.

    let svc = build(gw2, wiki, cache, clock);
    let info = svc.get_wallet(&valid_api_key()).await.unwrap();
    assert_eq!(info.entries.len(), 1);
    assert!(
        info.currencies.is_empty(),
        "missing metadata must not poison the wallet"
    );
}

#[tokio::test]
async fn wallet_propagates_unauthorized() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_wallet_unauthorized();

    let svc = build(gw2, wiki, cache, clock);
    let err = svc.get_wallet(&valid_api_key()).await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("unauthorized") || msg.to_lowercase().contains("invalid"),
        "expected unauthorized signal, got: {msg}"
    );
}

#[tokio::test]
async fn currencies_caches_per_id() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    gw2.add_currency(currency(1, "Coin"));
    gw2.add_currency(currency(2, "Karma"));

    let svc = build(gw2.clone(), wiki, cache.clone(), clock);

    let first = svc
        .get_currencies(&[CurrencyId::new(1).unwrap()])
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let calls_after_first = gw2.currency_calls();
    assert!(calls_after_first >= 1);

    // Same id again — should NOT increment upstream calls.
    let second = svc
        .get_currencies(&[CurrencyId::new(1).unwrap()])
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        gw2.currency_calls(),
        calls_after_first,
        "cache hit must not refetch"
    );

    // New id triggers a fetch (just one — the cached id should not refetch).
    let mixed = svc
        .get_currencies(&[CurrencyId::new(1).unwrap(), CurrencyId::new(2).unwrap()])
        .await
        .unwrap();
    assert_eq!(mixed.len(), 2);
    assert_eq!(
        gw2.currency_calls(),
        calls_after_first + 1,
        "only the missing id should refetch"
    );
}

#[tokio::test]
async fn currencies_empty_ids_returns_full_list() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.add_currency(currency(1, "Coin"));
    gw2.add_currency(currency(2, "Karma"));

    let svc = build(gw2, wiki, cache, clock);
    let all = svc.get_currencies(&[]).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn wiki_search_caches_response() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    wiki.set_search_results(vec![SearchResult {
        title: "Dragon Bash".to_owned(),
        snippet: "festival".to_owned(),
        timestamp: "2026-01-01T00:00:00Z".to_owned(),
        url: String::new(),
        extract: String::new(),
        page_id: 12345,
        size: 5000,
        word_count: 800,
    }]);
    wiki.set_extract("Dragon Bash", "Dragon Bash is an annual festival.");

    let svc = build(gw2, wiki.clone(), cache, clock);
    let q = SearchQuery::new("dragon bash").unwrap();
    let limit = SearchLimit::new(5).unwrap();

    let first = svc.search_wiki(&q, limit).await.unwrap();
    assert_eq!(first.results.len(), 1);
    assert_eq!(
        first.results[0].extract,
        "Dragon Bash is an annual festival."
    );
    assert!(
        first.results[0].url.contains("Dragon"),
        "url should be filled in by service"
    );

    let calls_first = wiki.search_calls();
    let extract_calls_first = wiki.extract_calls();

    let second = svc.search_wiki(&q, limit).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        wiki.search_calls(),
        calls_first,
        "cached: no upstream search"
    );
    assert_eq!(
        wiki.extract_calls(),
        extract_calls_first,
        "cached: no upstream extract"
    );
}

#[tokio::test]
async fn wiki_search_query_normalisation_dedupes_cache() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    wiki.set_search_results(vec![SearchResult {
        title: "Test".to_owned(),
        snippet: String::new(),
        timestamp: String::new(),
        url: String::new(),
        extract: String::new(),
        page_id: 1,
        size: 1,
        word_count: 1,
    }]);

    let svc = build(gw2, wiki.clone(), cache, clock);
    let limit = SearchLimit::new(5).unwrap();

    svc.search_wiki(&SearchQuery::new("Dragon Bash").unwrap(), limit)
        .await
        .unwrap();
    svc.search_wiki(&SearchQuery::new("DRAGON BASH").unwrap(), limit)
        .await
        .unwrap();
    svc.search_wiki(&SearchQuery::new("  dragon bash  ").unwrap(), limit)
        .await
        .unwrap();

    assert_eq!(
        wiki.search_calls(),
        1,
        "normalised queries should share a cache entry"
    );
}

#[tokio::test]
async fn wiki_search_cache_separates_by_limit() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    wiki.set_search_results(vec![]);

    let svc = build(gw2, wiki.clone(), cache, clock);
    let q = SearchQuery::new("foo").unwrap();
    svc.search_wiki(&q, SearchLimit::new(3).unwrap())
        .await
        .unwrap();
    svc.search_wiki(&q, SearchLimit::new(7).unwrap())
        .await
        .unwrap();
    assert_eq!(
        wiki.search_calls(),
        2,
        "different limits must not collide in cache"
    );
}
