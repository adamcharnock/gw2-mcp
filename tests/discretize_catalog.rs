//! Integration tests for the Discretize catalog adapter.
//!
//! Fixtures under `tests/fixtures/` are real responses captured from
//! `api.github.com` and `raw.githubusercontent.com` — see the file
//! header comment in `tests/fixtures/README.md` for capture commands.

use gw2_mcp::adapters::DiscretizeCatalog;
use gw2_mcp::ports::{BuildCatalog, CatalogError, CatalogFilter};
use pretty_assertions::assert_eq;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TREE_FIXTURE: &str = include_str!("fixtures/discretize_tree.json");
const POWER_DH_MD: &str = include_str!("fixtures/discretize_power_dragonhunter.md");

#[tokio::test]
async fn list_returns_every_build_entry_in_the_tree() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TREE_FIXTURE))
        .mount(&server)
        .await;

    let catalog =
        DiscretizeCatalog::with_bases(server.uri(), "http://unused.invalid".to_owned()).unwrap();
    let summaries = catalog.list(&CatalogFilter::default()).await.unwrap();

    // Spot-check: there are at least 30 builds in the real tree, and every
    // summary has a slug shaped like "<profession>/<build>".
    assert!(
        summaries.len() >= 30,
        "expected >= 30 builds, got {}",
        summaries.len()
    );
    for s in &summaries {
        assert!(s.slug.contains('/'), "malformed slug: {}", s.slug);
        assert_eq!(s.gamemode, "fractals");
        assert_eq!(s.source, "discretize");
        assert!(s.source_url.starts_with("https://discretize.eu/"));
    }
    // Spot-check a known build is present.
    assert!(
        summaries
            .iter()
            .any(|s| s.slug == "guardian/power-dragonhunter"),
        "guardian/power-dragonhunter must be in the listing"
    );
}

#[tokio::test]
async fn list_filters_by_profession() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TREE_FIXTURE))
        .mount(&server)
        .await;

    let catalog =
        DiscretizeCatalog::with_bases(server.uri(), "http://unused.invalid".to_owned()).unwrap();
    let only_guardian = catalog
        .list(&CatalogFilter {
            profession: Some("guardian".to_owned()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(!only_guardian.is_empty());
    for s in &only_guardian {
        assert_eq!(s.profession, "Guardian");
    }
}

#[tokio::test]
async fn list_clears_when_gamemode_is_not_fractals() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/discretize/discretize-guides/git/trees/master"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TREE_FIXTURE))
        .mount(&server)
        .await;

    let catalog =
        DiscretizeCatalog::with_bases(server.uri(), "http://unused.invalid".to_owned()).unwrap();
    let raids = catalog
        .list(&CatalogFilter {
            gamemode: Some("raids".to_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        raids.is_empty(),
        "discretize is fractals-only; raids filter must produce empty"
    );
}

#[tokio::test]
async fn fetch_parses_yaml_frontmatter_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/discretize/discretize-guides/master/builds/guardian/power-dragonhunter/index.md",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(POWER_DH_MD))
        .mount(&server)
        .await;

    let catalog =
        DiscretizeCatalog::with_bases("http://unused.invalid".to_owned(), server.uri()).unwrap();
    let detail = catalog.fetch("guardian/power-dragonhunter").await.unwrap();

    // Front-matter assertions — these come from the real Discretize file.
    assert_eq!(detail.summary.title, "Power Dragonhunter");
    assert_eq!(detail.summary.profession, "Guardian");
    assert_eq!(detail.summary.elite_spec.as_deref(), Some("Dragonhunter"));
    assert_eq!(detail.summary.role, "Power Damage");
    assert_eq!(detail.summary.rating.as_deref(), Some("Meta"));
    assert!(detail.chat_code.as_deref().unwrap().starts_with("[&"));

    // Body must contain at least one Character block (where the real gear lives).
    assert!(
        detail.description.contains("<Character"),
        "body must contain Character blocks; got: {}",
        &detail.description[..200.min(detail.description.len())]
    );
}

#[tokio::test]
async fn fetch_returns_not_found_for_missing_build() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/discretize/discretize-guides/master/builds/guardian/does-not-exist/index.md",
        ))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let catalog =
        DiscretizeCatalog::with_bases("http://unused.invalid".to_owned(), server.uri()).unwrap();
    let err = catalog.fetch("guardian/does-not-exist").await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound { .. }));
}

#[tokio::test]
async fn fetch_rejects_malformed_slug() {
    let server = MockServer::start().await;
    let catalog =
        DiscretizeCatalog::with_bases("http://unused.invalid".to_owned(), server.uri()).unwrap();
    let err = catalog.fetch("just-one-segment").await.unwrap_err();
    assert!(matches!(err, CatalogError::Parse { .. }));
}
