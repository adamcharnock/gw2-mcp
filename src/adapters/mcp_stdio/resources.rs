//! Resource registry: the `gw2://...` URIs the MCP server exposes.
//!
//! Two surfaces:
//!
//! - **Concrete resources** (`concrete_resources`) — fixed URIs that
//!   resolve straight to a service call: `gw2://currencies`, plus the
//!   per-source build listings.
//! - **Resource templates** (`resource_templates`) — RFC-6570 templates
//!   like `gw2://skills/{id}` that clients expand client-side before
//!   calling `resources/read`. The mod-level `parse_*` helpers turn a
//!   filled-in URI back into the typed argument the read dispatcher
//!   expects.
//!
//! Visibility: the URI constants and parsers are `pub(super)` so the
//! `ServerHandler::read_resource` impl in `mod.rs` can route by them.

use rmcp::model::{Annotated, RawResource, RawResourceTemplate};

pub(super) const CURRENCIES_RESOURCE_URI: &str = "gw2://currencies";
pub(super) const BUILDS_DISCRETIZE_URI: &str = "gw2://builds/discretize";
pub(super) const BUILDS_METABATTLE_URI: &str = "gw2://builds/metabattle";
pub(super) const BUILDS_SNOWCROWS_URI: &str = "gw2://builds/snowcrows";

pub(super) const SKILLS_PREFIX: &str = "gw2://skills/";
pub(super) const TRAITS_PREFIX: &str = "gw2://traits/";
pub(super) const SPECS_PREFIX: &str = "gw2://specializations/";
pub(super) const ITEMS_PREFIX: &str = "gw2://items/";
pub(super) const BUILDS_PREFIX: &str = "gw2://builds/";

pub(super) const RESOURCE_JSON_MIME: &str = "application/json";

/// Parse `<prefix><id>` into a typed id via `ctor`, returning `Some(Err)`
/// when the URI matches but the id is malformed and `None` when the URI
/// doesn't match this template at all (caller falls through to the next
/// route).
pub(super) fn parse_typed_id_uri<F, Id>(
    uri: &str,
    prefix: &str,
    ctor: F,
) -> Option<Result<Id, String>>
where
    F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
{
    let tail = uri.strip_prefix(prefix)?;
    if tail.is_empty() || tail.contains('/') {
        return Some(Err(format!(
            "invalid id segment in URI: {uri} (expected `{prefix}<positive integer>`)"
        )));
    }
    let parsed = tail
        .parse::<i64>()
        .map_err(|_| format!("invalid id in URI: {uri} (id segment must be a positive integer)"))
        .and_then(|n| ctor(n).map_err(|e| format!("invalid id in URI {uri}: {e}")));
    Some(parsed)
}

/// Strip `gw2://builds/` and split off the source segment. Returns
/// `Some((source, slug))` where `slug` may itself contain `/`. Returns
/// `None` if the URI is not a build-template URI (caller falls through
/// to the next route).
pub(super) fn parse_build_uri(uri: &str) -> Option<Result<(&str, &str), String>> {
    let tail = uri.strip_prefix(BUILDS_PREFIX)?;
    let Some((source, slug)) = tail.split_once('/') else {
        // No `/` in the tail: this is a concrete listing URI like
        // `gw2://builds/discretize`, not a per-build template URI. Let the
        // caller's listing dispatch handle it.
        return None;
    };
    if source.is_empty() || slug.is_empty() {
        return Some(Err(format!(
            "invalid builds URI: {uri} (expected `gw2://builds/<source>/<slug>`)"
        )));
    }
    Some(Ok((source, slug)))
}

/// The non-template resources that show up in `resources/list`. These have
/// fixed URIs that resolve straight to a service call — no parameters.
pub(super) fn concrete_resources() -> Vec<rmcp::model::Resource> {
    let mut currencies = RawResource::new(CURRENCIES_RESOURCE_URI, "Guild Wars 2 Currencies");
    currencies.description =
        Some("Complete list of Guild Wars 2 currencies (id + name).".to_owned());
    currencies.mime_type = Some(RESOURCE_JSON_MIME.to_owned());

    let mut discretize = RawResource::new(BUILDS_DISCRETIZE_URI, "Discretize Builds");
    discretize.description = Some(
        "Listing of curated fractal builds from Discretize (https://discretize.eu). Returns the \
         same shape as `list_catalog_builds` with no filter."
            .to_owned(),
    );
    discretize.mime_type = Some(RESOURCE_JSON_MIME.to_owned());

    let mut metabattle = RawResource::new(BUILDS_METABATTLE_URI, "MetaBattle Builds");
    metabattle.description = Some(
        "Listing of curated builds from MetaBattle (https://metabattle.com), covering all \
         gamemodes."
            .to_owned(),
    );
    metabattle.mime_type = Some(RESOURCE_JSON_MIME.to_owned());

    let mut snowcrows = RawResource::new(BUILDS_SNOWCROWS_URI, "Snow Crows Builds");
    snowcrows.description = Some(
        "Listing of curated builds from Snow Crows (https://snowcrows.com). Returns raids only by default (cold-start cost is one HTTP request); pass `gamemode=open_world|pvp|wvw` to list_catalog_builds for those categories."
            .to_owned(),
    );
    snowcrows.mime_type = Some(RESOURCE_JSON_MIME.to_owned());

    vec![
        Annotated::new(currencies, None),
        Annotated::new(discretize, None),
        Annotated::new(metabattle, None),
        Annotated::new(snowcrows, None),
    ]
}

/// RFC-6570 URI templates surfaced in `resources/templates/list`. Clients
/// fill in the `{...}` segments before calling `resources/read`.
pub(super) fn resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
    fn template(
        uri_template: &str,
        name: &str,
        description: &str,
    ) -> rmcp::model::ResourceTemplate {
        let raw = RawResourceTemplate {
            uri_template: uri_template.to_owned(),
            name: name.to_owned(),
            title: None,
            description: Some(description.to_owned()),
            mime_type: Some(RESOURCE_JSON_MIME.to_owned()),
            icons: None,
        };
        Annotated::new(raw, None)
    }

    vec![
        template(
            "gw2://skills/{id}",
            "Skill",
            "Single GW2 skill resolved by numeric API id. Returns the full /v2/skills entry \
             (including facts[]).",
        ),
        template(
            "gw2://traits/{id}",
            "Trait",
            "Single GW2 trait resolved by numeric API id. Returns the full /v2/traits entry.",
        ),
        template(
            "gw2://specializations/{id}",
            "Specialization",
            "Single GW2 specialization (core or elite) resolved by numeric API id. Returns the \
             full /v2/specializations entry.",
        ),
        template(
            "gw2://items/{id}",
            "Item",
            "Single GW2 item (equipment, consumable, etc.) resolved by numeric API id. Returns \
             the full /v2/items entry.",
        ),
        template(
            "gw2://builds/{source}/{slug}",
            "Curated Build",
            "Single curated build from a registered source. `source` is one of `discretize`, \
             `metabattle`, `snowcrows`. `slug` matches `list_catalog_builds`'s slug field — \
             note that some sources use multi-segment slugs (discretize: \
             `<profession>/<build>`; snowcrows: `<category>/<profession>/<build>`); the slug is \
             taken literally from the URI suffix after `gw2://builds/<source>/`.",
        ),
    ]
}
