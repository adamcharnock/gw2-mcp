//! Integration tests for the Tier-6C search MCP tools.
//!
//! Wires a `Service` with the in-process `SQLite` index (in-memory mode) and
//! the in-process `FakeGw2Api` so we can drive the full pipeline end-to-end
//! without any HTTP. Covers tool dispatch, the "still indexing" error path,
//! filter behaviour, and `get_index_status`.

mod common;

use std::sync::Arc;

use gw2_mcp::adapters::{ChatrDecoder, McpServer, SqliteSearchIndex, StubMumbleLink};
use gw2_mcp::domain::{Achievement, AchievementId, Item, ItemId, Skill, SkillId};
use gw2_mcp::indexing::{IndexingOpts, IndexingPipeline};
use gw2_mcp::ports::{
    BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, MapData, MumbleLink, SearchIndex, Wiki,
};
use gw2_mcp::service::Service;
use serde_json::{Value, json};

use crate::common::{FakeGw2Api, FakeMapData, FakeWiki, TestCache, TestClock};

// Each binary uses unique fixtures; this one constructs a fresh in-memory
// index per test to avoid cross-test interference.
fn build_search_server(api: Arc<FakeGw2Api>, idx: Arc<dyn SearchIndex>) -> McpServer {
    let clock: Arc<dyn Clock> = TestClock::new();
    let cache: Arc<dyn Cache> = TestCache::new(clock.clone());
    let wiki: Arc<dyn Wiki> = FakeWiki::new();
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    let gw2: Arc<dyn Gw2Api> = api;
    let mumble: Arc<dyn MumbleLink> = Arc::new(StubMumbleLink::new("search test: no mumble"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    let service = Service::new(gw2, wiki, cache, clock, decoder, catalogs, mumble, maps)
        .with_search_index(idx);
    McpServer::new(service)
}

fn skill_with(id: u32, name: &str, profession: &str) -> Skill {
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("description".into(), json!(format!("desc for {name}")));
    extra.insert("type".into(), json!("Weapon"));
    extra.insert("slot".into(), json!("Weapon_1"));
    extra.insert("professions".into(), json!([profession]));
    Skill {
        id: SkillId::new(i64::from(id)).unwrap(),
        name: name.to_owned(),
        extra,
    }
}

fn achievement_with(id: u32, name: &str, ty: &str) -> Achievement {
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("description".into(), json!(format!("description {name}")));
    extra.insert("requirement".into(), json!(format!("complete {name}")));
    extra.insert("type".into(), json!(ty));
    Achievement {
        id: AchievementId::new(i64::from(id)).unwrap(),
        name: name.to_owned(),
        extra,
    }
}

fn item_with(id: u32, name: &str, rarity: &str) -> Item {
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("type".into(), json!("Weapon"));
    extra.insert("rarity".into(), json!(rarity));
    extra.insert("level".into(), json!(80));
    Item {
        id: ItemId::new(i64::from(id)).unwrap(),
        name: name.to_owned(),
        extra,
    }
}

#[tokio::test]
async fn search_skills_dispatches_through_mcp() {
    let api = FakeGw2Api::new();
    api.add_skill(skill_with(1, "Mind Wrack", "Mesmer"));
    api.add_skill(skill_with(2, "Mind Stab", "Mesmer"));
    api.add_skill(skill_with(3, "Backstab", "Thief"));

    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    let pipeline = IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default());
    pipeline.ensure_fresh().await.unwrap();

    let mcp = build_search_server(api, idx);
    let result = mcp
        .dispatch_tool("search_skills", json!({ "query": "mind", "limit": 10 }))
        .await
        .unwrap();
    let arr = result.as_array().expect("array result");
    assert_eq!(arr.len(), 2);
    let names: Vec<&str> = arr.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Mind Wrack"));
    assert!(names.contains(&"Mind Stab"));
}

#[tokio::test]
async fn search_skills_with_profession_filter() {
    let api = FakeGw2Api::new();
    api.add_skill(skill_with(1, "Bladesong Sorrow", "Mesmer"));
    api.add_skill(skill_with(2, "Bladestorm", "Warrior"));

    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default())
        .ensure_fresh()
        .await
        .unwrap();

    let mcp = build_search_server(api, idx);
    let result = mcp
        .dispatch_tool(
            "search_skills",
            json!({ "query": "blade", "profession": "Mesmer" }),
        )
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Bladesong Sorrow");
}

#[tokio::test]
async fn search_returns_not_indexed_when_empty() {
    let api = FakeGw2Api::new();
    // Don't run any indexing — index stays empty.
    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);

    let mcp = build_search_server(api, idx);
    let err = mcp
        .dispatch_tool("search_skills", json!({ "query": "anything" }))
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("populating") || err.to_lowercase().contains("not"),
        "expected 'still populating' style error, got: {err}"
    );
}

#[tokio::test]
async fn search_disabled_returns_typed_error() {
    // Build a service WITHOUT a search index and verify search_skills
    // surfaces the SearchDisabled message rather than panicking.
    let api: Arc<dyn Gw2Api> = FakeGw2Api::new();
    let clock: Arc<dyn Clock> = TestClock::new();
    let cache: Arc<dyn Cache> = TestCache::new(clock.clone());
    let wiki: Arc<dyn Wiki> = FakeWiki::new();
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    let mumble: Arc<dyn MumbleLink> = Arc::new(StubMumbleLink::new("search test: no mumble"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    let service = Service::new(api, wiki, cache, clock, decoder, catalogs, mumble, maps);
    let mcp = McpServer::new(service);

    let err = mcp
        .dispatch_tool("search_skills", json!({ "query": "anything" }))
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("disabled"),
        "expected SearchDisabled, got: {err}"
    );
}

#[tokio::test]
async fn search_short_query_rejected() {
    let api = FakeGw2Api::new();
    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    let mcp = build_search_server(api, idx);
    let err = mcp
        .dispatch_tool("search_skills", json!({ "query": "a" }))
        .await
        .unwrap_err();
    assert!(err.contains("at least 2"));
}

#[tokio::test]
async fn get_index_status_returns_per_kind_counts() {
    let api = FakeGw2Api::new();
    api.add_skill(skill_with(1, "Foo", "Mesmer"));
    api.add_achievement(achievement_with(1, "Bar", "Daily"));

    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default())
        .ensure_fresh()
        .await
        .unwrap();

    let mcp = build_search_server(api, idx);
    let result = mcp
        .dispatch_tool("get_index_status", json!({}))
        .await
        .unwrap();
    let kinds = result["kinds"].as_array().expect("kinds array");
    assert_eq!(kinds.len(), 5);
    let by_name: std::collections::BTreeMap<String, &Value> = kinds
        .iter()
        .filter_map(|k| k["name"].as_str().map(|s| (s.to_owned(), k)))
        .collect();
    assert_eq!(by_name["skills"]["indexed"], 1);
    assert_eq!(by_name["achievements"]["indexed"], 1);
    // Items kind exists in the row set even when nothing was indexed.
    assert_eq!(by_name["items"]["indexed"], 0);
    // Build number stamped from the fake (default 123_456).
    assert_eq!(by_name["skills"]["build_number"], 123_456);
}

#[tokio::test]
async fn search_items_only_after_with_items() {
    let api = FakeGw2Api::new();
    api.add_item(item_with(1, "Berserker's Sword", "Exotic"));

    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    // Default (without items) — items table stays empty.
    IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default())
        .ensure_fresh()
        .await
        .unwrap();
    let mcp = build_search_server(api.clone(), idx.clone());
    let err = mcp
        .dispatch_tool("search_items", json!({ "query": "sword" }))
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("populating"));

    // Now run again with items enabled.
    IndexingPipeline::new(
        api.clone(),
        idx.clone(),
        IndexingOpts {
            include_items: true,
            force_rebuild: true,
        },
    )
    .ensure_fresh()
    .await
    .unwrap();
    let result = mcp
        .dispatch_tool("search_items", json!({ "query": "sword" }))
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Berserker's Sword");
}

#[tokio::test]
async fn build_number_invalidation_triggers_reindex() {
    let api = FakeGw2Api::new();
    api.add_skill(skill_with(1, "First", "Mesmer"));
    api.set_build_number(1);

    let idx_concrete = SqliteSearchIndex::open_in_memory().unwrap();
    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
    let pipeline = IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default());
    pipeline.ensure_fresh().await.unwrap();
    assert_eq!(idx.build_number().await.unwrap(), Some(1));

    // Game patch — build bumps. Add a new skill upstream and re-run.
    api.add_skill(skill_with(2, "Second", "Mesmer"));
    api.set_build_number(2);
    let pipeline2 = IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default());
    pipeline2.ensure_fresh().await.unwrap();
    assert_eq!(idx.build_number().await.unwrap(), Some(2));

    let mcp = build_search_server(api, idx);
    let result = mcp
        .dispatch_tool("search_skills", json!({ "query": "second" }))
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Second");
}
