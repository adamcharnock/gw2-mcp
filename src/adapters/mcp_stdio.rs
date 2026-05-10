//! MCP transport adapter — exposes the [`Service`](crate::service::Service)
//! over the Model Context Protocol via stdio (the standard MCP transport).
//!
//! This is the only place that imports `rmcp`. New transports (HTTP/SSE,
//! daemon mode) would be additional adapters wrapping the same `Service`.

use std::sync::Arc;

use rmcp::model::{
    Annotated, CallToolRequestParams, CallToolResult, Content, GetPromptRequestParams,
    GetPromptResult, Implementation, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, Prompt, PromptArgument, PromptMessage, PromptMessageRole,
    RawResource, RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};

use crate::domain::{
    ApiKey, BuildChatCode, CharacterName, CurrencyId, ItemId, SearchLimit, SearchQuery, SkillId,
    SpecializationId, TraitId,
};
use crate::ports::CatalogFilter;
use crate::service::{Service, TabSelector};

const CURRENCIES_RESOURCE_URI: &str = "gw2://currencies";
const BUILDS_DISCRETIZE_URI: &str = "gw2://builds/discretize";
const BUILDS_METABATTLE_URI: &str = "gw2://builds/metabattle";
const BUILDS_SNOWCROWS_URI: &str = "gw2://builds/snowcrows";

const SKILLS_PREFIX: &str = "gw2://skills/";
const TRAITS_PREFIX: &str = "gw2://traits/";
const SPECS_PREFIX: &str = "gw2://specializations/";
const ITEMS_PREFIX: &str = "gw2://items/";
const BUILDS_PREFIX: &str = "gw2://builds/";

const RESOURCE_JSON_MIME: &str = "application/json";

/// One full URI route for a resource template, with a parser that turns a
/// matching URI back into the typed args the read handler expects.
///
/// The single-id templates (`gw2://skills/{id}` …) are easy: prefix match,
/// parse the tail as `i64`. The build template is the tricky one: the slug
/// can contain `/` (discretize uses `<profession>/<build>`, snowcrows uses
/// `<category>/<profession>/<build>`), so we strip `gw2://builds/` then
/// split off the first segment as the source and treat *everything else*
/// as the literal slug.
fn parse_typed_id_uri<F, Id>(uri: &str, prefix: &str, ctor: F) -> Option<Result<Id, String>>
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
fn parse_build_uri(uri: &str) -> Option<Result<(&str, &str), String>> {
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

#[derive(Clone)]
pub struct McpServer {
    service: Arc<Service>,
}

impl McpServer {
    pub fn new(service: Service) -> Self {
        Self {
            service: Arc::new(service),
        }
    }

    /// Run forever, serving MCP over stdio. Returns when the client closes
    /// the stream or `cancel` is signalled.
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let transport = rmcp::transport::io::stdio();
        let server = self.serve(transport).await?;
        server.waiting().await?;
        Ok(())
    }

    /// Exposes tool dispatch outside of the rmcp transport layer. Returns
    /// the structured JSON value the MCP client would receive on success,
    /// or a human-readable error message on failure.
    ///
    /// Used by integration tests to exercise tool wiring without driving
    /// the stdio protocol end-to-end. The transport layer wraps this
    /// straight into [`CallToolResult::structured`], which auto-fills the
    /// text-content mirror per the MCP back-compat clause.
    pub async fn dispatch_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let outcome = match name {
            "wiki_search" => self.handle_wiki_search(&args).await,
            "get_wallet" => self.handle_get_wallet(&args).await,
            "get_currencies" => self.handle_get_currencies(&args).await,
            "get_skills" => self.handle_get_skills(&args).await,
            "get_traits" => self.handle_get_traits(&args).await,
            "get_specializations" => self.handle_get_specializations(&args).await,
            "get_items" => self.handle_get_items(&args).await,
            "get_character_build" => self.handle_get_character_build(&args).await,
            "decode_build_code" => self.handle_decode_build_code(&args).await,
            "list_build_sources" => self.handle_list_build_sources(),
            "list_recommended_builds" => self.handle_list_recommended_builds(&args).await,
            "get_recommended_build" => self.handle_get_recommended_build(&args).await,
            other => return Err(format!("unknown tool: {other}")),
        };
        outcome.map_err(|e| e.to_string())
    }

    /// Read a resource by URI. Returns the body the MCP client would receive.
    /// Dispatches through the same router as the protocol `read_resource`,
    /// so tests pin the wire behaviour, not a parallel implementation.
    pub async fn read_resource_for_test(&self, uri: &str) -> Result<String, String> {
        self.read_resource_body(uri).await
    }

    /// Internal router shared by `read_resource_for_test` and the MCP
    /// `ServerHandler::read_resource` impl. Returns the JSON body as a
    /// string (or a flat error message — the caller decides whether to
    /// wrap it in an MCP `ErrorData::resource_not_found`).
    async fn read_resource_body(&self, uri: &str) -> Result<String, String> {
        // Concrete (non-template) resources first — these are exact matches.
        match uri {
            CURRENCIES_RESOURCE_URI => {
                let map = self
                    .service
                    .get_currencies(&[])
                    .await
                    .map_err(|e| e.to_string())?;
                return serde_json::to_string(&map).map_err(|e| e.to_string());
            }
            BUILDS_DISCRETIZE_URI => {
                return self.read_catalog_listing("discretize").await;
            }
            BUILDS_METABATTLE_URI => {
                return self.read_catalog_listing("metabattle").await;
            }
            BUILDS_SNOWCROWS_URI => {
                return self.read_catalog_listing("snowcrows").await;
            }
            _ => {}
        }

        // Single-id templates. Order does not matter — the prefixes are disjoint.
        if let Some(parsed) = parse_typed_id_uri(uri, SKILLS_PREFIX, SkillId::new) {
            let id = parsed?;
            let value = self
                .service
                .get_skills_view(&[id], false)
                .await
                .map_err(|e| e.to_string())?;
            return serde_json::to_string(&value).map_err(|e| e.to_string());
        }
        if let Some(parsed) = parse_typed_id_uri(uri, TRAITS_PREFIX, TraitId::new) {
            let id = parsed?;
            let value = self
                .service
                .get_traits_view(&[id], false)
                .await
                .map_err(|e| e.to_string())?;
            return serde_json::to_string(&value).map_err(|e| e.to_string());
        }
        if let Some(parsed) = parse_typed_id_uri(uri, SPECS_PREFIX, SpecializationId::new) {
            let id = parsed?;
            let value = self
                .service
                .get_specializations_view(&[id], false)
                .await
                .map_err(|e| e.to_string())?;
            return serde_json::to_string(&value).map_err(|e| e.to_string());
        }
        if let Some(parsed) = parse_typed_id_uri(uri, ITEMS_PREFIX, ItemId::new) {
            let id = parsed?;
            let map = self
                .service
                .get_items(&[id])
                .await
                .map_err(|e| e.to_string())?;
            return serde_json::to_string(&map).map_err(|e| e.to_string());
        }

        // Build template — last because its prefix `gw2://builds/` overlaps
        // the catalog-listing URIs above; those exact matches consume their
        // own paths first, so we only see per-build URIs here.
        if let Some(parsed) = parse_build_uri(uri) {
            let (source, slug) = parsed?;
            let detail = self
                .service
                .get_catalog_build(source, slug)
                .await
                .map_err(|e| e.to_string())?;
            return serde_json::to_string(&detail).map_err(|e| e.to_string());
        }

        Err(format!("resource_not_found: {uri}"))
    }

    async fn read_catalog_listing(&self, source: &str) -> Result<String, String> {
        let summaries = self
            .service
            .list_catalog_builds(source, CatalogFilter::default())
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&summaries).map_err(|e| e.to_string())
    }

    /// List all the prompts the server publishes via `prompts/list`. Used
    /// by integration tests to assert the prompt surface area without
    /// driving the full stdio protocol.
    #[must_use]
    pub fn list_prompts_for_test() -> Vec<Prompt> {
        build_prompts()
    }

    /// Render a prompt by name + args. Used by integration tests to pin
    /// the rendered message contents. Mirrors `prompts/get` exactly: the
    /// server handler is a thin wrapper over [`render_prompt`].
    pub fn get_prompt_for_test(
        name: &str,
        args: serde_json::Value,
    ) -> Result<GetPromptResult, String> {
        let map = match args {
            serde_json::Value::Object(m) => m,
            serde_json::Value::Null => serde_json::Map::new(),
            _ => return Err("prompt args must be a JSON object".to_owned()),
        };
        match render_prompt(name, &map) {
            Ok(r) => Ok(r),
            Err(PromptError::NotFound(n)) => Err(format!("unknown prompt: {n}")),
            Err(PromptError::MissingArg { prompt, arg }) => {
                Err(format!("prompt '{prompt}' missing required arg '{arg}'"))
            }
        }
    }

    /// List all resource templates published via `resources/templates/list`.
    /// Used by integration tests.
    #[must_use]
    pub fn list_resource_templates_for_test() -> Vec<rmcp::model::ResourceTemplate> {
        resource_templates()
    }

    /// List all concrete resources published via `resources/list`. Used by
    /// integration tests.
    #[must_use]
    pub fn list_resources_for_test() -> Vec<rmcp::model::Resource> {
        concrete_resources()
    }

    // -- tool dispatch --------------------------------------------------

    async fn handle_wiki_search(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query_str = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("query"))?;
        let query = SearchQuery::new(query_str).map_err(CallError::Domain)?;

        let limit = match args.get("limit") {
            None => SearchLimit::default(),
            Some(v) => {
                let raw = v.as_u64().ok_or(CallError::BadArg {
                    name: "limit",
                    expected: "non-negative integer",
                })?;
                let raw = u32::try_from(raw).unwrap_or(u32::MAX);
                SearchLimit::new(raw).map_err(CallError::Domain)?
            }
        };

        let response = self
            .service
            .search_wiki(&query, limit)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&response)?)
    }

    async fn handle_get_wallet(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let raw_key = args
            .get("api_key")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("api_key"))?;
        let key = ApiKey::new(raw_key).map_err(CallError::Domain)?;
        let wallet = self
            .service
            .get_wallet(&key)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&wallet)?)
    }

    async fn handle_get_currencies(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let ids = parse_id_array(args, "ids", CurrencyId::new)?;
        let map = self
            .service
            .get_currencies(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&map)?)
    }

    async fn handle_get_skills(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let ids = parse_required_id_array(args, "ids", SkillId::new)?;
        let summary = parse_summary(args);
        let value = self
            .service
            .get_skills_view(&ids, summary)
            .await
            .map_err(CallError::Service)?;
        Ok(value)
    }

    async fn handle_get_traits(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let ids = parse_required_id_array(args, "ids", TraitId::new)?;
        let summary = parse_summary(args);
        let value = self
            .service
            .get_traits_view(&ids, summary)
            .await
            .map_err(CallError::Service)?;
        Ok(value)
    }

    async fn handle_get_specializations(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let ids = parse_required_id_array(args, "ids", SpecializationId::new)?;
        let summary = parse_summary(args);
        let value = self
            .service
            .get_specializations_view(&ids, summary)
            .await
            .map_err(CallError::Service)?;
        Ok(value)
    }

    async fn handle_get_items(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let ids = parse_required_id_array(args, "ids", ItemId::new)?;
        let map = self
            .service
            .get_items(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&map)?)
    }

    async fn handle_get_character_build(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let raw_key = args
            .get("api_key")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("api_key"))?;
        let raw_name = args
            .get("character")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("character"))?;
        let key = ApiKey::new(raw_key).map_err(CallError::Domain)?;
        let name = CharacterName::new(raw_name).map_err(CallError::Domain)?;
        let tab = parse_tab_selector(args)?;
        let snap = self
            .service
            .get_character_build(&key, &name, tab)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&snap)?)
    }

    async fn handle_decode_build_code(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let raw = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("code"))?;
        let code = BuildChatCode::new(raw).map_err(CallError::Domain)?;
        let value = self
            .service
            .decode_build_code(&code)
            .await
            .map_err(CallError::Service)?;
        Ok(value)
    }

    fn handle_list_build_sources(&self) -> Result<serde_json::Value, CallError> {
        let names = self.service.list_catalogs();
        Ok(serde_json::to_value(&names)?)
    }

    async fn handle_list_recommended_builds(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("source"))?;
        let filter = CatalogFilter {
            profession: args
                .get("profession")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            gamemode: args
                .get("gamemode")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            limit: args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
        };
        let summaries = self
            .service
            .list_catalog_builds(source, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&summaries)?)
    }

    async fn handle_get_recommended_build(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("source"))?;
        let slug = args
            .get("slug")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("slug"))?;
        let detail = self
            .service
            .get_catalog_build(source, slug)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&detail)?)
    }
}

/// Parse an optional array of positive ints into typed ids.
fn parse_id_array<Id, F>(
    args: &serde_json::Value,
    name: &'static str,
    ctor: F,
) -> Result<Vec<Id>, CallError>
where
    F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
{
    match args.get(name) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_i64()
                    .ok_or(CallError::BadArg {
                        name,
                        expected: "array of positive integers",
                    })
                    .and_then(|n| ctor(n).map_err(CallError::Domain))
            })
            .collect(),
        Some(_) => Err(CallError::BadArg {
            name,
            expected: "array",
        }),
    }
}

fn parse_required_id_array<Id, F>(
    args: &serde_json::Value,
    name: &'static str,
    ctor: F,
) -> Result<Vec<Id>, CallError>
where
    F: Fn(i64) -> Result<Id, crate::domain::DomainError>,
{
    let ids = parse_id_array(args, name, ctor)?;
    if ids.is_empty() {
        return Err(CallError::BadArg {
            name,
            expected: "non-empty array of positive integers",
        });
    }
    Ok(ids)
}

/// Parse the optional `summary` flag. Defaults to `true` because summary
/// mode drops ~70% of payload bytes (facts, icon URLs) — the right default
/// for an LLM-facing API.
fn parse_summary(args: &serde_json::Value) -> bool {
    args.get("summary")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Parse the optional `tab` selector for `get_character_build`. Accepts
/// `"active"` (default), `"all"`, or a numeric string `"1"`/`"2"`/...
fn parse_tab_selector(args: &serde_json::Value) -> Result<TabSelector, CallError> {
    let Some(v) = args.get("tab") else {
        return Ok(TabSelector::default());
    };
    match v {
        serde_json::Value::Null => Ok(TabSelector::default()),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.eq_ignore_ascii_case("active") || s.is_empty() {
                Ok(TabSelector::Active)
            } else if s.eq_ignore_ascii_case("all") {
                Ok(TabSelector::All)
            } else if let Ok(n) = s.parse::<u8>() {
                Ok(TabSelector::Index(n))
            } else {
                Err(CallError::BadArg {
                    name: "tab",
                    expected: "\"active\", \"all\", or a numeric tab index (e.g. \"1\")",
                })
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64()
                && let Ok(idx) = u8::try_from(u)
            {
                Ok(TabSelector::Index(idx))
            } else {
                Err(CallError::BadArg {
                    name: "tab",
                    expected: "tab index in 0..=255",
                })
            }
        }
        _ => Err(CallError::BadArg {
            name: "tab",
            expected: "string or integer",
        }),
    }
}

#[derive(Debug)]
enum CallError {
    MissingArg(&'static str),
    BadArg {
        name: &'static str,
        expected: &'static str,
    },
    Domain(crate::domain::DomainError),
    Service(crate::service::ServiceError),
    Encode(serde_json::Error),
}

impl From<serde_json::Error> for CallError {
    fn from(value: serde_json::Error) -> Self {
        Self::Encode(value)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingArg(name) => write!(f, "missing required argument '{name}'"),
            Self::BadArg { name, expected } => {
                write!(f, "argument '{name}' has wrong type, expected {expected}")
            }
            Self::Domain(e) => write!(f, "validation error: {e}"),
            Self::Service(e) => write!(f, "{e}"),
            Self::Encode(e) => write!(f, "failed to encode response: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // Capability surface:
        // - tools/resources/prompts: all three are exercised. Resources cover
        //   currencies + per-id GW2 references + curated catalogs; prompts
        //   give clients turn-key slash commands for the common build
        //   workflows.
        // - sampling: NOT enabled. The host *is* the LLM in this topology,
        //   so the server has no use for client-driven completions.
        // - elicitation: NOT enabled. The only candidate prompt-time inputs
        //   are GW2 API keys, and the MCP elicitation spec explicitly
        //   forbids using it for sensitive material. Clients can prompt
        //   the user themselves before invoking the auth'd tools.
        ServerInfo {
            protocol_version: rmcp::model::ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
            server_info: Implementation {
                name: "gw2-mcp".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            },
            instructions: Some(
                "Guild Wars 2 MCP server.\n\
                 \n\
                 Read-only / no-auth tools:\n\
                 - wiki_search — search the GW2 wiki, returns enriched results with prose extracts.\n\
                 - get_currencies — currency metadata. Pass `ids` for specific currencies, omit for all.\n\
                 - get_skills / get_traits / get_specializations — resolve API ids (returned by\n\
                   get_character_build) into name + description + facts. `ids` is required.\n\
                 - decode_build_code — decode a `[&Dw…]` build chat code into structured JSON\n\
                   (profession, specs, traits, skill palette ids, pets/legends).\n\
                 - list_build_sources / list_recommended_builds / get_recommended_build — browse\n\
                   curated builds from Discretize (fractals), MetaBattle (all gamemodes), or Snow\n\
                   Crows (raids/strikes; on-demand only — pass slug `<category>/<profession>/<build>`).\n\
                 \n\
                 Authed tools (need a GW2 API key from https://account.arena.net/applications):\n\
                 - get_wallet — wallet contents + currency metadata (scopes: account, wallet).\n\
                 - get_character_build — every build/equipment tab for a character\n\
                   (scopes: account, characters, builds).\n\
                 \n\
                 Resource: gw2://currencies — full currency list as JSON."
                    .to_owned(),
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(build_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = request.arguments.map_or_else(
            || serde_json::Value::Object(serde_json::Map::new()),
            serde_json::Value::Object,
        );

        // Single dispatch path — `dispatch_tool` is also exercised by
        // integration tests, so the protocol handler can never drift out
        // of sync with the test harness.
        //
        // `CallToolResult::structured` populates both `structured_content`
        // (for clients that consume the typed shape) and the text-content
        // mirror (for back-compat with clients that only read text). No
        // round-trip parse, no silent structure loss on malformed JSON.
        match self.dispatch_tool(&request.name, args).await {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(concrete_resources()))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            resource_templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        match self.read_resource_body(&request.uri).await {
            Ok(body) => Ok(ReadResourceResult {
                contents: vec![ResourceContents::TextResourceContents {
                    uri: request.uri,
                    mime_type: Some(RESOURCE_JSON_MIME.to_owned()),
                    text: body,
                    meta: None,
                }],
            }),
            Err(msg) => {
                if msg.starts_with("resource_not_found:") {
                    Err(ErrorData::resource_not_found(msg, None))
                } else {
                    Err(ErrorData::internal_error(msg, None))
                }
            }
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(build_prompts()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        match render_prompt(&request.name, &args) {
            Ok(result) => Ok(result),
            Err(PromptError::NotFound(name)) => Err(ErrorData::invalid_params(
                format!("unknown prompt: {name}"),
                None,
            )),
            Err(PromptError::MissingArg { prompt, arg }) => Err(ErrorData::invalid_params(
                format!("prompt '{prompt}' is missing required argument '{arg}'"),
                None,
            )),
        }
    }
}

#[allow(clippy::too_many_lines)] // Each tool needs its own schema literal; refactoring into a
// table-driven form would obscure them more than help.
fn build_tools() -> Vec<Tool> {
    let wiki_search: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query (e.g. 'Dragon Bash', 'wallet', 'Mystic Forge')."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 50,
                "default": 5,
                "description": "Maximum results to return."
            }
        },
        "required": ["query"]
    }))
    .expect("valid schema literal");
    let get_wallet: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "api_key": {
                "type": "string",
                "description": "GW2 API key with 'account' and 'wallet' scopes. \
                                Generate at https://account.arena.net/applications."
            }
        },
        "required": ["api_key"]
    }))
    .expect("valid schema literal");
    let get_currencies: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "ids": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "description": "Specific currency ids. Omit (or pass empty) to fetch all."
            }
        }
    }))
    .expect("valid schema literal");

    let by_required_ids_with_summary: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "items": { "type": "integer", "minimum": 1 },
                    "minItems": 1,
                    "description": "Ids to fetch. Required — there are 1000s of entries; pass only what you need."
                },
                "summary": {
                    "type": "boolean",
                    "default": true,
                    "description": "When true (default), return a compact projection (id, name, description, slot/type) and drop facts[] + icon URLs. Set false for the full GW2 API shape."
                }
            },
            "required": ["ids"]
        }))
        .expect("valid schema literal");

    let by_required_ids: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "ids": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "minItems": 1,
                "description": "Ids to fetch. Required — there are 1000s of entries; pass only what you need."
            }
        },
        "required": ["ids"]
    }))
    .expect("valid schema literal");

    let get_character_build: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "api_key": { "type": "string", "description": "GW2 API key with 'account' + 'characters' + 'builds' scopes." },
                "character": { "type": "string", "description": "Character name (case-sensitive)." },
                "tab": {
                    "type": ["string", "integer"],
                    "default": "active",
                    "description": "Which tab(s) to return: \"active\" (default; the in-game-equipped one), \"all\", or a numeric tab index like 1, 2, 3."
                }
            },
            "required": ["api_key", "character"]
        }))
        .expect("valid schema literal");

    let decode_build_code: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "Build chat code, e.g. `[&DQ...=]`. Decoded into structured JSON (profession, specs, traits, palette skill ids, pets/legends)."
            }
        },
        "required": ["code"]
    }))
    .expect("valid schema literal");

    let list_recommended: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "description": "Source name from list_build_sources (e.g. discretize, metabattle, snowcrows)." },
            "profession": {
                "type": "string",
                "enum": [
                    "guardian", "warrior", "engineer", "ranger", "thief",
                    "elementalist", "mesmer", "necromancer", "revenant"
                ],
                "description": "Optional profession filter (lower-case). Catalogs match case-insensitively."
            },
            "gamemode": {
                "type": "string",
                "enum": ["fractals", "raids", "strikes", "open_world", "wvw", "pvp"],
                "description": "Optional game-mode filter. Available values depend on the source: discretize → fractals only; snowcrows → raids/strikes; metabattle → all."
            },
            "limit": { "type": "integer", "minimum": 1, "description": "Cap on number of results." }
        },
        "required": ["source"]
    }))
    .expect("valid schema literal");

    let get_recommended: rmcp::model::JsonObject = serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "description": "Source name." },
            "slug": { "type": "string", "description": "Build slug from list_recommended_builds." }
        },
        "required": ["source", "slug"]
    }))
    .expect("valid schema literal");

    let empty_args: rmcp::model::JsonObject =
        serde_json::from_value(serde_json::json!({ "type": "object" }))
            .expect("valid schema literal");

    vec![
        Tool::new(
            "wiki_search",
            "Search the Guild Wars 2 wiki and return enriched results (with prose extracts).",
            wiki_search,
        )
        .annotate(read_only_open_world("Search GW2 Wiki"))
        .with_output_schema::<crate::domain::SearchResponse>(),
        Tool::new(
            "get_wallet",
            "Fetch the user's wallet, including currency metadata. Requires an API key.",
            get_wallet,
        )
        .annotate(read_only_open_world("Get Wallet"))
        .with_output_schema::<crate::domain::WalletInfo>(),
        // get_currencies, get_skills, get_traits, get_specializations, and
        // get_items all return `BTreeMap<TypedId, T>`. The wire shape is a JSON
        // object keyed by stringified numeric ids — useful for the LLM but
        // schemars 1.x emits a non-`object`-rooted schema for maps with
        // non-string key types, which `Tool::with_output_schema` rejects per
        // the MCP spec. Keeping the wire shape (and Tier 1's tests) wins;
        // outputSchema is left off these five intentionally. See report.
        Tool::new(
            "get_currencies",
            "Fetch Guild Wars 2 currency metadata. Pass `ids` for specific currencies; omit to \
             fetch all.",
            get_currencies,
        )
        .annotate(read_only_open_world("Get Currencies")),
        Tool::new(
            "get_skills",
            "Resolve GW2 skill ids (e.g. those returned by get_character_build) into name + description. Returns a compact summary by default; pass `summary=false` for the full payload (facts[], icon URLs, etc.).",
            by_required_ids_with_summary.clone(),
        )
        .annotate(read_only_closed_world("Get Skills")),
        Tool::new(
            "get_traits",
            "Resolve GW2 trait ids into name + description. Returns a compact summary by default; pass `summary=false` for the full payload.",
            by_required_ids_with_summary.clone(),
        )
        .annotate(read_only_closed_world("Get Traits")),
        Tool::new(
            "get_specializations",
            "Resolve GW2 specialization ids (core + elite) into name, profession, and minor/major trait ids. Pass `summary=false` for the full payload.",
            by_required_ids_with_summary,
        )
        .annotate(read_only_closed_world("Get Specializations")),
        Tool::new(
            "get_items",
            "Resolve GW2 equipment / item ids (e.g. those returned by get_character_build) into name and details.",
            by_required_ids,
        )
        .annotate(read_only_closed_world("Get Items")),
        Tool::new(
            "get_character_build",
            "Fetch a character's build + equipment, with skill/trait/specialization names pre-resolved. Defaults to the active tab; pass `tab=\"all\"` or a specific index for others. Requires an API key with 'builds' scope.",
            get_character_build,
        )
        .annotate(read_only_open_world("Get Character Build"))
        .with_output_schema::<crate::service::CharacterBuildSnapshot>(),
        Tool::new(
            "decode_build_code",
            "Decode a `[&Dw...]` build chat code into structured JSON. No auth required.",
            decode_build_code,
        )
        .annotate(read_only_closed_world("Decode Build Chat Code")),
        Tool::new(
            "list_build_sources",
            "List the registered curated-build sources (Discretize, MetaBattle, Snow Crows, …).",
            empty_args,
        )
        .annotate(read_only_closed_world("List Build Sources")),
        // list_recommended_builds returns Vec<BuildSummary>. JSON Schema for
        // an array root is valid in general, but `Tool::with_output_schema`
        // enforces MCP's "outputSchema must be type=object" rule, so we'd
        // have to wrap as `{items: [...]}` and break the wire shape (and
        // Tier 1's tests). Skipped — the per-item shape is documented in
        // `BuildSummary` and clients that care can derive the schema there.
        Tool::new(
            "list_recommended_builds",
            "List builds from a curated source. Returns lightweight summaries; use get_recommended_build for full details.",
            list_recommended,
        )
        .annotate(read_only_open_world("List Recommended Builds")),
        Tool::new(
            "get_recommended_build",
            "Fetch full details for a specific curated build by source + slug.",
            get_recommended,
        )
        .annotate(read_only_open_world("Get Recommended Build"))
        .with_output_schema::<crate::ports::BuildDetail>(),
    ]
}

/// Builder for a `ToolAnnotations` describing a read-only, idempotent,
/// non-destructive tool whose data crosses the network boundary (open world).
fn read_only_open_world(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .idempotent(true)
        .destructive(false)
        .open_world(true)
}

/// Builder for a `ToolAnnotations` describing a read-only, idempotent,
/// non-destructive tool whose dataset is finite and offline (closed world).
fn read_only_closed_world(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .idempotent(true)
        .destructive(false)
        .open_world(false)
}

// ---------------------------------------------------------------------------
// Resources & resource templates
// ---------------------------------------------------------------------------

/// The non-template resources that show up in `resources/list`. These have
/// fixed URIs that resolve straight to a service call — no parameters.
fn concrete_resources() -> Vec<rmcp::model::Resource> {
    let mut currencies = RawResource::new(CURRENCIES_RESOURCE_URI, "Guild Wars 2 Currencies");
    currencies.description = Some(
        "Complete list of Guild Wars 2 currencies with metadata (name, description, icon, order)."
            .to_owned(),
    );
    currencies.mime_type = Some(RESOURCE_JSON_MIME.to_owned());

    let mut discretize = RawResource::new(BUILDS_DISCRETIZE_URI, "Discretize Builds");
    discretize.description = Some(
        "Listing of curated fractal builds from Discretize (https://discretize.eu). Returns the \
         same shape as `list_recommended_builds` with no filter."
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
        "Listing of curated raid/strike builds from Snow Crows (https://snowcrows.com).".to_owned(),
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
fn resource_templates() -> Vec<rmcp::model::ResourceTemplate> {
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
             `metabattle`, `snowcrows`. `slug` matches `list_recommended_builds`'s slug field — \
             note that some sources use multi-segment slugs (discretize: \
             `<profession>/<build>`; snowcrows: `<category>/<profession>/<build>`); the slug is \
             taken literally from the URI suffix after `gw2://builds/<source>/`.",
        ),
    ]
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

/// Slash commands surfaced via `prompts/list`. The body of each prompt is
/// rendered server-side in [`render_prompt`] — keep the two in sync.
fn build_prompts() -> Vec<Prompt> {
    fn arg(name: &str, description: &str, required: bool) -> PromptArgument {
        PromptArgument {
            name: name.to_owned(),
            title: None,
            description: Some(description.to_owned()),
            required: Some(required),
        }
    }

    vec![
        Prompt::new(
            PROMPT_ANALYZE_CHARACTER,
            Some(
                "Fetch a character's build and equipment, resolve all IDs to names, and produce \
                 a build summary.",
            ),
            Some(vec![
                arg("character", "GW2 character name (case-sensitive).", true),
                arg(
                    "api_key",
                    "GW2 API key with `account`, `characters`, and `builds` scopes. Generate at \
                     https://account.arena.net/applications.",
                    true,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_COMPARE_TO_META,
            Some(
                "Compare a character's current build to curated meta builds for their profession.",
            ),
            Some(vec![
                arg("character", "GW2 character name.", true),
                arg(
                    "api_key",
                    "GW2 API key with `account`, `characters`, and `builds` scopes.",
                    true,
                ),
                arg(
                    "gamemode",
                    "Optional. One of: `fractals`, `raids`, `open_world`, `pvp`, `wvw`. If \
                     omitted the prompt asks the user.",
                    false,
                ),
            ]),
        ),
        Prompt::new(
            PROMPT_DECODE_AND_EXPLAIN,
            Some("Decode a Guild Wars 2 build chat code and explain what the build does."),
            Some(vec![arg(
                "code",
                "GW2 build chat code, including the surrounding `[& ... ]` brackets.",
                true,
            )]),
        ),
        Prompt::new(
            PROMPT_RECOMMEND_BUILD,
            Some("Recommend a curated meta build for a profession + gamemode and explain it."),
            Some(vec![
                arg(
                    "profession",
                    "One of the nine professions: `guardian`, `warrior`, `engineer`, `ranger`, \
                     `thief`, `elementalist`, `mesmer`, `necromancer`, `revenant`.",
                    true,
                ),
                arg(
                    "gamemode",
                    "One of: `fractals`, `raids`, `open_world`, `pvp`, `wvw`.",
                    true,
                ),
                arg(
                    "experience",
                    "Optional. One of: `beginner`, `intermediate`, `expert`. Tunes the \
                     explanation depth.",
                    false,
                ),
            ]),
        ),
    ]
}

const PROMPT_ANALYZE_CHARACTER: &str = "analyze-character";
const PROMPT_COMPARE_TO_META: &str = "compare-to-meta";
const PROMPT_DECODE_AND_EXPLAIN: &str = "decode-and-explain";
const PROMPT_RECOMMEND_BUILD: &str = "recommend-build";

#[derive(Debug)]
enum PromptError {
    NotFound(String),
    MissingArg {
        prompt: &'static str,
        arg: &'static str,
    },
}

/// Render a prompt name + args into a `GetPromptResult`. Pure function: no
/// I/O, no state — each prompt boils down to a templated user message that
/// instructs the LLM which tools to call. Exposed at module scope so the
/// integration tests can pin the rendered body without spinning up a full
/// `McpServer`.
fn require_arg(
    args: &serde_json::Map<String, serde_json::Value>,
    prompt: &'static str,
    arg: &'static str,
) -> Result<String, PromptError> {
    args.get(arg)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(PromptError::MissingArg { prompt, arg })
}

fn optional_arg(args: &serde_json::Map<String, serde_json::Value>, arg: &str) -> Option<String> {
    args.get(arg)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn finish_prompt(description: &str, body: String) -> GetPromptResult {
    GetPromptResult {
        description: Some(description.to_owned()),
        messages: vec![PromptMessage::new_text(PromptMessageRole::User, body)],
    }
}

fn render_analyze_character(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let character = require_arg(args, PROMPT_ANALYZE_CHARACTER, "character")?;
    let api_key = require_arg(args, PROMPT_ANALYZE_CHARACTER, "api_key")?;
    let body = format!(
        "You are analysing a Guild Wars 2 character build.\n\n\
         Steps to follow:\n\n\
         1. Use the `get_character_build` tool with `character=\"{character}\"` and \
         `api_key=\"{api_key}\"`. The response now pre-resolves skill, trait, and \
         specialization names — you do not need to call `get_skills` / `get_traits` \
         / `get_specializations` again for those ids.\n\
         2. For any equipment ids that come back unresolved (slot entries with `id` \
         fields under `equipment`), use the `get_items` tool with the full list of \
         ids in one call.\n\
         3. Summarise the build: profession + elite spec, the role it plays, the \
         gamemode it likely fits, rotation hints derived from the chosen skills, and \
         gear quality (ascended vs exotic, sigils, runes, infusions).\n\
         4. Call out anything missing or unusual (empty equipment slots, mismatched \
         stat sets, unexpected utility-skill choices) so the user knows what to look \
         at."
    );
    Ok(finish_prompt(
        "Fetch a character's build and equipment, resolve all IDs to names, and produce a build \
         summary.",
        body,
    ))
}

fn render_compare_to_meta(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let character = require_arg(args, PROMPT_COMPARE_TO_META, "character")?;
    let api_key = require_arg(args, PROMPT_COMPARE_TO_META, "api_key")?;
    let body = match optional_arg(args, "gamemode") {
        Some(gm) => format!(
            "You are comparing a Guild Wars 2 character to curated meta builds.\n\n\
             Steps to follow:\n\n\
             1. Call `get_character_build` with `character=\"{character}\"` and \
             `api_key=\"{api_key}\"` to capture the current build (the response \
             pre-resolves skill/trait/spec names).\n\
             2. Pick the catalog source that fits `gamemode=\"{gm}\"`: \
             `discretize` for fractals, `snowcrows` for raids/strikes, `metabattle` for \
             anything else.\n\
             3. Call `list_recommended_builds` with that source, the character's \
             profession (lower-cased), and `gamemode=\"{gm}\"`. Pick the best match \
             (highest rating if present, otherwise the closest role/elite-spec match).\n\
             4. Call `get_recommended_build` with the chosen `source` and `slug` to get \
             the canonical build details.\n\
             5. Produce a gap analysis: traits + skills that differ, equipment / stat \
             differences, sigils/runes, and any rotation steps the character can't \
             execute with its current setup. Recommend the smallest set of changes that \
             would close the gap."
        ),
        None => format!(
            "You are comparing a Guild Wars 2 character to curated meta builds, but the \
             gamemode was not provided.\n\n\
             Step 1: Ask the user which gamemode they're targeting (one of `fractals`, \
             `raids`, `open_world`, `pvp`, `wvw`) and wait for their answer before \
             proceeding.\n\n\
             Once the user responds, follow the standard flow:\n\n\
             1. Call `get_character_build` with `character=\"{character}\"` and \
             `api_key=\"{api_key}\"`.\n\
             2. Pick the catalog source that fits the user's gamemode: `discretize` for \
             fractals, `snowcrows` for raids/strikes, `metabattle` otherwise.\n\
             3. Call `list_recommended_builds` with that source plus the profession and \
             gamemode filters, then `get_recommended_build` with the best match's slug.\n\
             4. Produce a gap analysis: traits + skills that differ, equipment / stat \
             differences, and the smallest changes that would close the gap."
        ),
    };
    Ok(finish_prompt(
        "Compare a character's current build to curated meta builds for their profession.",
        body,
    ))
}

fn render_decode_and_explain(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let code = require_arg(args, PROMPT_DECODE_AND_EXPLAIN, "code")?;
    let body = format!(
        "You are explaining a Guild Wars 2 build chat code in plain English.\n\n\
         Steps to follow:\n\n\
         1. Call `decode_build_code` with `code=\"{code}\"` to extract the \
         profession byte, specialization ids, trait choices, and palette skill ids \
         (with their resolved api skill ids and profession name).\n\
         2. Collect the resolved trait ids across all three specialization slots and \
         pass them to `get_traits` in one call. Pass the resolved api skill ids to \
         `get_skills` in one call. If the build references specialization ids you \
         want descriptions for, pass them to `get_specializations`.\n\
         3. Produce a plain-English explanation of what the build does: profession + \
         elite spec (named, not numeric), the role it fills, key trait synergies, \
         and what each skill in the bar contributes. Keep it readable for a player \
         who has not seen this build before."
    );
    Ok(finish_prompt(
        "Decode a Guild Wars 2 build chat code and explain what the build does.",
        body,
    ))
}

fn render_recommend_build(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    let profession = require_arg(args, PROMPT_RECOMMEND_BUILD, "profession")?;
    let gamemode = require_arg(args, PROMPT_RECOMMEND_BUILD, "gamemode")?;
    let experience = optional_arg(args, "experience").unwrap_or_else(|| "intermediate".to_owned());
    let source = match gamemode.as_str() {
        "fractals" => "discretize",
        "raids" | "strikes" => "snowcrows",
        _ => "metabattle",
    };
    let body = format!(
        "You are recommending a Guild Wars 2 meta build.\n\n\
         Steps to follow:\n\n\
         1. Call `list_recommended_builds` with `source=\"{source}\"`, \
         `profession=\"{profession}\"`, and `gamemode=\"{gamemode}\"`. Pick the \
         top-rated match (or the closest role match if the catalog doesn't carry \
         ratings).\n\
         2. Call `get_recommended_build` with the chosen `source` and `slug` to \
         fetch the full build detail.\n\
         3. Explain the build to a {experience} player. For `beginner`, lead with \
         the role the build plays and avoid jargon; describe the rotation as a \
         short, ordered list. For `intermediate`, include trait synergies and key \
         boon outputs. For `expert`, include CC priorities, edge-case rotations, \
         and the trade-offs vs adjacent builds in the same role.\n\
         4. Always finish with the chat code (if the catalog provided one) and the \
         source URL so the user can verify."
    );
    Ok(finish_prompt(
        "Recommend a curated meta build for a profession + gamemode and explain it.",
        body,
    ))
}

fn render_prompt(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<GetPromptResult, PromptError> {
    match name {
        PROMPT_ANALYZE_CHARACTER => render_analyze_character(args),
        PROMPT_COMPARE_TO_META => render_compare_to_meta(args),
        PROMPT_DECODE_AND_EXPLAIN => render_decode_and_explain(args),
        PROMPT_RECOMMEND_BUILD => render_recommend_build(args),
        other => Err(PromptError::NotFound(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `with_output_schema::<T>()` panics if `T`'s schemars-generated schema
    /// doesn't have root `type: "object"`. Make that failure show up at
    /// `cargo test` instead of at server startup.
    #[test]
    fn build_tools_constructs_without_panicking() {
        let tools = build_tools();
        assert_eq!(tools.len(), 12, "tier-2 ships 12 tools");
    }

    #[test]
    fn every_tool_has_annotations() {
        for t in build_tools() {
            let ann = t
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool {} missing annotations", t.name));
            assert_eq!(
                ann.read_only_hint,
                Some(true),
                "{}: every tool in this server is read-only",
                t.name
            );
            assert_eq!(
                ann.destructive_hint,
                Some(false),
                "{}: nothing in this server is destructive",
                t.name
            );
            assert_eq!(
                ann.idempotent_hint,
                Some(true),
                "{}: every tool is idempotent",
                t.name
            );
            assert!(
                ann.open_world_hint.is_some(),
                "{}: openWorld must be set explicitly (default true would be wrong for offline tools)",
                t.name
            );
            assert!(
                ann.title.as_ref().is_some_and(|s| !s.is_empty()),
                "{}: title must be set",
                t.name
            );
        }
    }

    #[test]
    fn typed_return_tools_publish_output_schema() {
        let by_name: std::collections::BTreeMap<_, _> = build_tools()
            .into_iter()
            .map(|t| (t.name.clone(), t))
            .collect();
        for name in [
            "wiki_search",
            "get_wallet",
            "get_character_build",
            "get_recommended_build",
        ] {
            let t = by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                t.output_schema.is_some(),
                "{name}: outputSchema must be declared"
            );
        }
    }
}
