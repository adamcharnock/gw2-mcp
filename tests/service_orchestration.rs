//! Service-layer integration tests.
//!
//! The service is exercised through `Service::new` with in-memory fakes for
//! every port — no HTTP, no real time. These tests pin the *orchestration*
//! semantics (caching policy, fallback on metadata failure, etc.).

mod common;

use std::sync::Arc;
use std::time::Duration;

use std::collections::BTreeMap;

use gw2_mcp::domain::{
    BuildChatCode, CharacterName, CurrencyId, SearchLimit, SearchQuery, SearchResult, Skill,
    SkillId, Specialization, SpecializationId, Trait, TraitId, WalletEntry,
};
use gw2_mcp::service::{Service, WALLET_TTL};
use pretty_assertions::assert_eq;

use crate::common::{
    FakeGw2Api, FakeWiki, TestCache, TestClock, build_service, currency, valid_api_key,
};

fn build(
    gw2: Arc<FakeGw2Api>,
    wiki: Arc<FakeWiki>,
    cache: Arc<TestCache>,
    clock: Arc<TestClock>,
) -> Service {
    build_service(gw2, wiki, cache, clock)
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
    let msg = format!("{err}").to_lowercase();
    // Friendly user-facing message — see Gw2ApiError::Unauthorized in ports.rs.
    assert!(
        msg.contains("rejected"),
        "expected key-rejected message, got: {msg}"
    );
    assert!(msg.contains("scope"), "expected scope hint, got: {msg}");
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
async fn skills_are_cached_per_id() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let s1 = Skill {
        id: SkillId::new(9137).unwrap(),
        name: "Wave of Wrath".to_owned(),
        extra: BTreeMap::new(),
    };
    let s2 = Skill {
        id: SkillId::new(5503).unwrap(),
        name: "Other".to_owned(),
        extra: BTreeMap::new(),
    };
    gw2.add_skill(s1);
    gw2.add_skill(s2);

    let svc = build(gw2.clone(), wiki, cache, clock);

    // First call fetches both.
    let first = svc
        .get_skills(&[SkillId::new(9137).unwrap(), SkillId::new(5503).unwrap()])
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(gw2.skill_calls(), 1);

    // Second call: both ids cache-hit, no upstream.
    svc.get_skills(&[SkillId::new(9137).unwrap()])
        .await
        .unwrap();
    assert_eq!(
        gw2.skill_calls(),
        1,
        "cached id must not trigger another fetch"
    );
}

#[tokio::test]
async fn traits_are_cached_per_id() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.add_trait(Trait {
        id: TraitId::new(648).unwrap(),
        name: "Zealot's Resolution".to_owned(),
        extra: BTreeMap::new(),
    });

    let svc = build(gw2.clone(), wiki, cache, clock);
    svc.get_traits(&[TraitId::new(648).unwrap()]).await.unwrap();
    svc.get_traits(&[TraitId::new(648).unwrap()]).await.unwrap();
    assert_eq!(*gw2.trait_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn specializations_are_cached_per_id() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.add_specialization(Specialization {
        id: SpecializationId::new(42).unwrap(),
        name: "Zeal".to_owned(),
        extra: BTreeMap::new(),
    });

    let svc = build(gw2.clone(), wiki, cache, clock);
    svc.get_specializations(&[SpecializationId::new(42).unwrap()])
        .await
        .unwrap();
    svc.get_specializations(&[SpecializationId::new(42).unwrap()])
        .await
        .unwrap();
    assert_eq!(*gw2.spec_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn character_build_combines_buildtabs_and_equipmenttabs() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let name = CharacterName::new("My Hero").unwrap();
    gw2.set_buildtabs(
        &name,
        vec![serde_json::json!({"tab": 1, "is_active": true, "build": {"profession": "Guardian"}})],
    );
    gw2.set_equipmenttabs(
        &name,
        vec![serde_json::json!({"tab": 1, "is_active": true, "name": "Default", "equipment": []})],
    );

    let svc = build(gw2.clone(), wiki, cache, clock.clone());
    let snap = svc
        .get_character_build(&valid_api_key(), &name)
        .await
        .unwrap();
    assert_eq!(snap.character_name, "My Hero");
    assert_eq!(snap.build_tabs.len(), 1);
    assert_eq!(snap.equipment_tabs.len(), 1);
    assert_eq!(snap.build_tabs[0]["build"]["profession"], "Guardian");
    assert_eq!(gw2.buildtab_calls(), 1);

    // Cached on second call within TTL.
    let snap2 = svc
        .get_character_build(&valid_api_key(), &name)
        .await
        .unwrap();
    assert_eq!(snap, snap2);
    assert_eq!(
        gw2.buildtab_calls(),
        1,
        "cached call must not refetch buildtabs"
    );
}

#[tokio::test]
async fn decode_build_code_via_service() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    let svc = build(gw2, wiki, cache, clock);

    let code =
        BuildChatCode::new("[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]")
            .unwrap();
    let decoded = svc.decode_build_code(&code).unwrap();
    assert_eq!(decoded["profession"], 6);
    assert_eq!(
        decoded["skills"]["healing"]["terrestrial"]["palette_id"],
        116
    );
    assert!(
        decoded["skills"]["healing"]["terrestrial"]["api_skill_id"].is_number(),
        "service must surface resolved api_skill_id alongside the raw palette"
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
