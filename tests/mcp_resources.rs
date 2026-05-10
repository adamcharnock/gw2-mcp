//! Pin the resource surface area exposed via MCP `resources/list`,
//! `resources/templates/list`, and `resources/read`.
//!
//! Reads go through the same router the protocol handler uses, so these
//! tests cover both the public test seam and the wire behaviour.

mod common;

use std::sync::Arc;

use gw2_mcp::adapters::{ChatrDecoder, McpServer, StubMumbleLink};
use gw2_mcp::domain::{
    Currency, CurrencyId, Item, ItemId, Skill, SkillId, Specialization, SpecializationId, Trait,
    TraitId,
};
use gw2_mcp::ports::MumbleLink;
use gw2_mcp::ports::{BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, MapData, Wiki};
use gw2_mcp::service::Service;
use serde_json::Value;

use crate::common::{
    FakeCatalog, FakeGw2Api, FakeMapData, FakeWiki, TestCache, TestClock, build_detail,
    build_summary,
};

fn build_server_with_fakes(gw2: Arc<FakeGw2Api>, catalogs: Arc<CatalogRegistry>) -> McpServer {
    let clock: Arc<dyn Clock> = TestClock::new();
    let cache: Arc<dyn Cache> = TestCache::new(clock.clone());
    let wiki: Arc<dyn Wiki> = FakeWiki::new();
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let gw2_port: Arc<dyn Gw2Api> = gw2;
    let mumble: Arc<dyn MumbleLink> = Arc::new(StubMumbleLink::new("test default: no mumble"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    McpServer::new(Service::new(
        gw2_port, wiki, cache, clock, decoder, catalogs, mumble, maps,
    ))
}

fn empty_server() -> McpServer {
    let gw2 = FakeGw2Api::new();
    let catalogs = Arc::new(CatalogRegistry::new());
    build_server_with_fakes(gw2, catalogs)
}

// ---------------------------------------------------------------------------
// list_resources / list_resource_templates
// ---------------------------------------------------------------------------

#[test]
fn list_resources_publishes_currencies_and_three_catalog_listings() {
    let resources = McpServer::list_resources_for_test();
    let uris: Vec<String> = resources.iter().map(|r| r.uri.clone()).collect();
    for expected in [
        "gw2://currencies",
        "gw2://builds/discretize",
        "gw2://builds/metabattle",
        "gw2://builds/snowcrows",
    ] {
        assert!(
            uris.iter().any(|u| u == expected),
            "missing resource {expected}, got {uris:?}"
        );
    }
    for r in &resources {
        assert_eq!(
            r.mime_type.as_deref(),
            Some("application/json"),
            "{} should advertise application/json",
            r.uri
        );
        assert!(
            r.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{} needs a description",
            r.uri
        );
    }
}

#[test]
fn list_resource_templates_publishes_five_per_id_templates() {
    let templates = McpServer::list_resource_templates_for_test();
    let uri_templates: Vec<String> = templates.iter().map(|t| t.uri_template.clone()).collect();
    for expected in [
        "gw2://skills/{id}",
        "gw2://traits/{id}",
        "gw2://specializations/{id}",
        "gw2://items/{id}",
        "gw2://builds/{source}/{slug}",
    ] {
        assert!(
            uri_templates.iter().any(|t| t == expected),
            "missing template {expected}, got {uri_templates:?}"
        );
    }
    for t in &templates {
        assert_eq!(t.mime_type.as_deref(), Some("application/json"));
        assert!(t.description.as_ref().is_some_and(|d| !d.is_empty()));
    }
}

// ---------------------------------------------------------------------------
// per-template reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_skills_template_returns_full_skill_record() {
    let gw2 = FakeGw2Api::new();
    gw2.add_skill(Skill {
        id: SkillId::new(9137).unwrap(),
        name: "Wave of Wrath".to_owned(),
        extra: serde_json::from_str(
            r#"{"description":"Send out a wave","facts":[{"type":"Damage"}]}"#,
        )
        .unwrap(),
    });
    let mcp = build_server_with_fakes(gw2, Arc::new(CatalogRegistry::new()));
    let body = mcp
        .read_resource_for_test("gw2://skills/9137")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["9137"]["name"], "Wave of Wrath");
    // Resource reads are non-summarised: facts[] survive.
    assert_eq!(parsed["9137"]["facts"][0]["type"], "Damage");
}

#[tokio::test]
async fn read_traits_template_returns_full_trait_record() {
    let gw2 = FakeGw2Api::new();
    gw2.add_trait(Trait {
        id: TraitId::new(214).unwrap(),
        name: "Big Game Hunter".to_owned(),
        extra: serde_json::from_str(r#"{"description":"More damage","facts":[]}"#).unwrap(),
    });
    let mcp = build_server_with_fakes(gw2, Arc::new(CatalogRegistry::new()));
    let body = mcp
        .read_resource_for_test("gw2://traits/214")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["214"]["name"], "Big Game Hunter");
    // facts[] is preserved (non-summary view).
    assert!(parsed["214"]["facts"].is_array());
}

#[tokio::test]
async fn read_specializations_template_returns_full_record() {
    let gw2 = FakeGw2Api::new();
    gw2.add_specialization(Specialization {
        id: SpecializationId::new(46).unwrap(),
        name: "Dragonhunter".to_owned(),
        extra: serde_json::from_str(r#"{"profession":"Guardian","elite":true}"#).unwrap(),
    });
    let mcp = build_server_with_fakes(gw2, Arc::new(CatalogRegistry::new()));
    let body = mcp
        .read_resource_for_test("gw2://specializations/46")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["46"]["name"], "Dragonhunter");
    assert_eq!(parsed["46"]["profession"], "Guardian");
}

#[tokio::test]
async fn read_items_template_returns_full_item_record() {
    let gw2 = FakeGw2Api::new();
    gw2.add_item(Item {
        id: ItemId::new(80384).unwrap(),
        name: "The Predator".to_owned(),
        extra: serde_json::from_str(r#"{"rarity":"Legendary","type":"Weapon"}"#).unwrap(),
    });
    let mcp = build_server_with_fakes(gw2, Arc::new(CatalogRegistry::new()));
    let body = mcp
        .read_resource_for_test("gw2://items/80384")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["80384"]["name"], "The Predator");
    assert_eq!(parsed["80384"]["rarity"], "Legendary");
}

#[tokio::test]
async fn read_currencies_concrete_resource_returns_full_list() {
    let gw2 = FakeGw2Api::new();
    gw2.add_currency(Currency {
        id: CurrencyId::new(1).unwrap(),
        name: "Coin".to_owned(),
        description: "Coins.".to_owned(),
        icon: "https://x.png".to_owned(),
        order: 1,
    });
    gw2.add_currency(Currency {
        id: CurrencyId::new(2).unwrap(),
        name: "Karma".to_owned(),
        description: "Karma.".to_owned(),
        icon: "https://y.png".to_owned(),
        order: 2,
    });
    let mcp = build_server_with_fakes(gw2, Arc::new(CatalogRegistry::new()));
    let body = mcp
        .read_resource_for_test("gw2://currencies")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["1"]["name"], "Coin");
    assert_eq!(parsed["2"]["name"], "Karma");
}

// ---------------------------------------------------------------------------
// build template — including the slug-with-slashes case
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_builds_template_with_two_segment_slug_routes_to_discretize() {
    // discretize slug shape: <profession>/<build>
    let cat = FakeCatalog::new("discretize");
    cat.set_fetch(build_detail("guardian/power-dragonhunter", "Guardian"));
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let mcp = build_server_with_fakes(FakeGw2Api::new(), registry);

    let body = mcp
        .read_resource_for_test("gw2://builds/discretize/guardian/power-dragonhunter")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["summary"]["slug"], "guardian/power-dragonhunter");
    assert_eq!(parsed["summary"]["profession"], "Guardian");
    assert_eq!(cat.fetch_calls(), 1);
}

#[tokio::test]
async fn read_builds_template_with_three_segment_slug_routes_to_snowcrows() {
    // snowcrows slug shape: <category>/<profession>/<build>
    let cat = FakeCatalog::new("snowcrows");
    cat.set_fetch(build_detail(
        "raids/guardian/quickness-dragonhunter",
        "Guardian",
    ));
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let mcp = build_server_with_fakes(FakeGw2Api::new(), registry);

    let body = mcp
        .read_resource_for_test("gw2://builds/snowcrows/raids/guardian/quickness-dragonhunter")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    // Crucially, the slug in the URI is taken verbatim — not re-split into source segments.
    assert_eq!(
        parsed["summary"]["slug"], "raids/guardian/quickness-dragonhunter",
        "the URI suffix after `gw2://builds/<source>/` must be passed as the literal slug"
    );
    assert_eq!(cat.fetch_calls(), 1);
}

#[tokio::test]
async fn read_builds_listing_concrete_resource_calls_list() {
    let cat = FakeCatalog::new("metabattle");
    cat.set_list(vec![
        build_summary("Power_Reaper", "Necromancer"),
        build_summary("Healing_Druid", "Ranger"),
    ]);
    let registry = Arc::new(CatalogRegistry::new().with(cat.clone()));
    let mcp = build_server_with_fakes(FakeGw2Api::new(), registry);

    let body = mcp
        .read_resource_for_test("gw2://builds/metabattle")
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&body).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["slug"], "Power_Reaper");
    assert_eq!(cat.list_calls(), 1);
}

// ---------------------------------------------------------------------------
// error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_unknown_uri_errors() {
    let mcp = empty_server();
    let err = mcp
        .read_resource_for_test("gw2://does-not-exist")
        .await
        .unwrap_err();
    assert!(err.contains("not_found") || err.contains("not found"));
}

#[tokio::test]
async fn read_skill_uri_with_bad_id_segment_errors() {
    let mcp = empty_server();
    let err = mcp
        .read_resource_for_test("gw2://skills/not-a-number")
        .await
        .unwrap_err();
    assert!(err.contains("invalid id"), "got: {err}");
}

#[tokio::test]
async fn read_skill_uri_with_zero_id_errors() {
    let mcp = empty_server();
    let err = mcp
        .read_resource_for_test("gw2://skills/0")
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("skill id") || err.to_lowercase().contains("invalid id"),
        "should explain why id 0 is bad: got {err}"
    );
}

#[tokio::test]
async fn read_builds_uri_with_unknown_source_errors() {
    let mcp = empty_server();
    let err = mcp
        .read_resource_for_test("gw2://builds/no-such-source/foo/bar")
        .await
        .unwrap_err();
    assert!(
        err.to_lowercase().contains("no such build source"),
        "got: {err}"
    );
}
