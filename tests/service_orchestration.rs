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
    Account, AccountAchievement, AccountMastery, BuildChatCode, CharacterName, CurrencyId,
    SearchLimit, SearchQuery, SearchResult, Skill, SkillId, Specialization, SpecializationId,
    Trait, TraitId, WalletEntry, WizardsVaultObjective, WizardsVaultTrack,
};
use gw2_mcp::service::{DAILIES_TTL, DailiesWhich, Service, TabSelector, WALLET_TTL};
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
        holding_cap: None,
        weekly_earn_cap: None,
        at_risk: None,
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
        holding_cap: None,
        weekly_earn_cap: None,
        at_risk: None,
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
        holding_cap: None,
        weekly_earn_cap: None,
        at_risk: None,
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
async fn bank_summary_groups_by_item_id_and_sums_counts() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    // Three slots: two stacks of item 42 (10 + 5 = 15) and one of
    // item 43 (count 1). Summary should collapse to two rows.
    gw2.set_bank(vec![
        gw2_mcp::domain::InventorySlot {
            id: 42,
            count: 10,
            binding: None,
            bound_to: None,
            charges: None,
        },
        gw2_mcp::domain::InventorySlot {
            id: 42,
            count: 5,
            binding: Some("Account".into()),
            bound_to: None,
            charges: None,
        },
        gw2_mcp::domain::InventorySlot {
            id: 43,
            count: 1,
            binding: None,
            bound_to: None,
            charges: None,
        },
    ]);

    let svc = build(gw2.clone(), wiki, cache.clone(), clock.clone());
    let key = valid_api_key();

    let snap = svc.get_account_bank(&key, true).await.unwrap();
    assert!(snap.summary);
    assert_eq!(snap.items.len(), 2, "summary collapses two slots of id=42");
    assert_eq!(snap.unique_item_count, 2);
    assert_eq!(snap.used_slots, 3);
    // Largest stockpile first.
    assert_eq!(snap.items[0].id, 42);
    assert_eq!(snap.items[0].count, 15);
    assert!(snap.items[0].binding.is_none(), "summary drops binding");
    assert_eq!(snap.items[1].id, 43);
    assert_eq!(snap.items[1].count, 1);
}

#[tokio::test]
async fn bank_full_mode_preserves_per_slot_detail() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    gw2.set_bank(vec![
        gw2_mcp::domain::InventorySlot {
            id: 42,
            count: 10,
            binding: Some("Account".into()),
            bound_to: None,
            charges: None,
        },
        gw2_mcp::domain::InventorySlot {
            id: 42,
            count: 5,
            binding: Some("Character".into()),
            bound_to: Some("Hero".into()),
            charges: None,
        },
    ]);

    let svc = build(gw2, wiki, cache, clock);
    let snap = svc.get_account_bank(&valid_api_key(), false).await.unwrap();
    assert!(!snap.summary);
    assert_eq!(snap.items.len(), 2, "full mode keeps per-slot rows");
    assert_eq!(snap.items[0].binding.as_deref(), Some("Account"));
    assert_eq!(snap.items[1].binding.as_deref(), Some("Character"));
    assert_eq!(snap.items[1].bound_to.as_deref(), Some("Hero"));
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
        url: String::new(),
        extract: String::new(),
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
        url: String::new(),
        extract: String::new(),
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

// ---------------------------------------------------------------------------
// Tier 6A — account / progression / dailies orchestration tests.
// ---------------------------------------------------------------------------

fn fake_account() -> Account {
    let raw = include_str!("fixtures/account_basic.json");
    serde_json::from_str(raw).unwrap()
}

#[tokio::test]
async fn account_caches_within_wallet_ttl() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_account(fake_account());

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();

    let first = svc.get_account(&key).await.unwrap();
    assert_eq!(first.name, "Snowflake.1234");
    assert_eq!(gw2.account_calls(), 1);

    svc.get_account(&key).await.unwrap();
    assert_eq!(gw2.account_calls(), 1, "cache hit must not refetch");
}

#[tokio::test]
async fn account_refetches_after_wallet_ttl_expiry() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_account(fake_account());

    let svc = build(gw2.clone(), wiki, cache, clock.clone());
    let key = valid_api_key();
    svc.get_account(&key).await.unwrap();
    clock.advance(WALLET_TTL + Duration::from_secs(1));
    svc.get_account(&key).await.unwrap();
    assert_eq!(gw2.account_calls(), 2, "expired cache must refetch");
}

#[tokio::test]
async fn list_characters_returns_names_and_caches() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_characters_list(vec!["Snowflake".to_owned(), "Vesta Vey".to_owned()]);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();
    let first = svc.list_characters(&key).await.unwrap();
    assert_eq!(first.characters.len(), 2);
    assert_eq!(first.total, 2);
    svc.list_characters(&key).await.unwrap();
    assert_eq!(gw2.characters_list_calls(), 1, "cached on second call");
}

#[tokio::test]
async fn account_achievements_summary_drops_done_and_not_started() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    // Fixture mirrors tests/fixtures/account_achievements.json:
    // - id 100: not started (current=0, !done) -> dropped in summary
    // - id 200: in progress (5/10) -> KEPT
    // - id 300: completed (10/10, done) -> dropped
    // - id 400: done (no current/max) -> dropped
    // - id 500: in progress (7/25) -> KEPT
    // - id 600: not started (no current, !done) -> dropped
    gw2.set_achievements(vec![
        AccountAchievement {
            id: 100,
            current: Some(0),
            max: Some(10),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        },
        AccountAchievement {
            id: 200,
            current: Some(5),
            max: Some(10),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        },
        AccountAchievement {
            id: 300,
            current: Some(10),
            max: Some(10),
            done: true,
            bits: None,
            repeated: None,
            unlocked: None,
        },
        AccountAchievement {
            id: 400,
            current: None,
            max: None,
            done: true,
            bits: None,
            repeated: None,
            unlocked: None,
        },
        AccountAchievement {
            id: 500,
            current: Some(7),
            max: Some(25),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        },
        AccountAchievement {
            id: 600,
            current: None,
            max: None,
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        },
    ]);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();

    let summary = svc.get_account_achievements(&key, true).await.unwrap();
    let summary_ids: Vec<u32> = summary.achievements.iter().map(|a| a.progress.id).collect();
    assert_eq!(
        summary_ids,
        vec![200, 500],
        "summary keeps only in-progress"
    );
    assert!(summary.summary, "summary flag echoes the request");
    assert_eq!(summary.total, 2);

    let raw = svc.get_account_achievements(&key, false).await.unwrap();
    assert_eq!(
        raw.achievements.len(),
        6,
        "summary=false returns the full list"
    );
    assert!(!raw.summary);

    // Both calls share the cached upstream payload.
    assert_eq!(gw2.achievements_calls(), 1);
}

#[tokio::test]
async fn account_masteries_caches() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_masteries(vec![
        AccountMastery { id: 1, level: 4 },
        AccountMastery { id: 2, level: 6 },
    ]);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();
    let m = svc.get_account_masteries(&key).await.unwrap();
    assert_eq!(m.masteries.len(), 2);
    assert_eq!(m.total, 2);
    svc.get_account_masteries(&key).await.unwrap();
    assert_eq!(gw2.masteries_calls(), 1);
}

#[tokio::test]
async fn account_raids_and_dungeons_cache_independently() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_raids(vec!["vale_guardian".to_owned()]);
    gw2.set_dungeons(vec!["ascalonian_catacombs_story".to_owned()]);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();

    svc.get_account_raids(&key).await.unwrap();
    svc.get_account_raids(&key).await.unwrap();
    svc.get_account_dungeons(&key).await.unwrap();
    svc.get_account_dungeons(&key).await.unwrap();

    assert_eq!(gw2.raids_calls(), 1, "raids cached on second call");
    assert_eq!(gw2.dungeons_calls(), 1, "dungeons cached on second call");
}

#[tokio::test]
async fn wizards_vault_tracks_use_separate_cache_entries() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let daily = WizardsVaultTrack {
        meta_progress_current: 1,
        meta_progress_complete: 4,
        meta_reward_item_id: None,
        meta_reward_astral: 50,
        meta_reward_claimed: false,
        objectives: vec![WizardsVaultObjective {
            id: 1,
            title: "Complete an event".to_owned(),
            track: "PvE".to_owned(),
            acclaim: 25,
            progress_current: 0,
            progress_complete: 1,
            claimed: false,
        }],
    };
    let weekly = WizardsVaultTrack {
        meta_progress_current: 0,
        meta_progress_complete: 8,
        meta_reward_item_id: None,
        meta_reward_astral: 450,
        meta_reward_claimed: false,
        objectives: vec![WizardsVaultObjective {
            id: 2,
            title: "Defeat 50 enemies".to_owned(),
            track: "PvE".to_owned(),
            acclaim: 50,
            progress_current: 0,
            progress_complete: 50,
            claimed: false,
        }],
    };
    gw2.set_wizards_vault_daily(daily);
    gw2.set_wizards_vault_weekly(weekly);

    let svc = build(gw2.clone(), wiki, cache, clock);
    let key = valid_api_key();
    let d = svc.get_dailies(&key, DailiesWhich::Daily).await.unwrap();
    let w = svc.get_dailies(&key, DailiesWhich::Weekly).await.unwrap();
    assert_eq!(d.track.objectives[0].id, 1);
    assert_eq!(w.track.objectives[0].id, 2);
    assert_eq!(
        gw2.wizards_vault_calls(),
        2,
        "daily and weekly are independent cache entries"
    );

    // Second calls must hit the cache.
    svc.get_dailies(&key, DailiesWhich::Daily).await.unwrap();
    svc.get_dailies(&key, DailiesWhich::Weekly).await.unwrap();
    assert_eq!(gw2.wizards_vault_calls(), 2, "both cached on second access");
}

#[tokio::test]
async fn wizards_vault_refetches_after_dailies_ttl_expiry() {
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let svc = build(gw2.clone(), wiki, cache, clock.clone());
    let key = valid_api_key();
    svc.get_dailies(&key, DailiesWhich::Daily).await.unwrap();
    clock.advance(DAILIES_TTL + Duration::from_secs(1));
    svc.get_dailies(&key, DailiesWhich::Daily).await.unwrap();
    assert_eq!(gw2.wizards_vault_calls(), 2);
}

#[tokio::test]
async fn account_caches_under_fingerprinted_key_not_raw_secret() {
    // Two distinct API keys must NOT share a cache entry, and the second
    // key's call must trigger an upstream fetch — proves the fingerprint is
    // part of the cache key (otherwise both calls would collide on a
    // shared "account" key and the second call would return the first
    // call's payload).
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    gw2.set_account(fake_account());

    let svc = build(gw2.clone(), wiki, cache, clock);

    let key_a = valid_api_key();
    // A second key with a different prefix → different fingerprint.
    let key_b = gw2_mcp::domain::ApiKey::new(
        "11111111-2222-3333-4444-555555555555-66666666-7777-8888-9999-AAAAAAAAAAAA".to_owned(),
    )
    .unwrap();

    svc.get_account(&key_a).await.unwrap();
    svc.get_account(&key_b).await.unwrap();
    assert_eq!(
        gw2.account_calls(),
        2,
        "different keys must not share a cache entry"
    );
    // Sanity: a third call with the *first* key must hit the cache.
    svc.get_account(&key_a).await.unwrap();
    assert_eq!(gw2.account_calls(), 2, "first key still cached");
}

// ---------------------------------------------------------------------------
// list_maps_in_region
// ---------------------------------------------------------------------------

fn build_test_region(
    id: u32,
    name: &str,
    maps: Vec<(u32, &str, u32, u32)>,
) -> gw2_mcp::domain::Region {
    let mut m = BTreeMap::new();
    for (mid, mname, lo, hi) in maps {
        m.insert(
            mid,
            gw2_mcp::domain::RegionMap {
                id: mid,
                name: mname.to_owned(),
                min_level: lo,
                max_level: hi,
                default_floor: 1,
            },
        );
    }
    gw2_mcp::domain::Region {
        id,
        name: name.to_owned(),
        label_coord: None,
        maps: m,
    }
}

#[tokio::test]
async fn list_maps_in_region_by_name_returns_maps_in_level_order() {
    use gw2_mcp::service::RegionQuery;
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let mut continent1 = BTreeMap::new();
    continent1.insert(
        4,
        build_test_region(
            4,
            "Maguuma Jungle",
            vec![
                (22, "Brisban Wildlands", 15, 25),
                (873, "Caledon Forest", 1, 15),
                (39, "Mount Maelstrom", 60, 70),
            ],
        ),
    );
    gw2.set_regions_on_floor(1, 1, continent1);
    // Continent 2 unused but registered so the "any continent reachable"
    // check passes.
    gw2.set_regions_on_floor(2, 1, BTreeMap::new());

    let svc = build(gw2.clone(), wiki, cache, clock);
    let result = svc
        .list_maps_in_region(RegionQuery::Name("maguuma".to_owned()))
        .await
        .unwrap();

    assert_eq!(result.region_id, 4);
    assert_eq!(result.region_name, "Maguuma Jungle");
    assert_eq!(result.continent_id, 1);
    assert_eq!(result.total, 3);
    assert_eq!(
        result.maps.iter().map(|m| m.map_id).collect::<Vec<_>>(),
        vec![873, 22, 39],
        "sorted by min_level ascending"
    );

    // Second call must hit the cache.
    svc.list_maps_in_region(RegionQuery::Id(4)).await.unwrap();
    assert_eq!(
        gw2.regions_calls(),
        2,
        "first call walked both continent floors; second was cached"
    );
}

#[tokio::test]
async fn list_maps_in_region_ambiguous_name_errors() {
    use gw2_mcp::service::RegionQuery;
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();

    let mut continent1 = BTreeMap::new();
    continent1.insert(1, build_test_region(1, "Shiverpeak Mountains", vec![]));
    continent1.insert(2, build_test_region(2, "Heart of Shiverpeaks", vec![]));
    gw2.set_regions_on_floor(1, 1, continent1);
    gw2.set_regions_on_floor(2, 1, BTreeMap::new());

    let svc = build(gw2, wiki, cache, clock);
    let err = svc
        .list_maps_in_region(RegionQuery::Name("shiver".to_owned()))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("multiple"), "got: {msg}");
    assert!(msg.contains("Shiverpeak Mountains"), "got: {msg}");
    assert!(msg.contains("Heart of Shiverpeaks"), "got: {msg}");
}

#[tokio::test]
async fn list_maps_in_region_unknown_id_errors_with_helpful_message() {
    use gw2_mcp::service::RegionQuery;
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    let mut continent1 = BTreeMap::new();
    continent1.insert(4, build_test_region(4, "Maguuma Jungle", vec![]));
    gw2.set_regions_on_floor(1, 1, continent1);
    gw2.set_regions_on_floor(2, 1, BTreeMap::new());

    let svc = build(gw2, wiki, cache, clock);
    let err = svc
        .list_maps_in_region(RegionQuery::Id(9999))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("9999"), "must mention bogus id: {msg}");
}

#[tokio::test]
async fn list_maps_in_region_reports_upstream_unavailable_not_not_found() {
    // Regression: when /v2/continents fails for every continent, the
    // old code synthesised a NotFound { name: "<index unavailable>",
    // available: "GW2 continents API unreachable" } which read like a
    // typo'd region name. New behaviour: distinct UpstreamUnavailable
    // variant with a clear message.
    use gw2_mcp::service::RegionQuery;
    let clock = TestClock::new();
    let cache = TestCache::new(clock.clone());
    let gw2 = FakeGw2Api::new();
    let wiki = FakeWiki::new();
    // Mark every continent as failing.
    gw2.set_regions_on_floor_error(1, 1);
    gw2.set_regions_on_floor_error(2, 1);

    let svc = build(gw2, wiki, cache, clock);
    for query in [RegionQuery::Name("Castora".to_owned()), RegionQuery::Id(99)] {
        let err = svc.list_maps_in_region(query).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unreachable"),
            "must surface the upstream outage, not pretend it's a name lookup miss: {msg}"
        );
        assert!(
            !msg.contains("<index unavailable>"),
            "must not leak the old sentinel name into the message: {msg}"
        );
        assert!(
            !msg.contains("Castora") && !msg.contains("99"),
            "upstream-failure error must NOT pretend it was a name/id miss: {msg}"
        );
    }
}
