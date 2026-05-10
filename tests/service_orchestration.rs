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
use gw2_mcp::service::{Service, TabSelector, WALLET_TTL};
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
    // tab=Active by default — and the seeded tab is the active one.
    let snap = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::Active)
        .await
        .unwrap();
    assert_eq!(snap.character_name, "My Hero");
    assert_eq!(snap.build_tabs.len(), 1);
    assert_eq!(snap.equipment_tabs.len(), 1);
    assert_eq!(snap.build_tabs[0]["build"]["profession"], "Guardian");
    assert_eq!(gw2.buildtab_calls(), 1);

    // Cached on second call within TTL.
    let snap2 = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::Active)
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
async fn character_build_active_picks_only_active_tab() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let name = CharacterName::new("My Hero").unwrap();
    let buildtabs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/buildtabs_sample.json")).unwrap();
    let equipmenttabs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/equipmenttabs_sample.json")).unwrap();
    gw2.set_buildtabs(&name, buildtabs);
    gw2.set_equipmenttabs(&name, equipmenttabs);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let snap = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::Active)
        .await
        .unwrap();

    assert_eq!(snap.build_tabs.len(), 1, "only the active build tab");
    assert_eq!(
        snap.equipment_tabs.len(),
        1,
        "only the active equipment tab"
    );
    assert_eq!(
        snap.build_tabs[0]["tab"], 2,
        "fixture: tab 2 is_active=true"
    );
    assert_eq!(snap.equipment_tabs[0]["tab"], 1);
}

#[tokio::test]
async fn character_build_strips_cosmetic_equipment_fields() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let name = CharacterName::new("My Hero").unwrap();
    gw2.set_buildtabs(&name, Vec::new());
    let equipmenttabs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/equipmenttabs_sample.json")).unwrap();
    gw2.set_equipmenttabs(&name, equipmenttabs);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let snap = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::Active)
        .await
        .unwrap();

    let pieces = snap.equipment_tabs[0]["equipment"].as_array().unwrap();
    assert!(!pieces.is_empty(), "fixture has equipment pieces");
    for piece in pieces {
        for stripped in ["dyes", "bound_to", "binding", "location"] {
            assert!(
                piece.get(stripped).is_none(),
                "expected {stripped} stripped, got: {piece}"
            );
        }
        // Useful fields preserved.
        assert!(piece.get("id").is_some());
        assert!(piece.get("slot").is_some());
    }
}

#[tokio::test]
async fn character_build_resolves_skill_and_trait_names_on_active_tab() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let name = CharacterName::new("My Hero").unwrap();
    let buildtabs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/buildtabs_sample.json")).unwrap();
    gw2.set_buildtabs(&name, buildtabs);
    gw2.set_equipmenttabs(&name, Vec::new());

    // Seed a skill and a trait we expect to see on the active tab.
    // Active tab (tab 2) has skills.heal=41714, utilities[0]=40915 and spec ids 16/49/62.
    gw2.add_skill(Skill {
        id: SkillId::new(41714).unwrap(),
        name: "Litany of Wrath".to_owned(),
        extra: BTreeMap::new(),
    });
    gw2.add_trait(Trait {
        id: TraitId::new(566).unwrap(),
        name: "Piercing Light".to_owned(),
        extra: BTreeMap::new(),
    });
    gw2.add_specialization(Specialization {
        id: SpecializationId::new(16).unwrap(),
        name: "Radiance".to_owned(),
        extra: BTreeMap::new(),
    });

    let svc = build(gw2.clone(), wiki, cache, clock);
    let snap = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::Active)
        .await
        .unwrap();

    let tab = &snap.build_tabs[0];
    // Heal skill becomes {id, name}.
    assert_eq!(tab["build"]["skills"]["heal"]["id"], 41714);
    assert_eq!(tab["build"]["skills"]["heal"]["name"], "Litany of Wrath");
    // First spec gets a name field.
    let first_spec = &tab["build"]["specializations"][0];
    assert_eq!(first_spec["id"], 16);
    assert_eq!(first_spec["name"], "Radiance");
    // First trait of first spec inlines the name.
    assert_eq!(first_spec["traits"][0]["id"], 566);
    assert_eq!(first_spec["traits"][0]["name"], "Piercing Light");
}

#[tokio::test]
async fn character_build_all_returns_every_tab() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let name = CharacterName::new("My Hero").unwrap();
    let buildtabs: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/buildtabs_sample.json")).unwrap();
    gw2.set_buildtabs(&name, buildtabs);
    gw2.set_equipmenttabs(&name, Vec::new());

    let svc = build(gw2.clone(), wiki, cache, clock);
    let snap = svc
        .get_character_build(&valid_api_key(), &name, TabSelector::All)
        .await
        .unwrap();
    assert_eq!(snap.build_tabs.len(), 3, "fixture has 3 build tabs");
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
    let decoded = svc.decode_build_code(&code).await.unwrap();
    assert_eq!(decoded["profession"], 6);
    // Profession byte → profession_name surfaced for LLM ergonomics.
    assert_eq!(decoded["profession_name"], "Elementalist");
    assert_eq!(
        decoded["skills"]["healing"]["terrestrial"]["palette_id"],
        116
    );
    assert!(
        decoded["skills"]["healing"]["terrestrial"]["api_skill_id"].is_number(),
        "service must surface resolved api_skill_id alongside the raw palette"
    );
    // Each trait slot is now {position, trait_id} (with trait_id null when
    // we couldn't resolve — no spec data was seeded into the fake).
    let first_spec = &decoded["specializations"][0];
    assert!(first_spec["traits"]["adept"]["position"].is_number());
    assert!(first_spec["traits"]["adept"].get("trait_id").is_some());
}

#[tokio::test]
async fn decode_build_code_resolves_trait_ids_via_specialization_lookup() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    // Power Dragonhunter: profession byte = 1 (Guardian).
    // The build code's first specialization is Radiance (id 16). We seed
    // the FakeGw2Api with Radiance's real major_traits array so we can
    // assert the position-to-trait_id mapping.
    // Radiance major_traits (from the real GW2 API):
    //   adept:       [566, 567, 1686]
    //   master:      [589, 568, 569]
    //   grandmaster: [563, 562, 564]
    // (positions referenced by the chat code: adept=2, master=2, grandmaster=2)
    let mut radiance_extra = BTreeMap::new();
    radiance_extra.insert(
        "major_traits".to_owned(),
        serde_json::json!([566, 567, 1686, 589, 568, 569, 563, 562, 564]),
    );
    gw2.add_specialization(Specialization {
        id: SpecializationId::new(16).unwrap(),
        name: "Radiance".to_owned(),
        extra: radiance_extra,
    });

    let svc = build(gw2.clone(), wiki, cache, clock);
    let code = BuildChatCode::new(
        "[&DQEQPyo6GzkmDyYPihJIAUgBLQH+ALkBtRI3AQAAAAAAAAAAAAAAAAAAAAACMgAjAAA=]",
    )
    .unwrap();
    let decoded = svc.decode_build_code(&code).await.unwrap();
    assert_eq!(decoded["profession"], 1);
    assert_eq!(decoded["profession_name"], "Guardian");

    // Find the Radiance spec in the decoded output.
    let arr = decoded["specializations"].as_array().unwrap();
    let radiance = arr
        .iter()
        .find(|s| s["id"].as_u64() == Some(16))
        .expect("first spec should be Radiance");

    // Each tier slot is {position, trait_id}.
    let adept = &radiance["traits"]["adept"];
    let master = &radiance["traits"]["master"];
    let grandmaster = &radiance["traits"]["grandmaster"];
    assert!(adept["position"].as_u64().unwrap() >= 1);
    assert!(adept["trait_id"].is_number());
    // Mapping: position p in tier T -> major_traits[(T-1)*3 + (p-1)]
    let p_a = usize::try_from(adept["position"].as_u64().unwrap()).unwrap();
    let p_m = usize::try_from(master["position"].as_u64().unwrap()).unwrap();
    let p_g = usize::try_from(grandmaster["position"].as_u64().unwrap()).unwrap();
    let expected_a = [566u64, 567, 1686][p_a - 1];
    let expected_m = [589u64, 568, 569][p_m - 1];
    let expected_g = [563u64, 562, 564][p_g - 1];
    assert_eq!(adept["trait_id"].as_u64().unwrap(), expected_a);
    assert_eq!(master["trait_id"].as_u64().unwrap(), expected_m);
    assert_eq!(grandmaster["trait_id"].as_u64().unwrap(), expected_g);
}

#[tokio::test]
async fn get_skills_view_summary_drops_facts_and_icon() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let mut extra = BTreeMap::new();
    extra.insert("facts".to_owned(), serde_json::json!([{"type": "Damage"}]));
    extra.insert(
        "icon".to_owned(),
        serde_json::json!("https://render.guildwars2.com/x.png"),
    );
    extra.insert("description".to_owned(), serde_json::json!("Strike."));
    extra.insert("type".to_owned(), serde_json::json!("Weapon"));
    extra.insert("slot".to_owned(), serde_json::json!("Weapon_1"));
    gw2.add_skill(Skill {
        id: SkillId::new(9137).unwrap(),
        name: "Wave of Wrath".to_owned(),
        extra,
    });

    let svc = build(gw2, wiki, cache, clock);
    // summary=true (default in MCP wrapper) — the projected payload omits
    // facts[] and icon.
    let summary = svc
        .get_skills_view(&[SkillId::new(9137).unwrap()], true)
        .await
        .unwrap();
    let entry = &summary["9137"];
    assert_eq!(entry["name"], "Wave of Wrath");
    assert_eq!(entry["description"], "Strike.");
    assert!(entry.get("facts").is_none(), "summary must drop facts[]");
    assert!(entry.get("icon").is_none(), "summary must drop icon");

    // summary=false returns the full shape including facts.
    let full = svc
        .get_skills_view(&[SkillId::new(9137).unwrap()], false)
        .await
        .unwrap();
    assert!(full["9137"].get("facts").is_some());
    assert!(full["9137"].get("icon").is_some());
}

#[tokio::test]
async fn items_are_cached_per_id() {
    use gw2_mcp::domain::{Item, ItemId};

    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.add_item(Item {
        id: ItemId::new(95438).unwrap(),
        name: "Test Helm".to_owned(),
        extra: BTreeMap::new(),
    });
    let svc = build(gw2.clone(), wiki, cache, clock);
    svc.get_items(&[ItemId::new(95438).unwrap()]).await.unwrap();
    svc.get_items(&[ItemId::new(95438).unwrap()]).await.unwrap();
    assert_eq!(*gw2.item_calls.lock().unwrap(), 1);
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
