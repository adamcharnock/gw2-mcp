//! Integration tests for Tier 6B navigation:
//! Mumble Link snapshot → bearing math → service tools.
//!
//! Every test wires `FakeMumbleLink` + `FakeMapData` through the real
//! `Service`, so the orchestration paths are exercised end-to-end without
//! touching the real game or HTTP.

mod common;

use std::sync::Arc;

use gw2_mcp::adapters::ChatrDecoder;
use gw2_mcp::domain::bearing::Bearing16;
use gw2_mcp::ports::{Cache, Clock, Gw2Api, MapData, MumbleError, MumbleLink, Wiki};
use gw2_mcp::service::{LocationRef, NearbyFilter, Service, ServiceError};

use crate::common::{
    FakeGw2Api, FakeMapData, FakeMumbleLink, FakeWiki, TestCache, TestClock,
    build_service_with_navigation, make_map_info, make_mumble_snapshot, make_poi,
};

/// Standard wiring for the navigation tests: real catalogs registry (empty),
/// real chatr decoder, fake mumble + maps. Tests can re-build the service
/// per case so seeded state stays isolated.
fn build(mumble: Arc<dyn MumbleLink>, maps: Arc<FakeMapData>) -> Service {
    let clock: Arc<dyn Clock> = TestClock::new();
    let cache: Arc<dyn Cache> = TestCache::new(clock.clone());
    let gw2: Arc<dyn Gw2Api> = FakeGw2Api::new();
    let wiki: Arc<dyn Wiki> = FakeWiki::new();
    let maps_port: Arc<dyn MapData> = maps;
    build_service_with_navigation(gw2, wiki, cache, clock, mumble, maps_port)
}

// ---------------------------------------------------------------------------
// get_my_location
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_my_location_returns_mumble_state_with_resolved_map_name() {
    // Avatar facing north (Mumble Y = 1, Z up).
    let snap = make_mumble_snapshot(15, 12345.0, 9876.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    let svc = build(mumble, maps.clone());

    let loc = svc.get_my_location().await.expect("location");
    assert_eq!(loc.character_name, "Hero");
    assert_eq!(loc.profession_name.as_deref(), Some("Guardian"));
    assert_eq!(loc.map_id, 15);
    assert_eq!(
        loc.map.as_ref().map(|m| m.name.as_str()),
        Some("Queensdale")
    );
    assert_eq!(loc.position, (12345.0, 9876.0));
    // facing avatar_front (0,1,0) is map-NORTH (we flip Y in facing_bearing).
    assert_eq!(loc.facing_bearing, Bearing16::N);
}

#[tokio::test]
async fn get_my_location_returns_position_even_when_map_lookup_fails() {
    let snap = make_mumble_snapshot(99, 100.0, 200.0, [1.0, 0.0, 0.0], "B", 2);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    // No map added => FakeMapData returns NotFound. Service should still
    // return a snapshot with map=None.
    let svc = build(mumble, maps);

    let loc = svc.get_my_location().await.expect("location");
    assert_eq!(loc.map_id, 99);
    assert!(loc.map.is_none(), "missing map metadata should yield None");
    assert_eq!(loc.position, (100.0, 200.0));
    // avatar_front (1,0,0) is east.
    assert_eq!(loc.facing_bearing, Bearing16::E);
}

#[tokio::test]
async fn get_my_location_propagates_mumble_unsupported() {
    let mumble = FakeMumbleLink::with_error(MumbleError::Unsupported("--no-mumble-link".into()));
    let maps = Arc::new(FakeMapData::new());
    let svc = build(mumble, maps);

    let err = svc.get_my_location().await.expect_err("must error");
    assert!(matches!(err, ServiceError::Mumble(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// get_directions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_directions_named_poi_lookup_then_bearing() {
    let snap = make_mumble_snapshot(15, 100.0, 100.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        vec![
            make_poi(118, "Shaemoor Waypoint", "waypoint", (100.0, 50.0)),
            make_poi(119, "Beetletun Waypoint", "waypoint", (100.0, 150.0)),
        ],
    );
    let svc = build(mumble, maps);

    // From "here" (100,100) to Shaemoor Waypoint (100,50) — north
    // (since Y is inverted).
    let res = svc
        .get_directions(
            LocationRef::Here,
            LocationRef::NamedPoi {
                map_id: 15,
                name: "Shaemoor Waypoint".to_owned(),
            },
        )
        .await
        .expect("directions");
    assert_eq!(res.bearing, Bearing16::N);
    assert!((res.distance_units - 50.0).abs() < 1e-6);
    assert_eq!(res.to.label, "Shaemoor Waypoint");
}

#[tokio::test]
async fn get_directions_unknown_poi_returns_no_such_poi() {
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(15, vec![]); // empty POI list
    let svc = build(mumble, maps);

    let err = svc
        .get_directions(
            LocationRef::Here,
            LocationRef::NamedPoi {
                map_id: 15,
                name: "Nonexistent".to_owned(),
            },
        )
        .await
        .expect_err("should error");
    match err {
        ServiceError::NoSuchPoi { map_id, name } => {
            assert_eq!(map_id, 15);
            assert_eq!(name, "Nonexistent");
        }
        other => panic!("expected NoSuchPoi, got {other:?}"),
    }
}

#[tokio::test]
async fn get_directions_with_literal_coords_skips_poi_lookup() {
    let snap = make_mumble_snapshot(1, 0.0, 0.0, [0.0, 1.0, 0.0], "X", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    let svc = build(mumble, maps.clone());

    let res = svc
        .get_directions(
            LocationRef::Coords {
                coord: (0.0, 0.0),
                map_id: None,
            },
            LocationRef::Coords {
                coord: (100.0, 0.0),
                map_id: None,
            },
        )
        .await
        .expect("directions");
    assert_eq!(res.bearing, Bearing16::E);
    assert_eq!(maps.poi_calls(), 0, "literal coords must not hit POI list");
}

// ---------------------------------------------------------------------------
// find_nearby
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_nearby_sorts_by_distance_and_respects_filter() {
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        vec![
            make_poi(1, "Near Waypoint", "waypoint", (10.0, 0.0)),
            make_poi(2, "Mid Vista", "vista", (50.0, 0.0)),
            make_poi(3, "Far Waypoint", "waypoint", (100.0, 0.0)),
            make_poi(4, "Far Landmark", "landmark", (75.0, 0.0)),
        ],
    );
    let svc = build(mumble, maps);

    let waypoints = svc
        .find_nearby(NearbyFilter::Waypoint, LocationRef::Here, 5)
        .await
        .expect("nearby");
    let names: Vec<_> = waypoints.iter().map(|w| w.poi.name.as_str()).collect();
    assert_eq!(names, vec!["Near Waypoint", "Far Waypoint"]);
    assert!(waypoints[0].distance_units < waypoints[1].distance_units);

    let any = svc
        .find_nearby(NearbyFilter::Any, LocationRef::Here, 5)
        .await
        .expect("nearby any");
    assert_eq!(any.len(), 4);
    let dists: Vec<f64> = any.iter().map(|w| w.distance_units).collect();
    assert!(
        dists.windows(2).all(|w| w[0] <= w[1]),
        "nearby must be sorted"
    );
}

#[tokio::test]
async fn find_nearby_clamps_limit() {
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        (0u32..50)
            .map(|i| {
                make_poi(
                    u64::from(i),
                    &format!("Waypoint {i}"),
                    "waypoint",
                    (f64::from(i), 0.0),
                )
            })
            .collect(),
    );
    let svc = build(mumble, maps);

    let three = svc
        .find_nearby(NearbyFilter::Any, LocationRef::Here, 3)
        .await
        .expect("nearby");
    assert_eq!(three.len(), 3);
}

// ---------------------------------------------------------------------------
// describe_facing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn describe_facing_picks_nearest_landmark_in_quadrant() {
    // Facing east → avatar_front = (1, 0, 0).
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [1.0, 0.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        vec![
            make_poi(1, "East Landmark", "landmark", (100.0, 0.0)),
            // Not visible: behind the player.
            make_poi(2, "West Landmark", "landmark", (-100.0, 0.0)),
        ],
    );
    let svc = build(mumble, maps);

    let desc = svc.describe_facing().await.expect("facing");
    assert_eq!(desc.facing_bearing, Bearing16::E);
    assert_eq!(desc.facing_long_name, "east");
    assert_eq!(
        desc.nearest_landmark.as_ref().map(|p| p.name.as_str()),
        Some("East Landmark")
    );
    assert!(desc.nearest_landmark_distance_meters.is_some());
}

#[tokio::test]
async fn describe_facing_returns_none_when_no_landmark_in_facing_direction() {
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        // Only a landmark behind the player (south).
        vec![make_poi(1, "Far South", "landmark", (0.0, 5000.0))],
    );
    let svc = build(mumble, maps);

    let desc = svc.describe_facing().await.expect("facing");
    assert_eq!(desc.facing_bearing, Bearing16::N);
    assert!(
        desc.nearest_landmark.is_none(),
        "south landmark must not be picked when facing north"
    );
}

// ---------------------------------------------------------------------------
// MCP tool dispatch — wires the full schema-validated layer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_dispatch_get_my_location_returns_structured_json() {
    use gw2_mcp::adapters::McpServer;
    let snap = make_mumble_snapshot(15, 1.0, 2.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    let svc = build(mumble, maps);
    let server = McpServer::new(svc);

    let v = server
        .dispatch_tool("get_my_location", serde_json::json!({}))
        .await
        .expect("ok");
    assert_eq!(v["character_name"], "Hero");
    assert_eq!(v["map_id"], 15);
}

#[tokio::test]
async fn mcp_dispatch_get_directions_with_named_poi_chain() {
    use gw2_mcp::adapters::McpServer;
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(
        15,
        vec![make_poi(118, "Shaemoor Waypoint", "waypoint", (100.0, 0.0))],
    );
    let svc = build(mumble, maps);
    let server = McpServer::new(svc);

    let v = server
        .dispatch_tool(
            "get_directions",
            serde_json::json!({
                "from": { "here": true },
                "to": { "poi_name": "Shaemoor Waypoint", "map_id": 15 }
            }),
        )
        .await
        .expect("ok");
    // Bearing label "E" because the waypoint is east of (0,0).
    assert_eq!(v["bearing_label"], "E");
    assert_eq!(v["to"]["label"], "Shaemoor Waypoint");
}

#[tokio::test]
async fn mcp_dispatch_find_nearby_defaults_around_to_here() {
    use gw2_mcp::adapters::McpServer;
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    maps.add_pois(15, vec![make_poi(1, "A", "waypoint", (10.0, 0.0))]);
    let svc = build(mumble, maps);
    let server = McpServer::new(svc);

    let v = server
        .dispatch_tool("find_nearby", serde_json::json!({ "filter": "waypoint" }))
        .await
        .expect("ok");
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["poi"]["name"], "A");
}

#[tokio::test]
async fn mcp_dispatch_find_nearby_rejects_unknown_filter() {
    use gw2_mcp::adapters::McpServer;
    let snap = make_mumble_snapshot(15, 0.0, 0.0, [0.0, 1.0, 0.0], "Hero", 1);
    let mumble = FakeMumbleLink::with_snapshot(snap);
    let maps = Arc::new(FakeMapData::new());
    maps.add_map(make_map_info(15, "Queensdale"));
    let svc = build(mumble, maps);
    let server = McpServer::new(svc);

    let err = server
        .dispatch_tool("find_nearby", serde_json::json!({ "filter": "bogus" }))
        .await
        .expect_err("should reject");
    assert!(err.contains("filter"), "error must mention filter: {err}");
}

// Suppress the unused-import for `ChatrDecoder` (kept symmetric with
// other test binaries that re-export the build_service builder).
#[allow(dead_code)]
fn _force_use_decoder() {
    let _ = ChatrDecoder;
}
