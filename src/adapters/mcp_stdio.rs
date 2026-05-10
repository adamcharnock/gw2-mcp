//! MCP transport adapter — exposes the [`Service`](crate::service::Service)
//! over the Model Context Protocol via stdio (the standard MCP transport).
//!
//! This is the only place that imports `rmcp`. New transports (HTTP/SSE,
//! daemon mode) would be additional adapters wrapping the same `Service`.

use std::sync::Arc;

use rmcp::model::{
    Annotated, CallToolRequestParams, CallToolResult, Content, Implementation, ListResourcesResult,
    ListToolsResult, RawResource, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};

use crate::domain::{
    ApiKey, BuildChatCode, CharacterName, CurrencyId, SearchLimit, SearchQuery, SkillId,
    SpecializationId, TraitId,
};
use crate::ports::CatalogFilter;
use crate::service::Service;

const CURRENCIES_RESOURCE_URI: &str = "gw2://currencies";

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
    /// the JSON string the MCP client would receive on success, or a
    /// human-readable error message on failure.
    ///
    /// Used by integration tests to exercise tool wiring without driving
    /// the stdio protocol end-to-end.
    pub async fn dispatch_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<String, String> {
        let outcome = match name {
            "wiki_search" => self.handle_wiki_search(&args).await,
            "get_wallet" => self.handle_get_wallet(&args).await,
            "get_currencies" => self.handle_get_currencies(&args).await,
            "get_skills" => self.handle_get_skills(&args).await,
            "get_traits" => self.handle_get_traits(&args).await,
            "get_specializations" => self.handle_get_specializations(&args).await,
            "get_character_build" => self.handle_get_character_build(&args).await,
            "decode_build_code" => self.handle_decode_build_code_sync(&args),
            "list_build_sources" => self.handle_list_build_sources(),
            "list_recommended_builds" => self.handle_list_recommended_builds(&args).await,
            "get_recommended_build" => self.handle_get_recommended_build(&args).await,
            other => return Err(format!("unknown tool: {other}")),
        };
        outcome.map_err(|e| e.to_string())
    }

    /// Read a resource by URI. Returns the body the MCP client would receive.
    pub async fn read_resource_for_test(&self, uri: &str) -> Result<String, String> {
        if uri == CURRENCIES_RESOURCE_URI {
            let map = self
                .service
                .get_currencies(&[])
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_string_pretty(&map).map_err(|e| e.to_string())
        } else {
            Err(format!("resource_not_found: {uri}"))
        }
    }

    // -- tool dispatch --------------------------------------------------

    async fn handle_wiki_search(&self, args: &serde_json::Value) -> Result<String, CallError> {
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
        Ok(serde_json::to_string_pretty(&response)?)
    }

    async fn handle_get_wallet(&self, args: &serde_json::Value) -> Result<String, CallError> {
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
        Ok(serde_json::to_string_pretty(&wallet)?)
    }

    async fn handle_get_currencies(&self, args: &serde_json::Value) -> Result<String, CallError> {
        let ids = parse_id_array(args, "ids", CurrencyId::new)?;
        let map = self
            .service
            .get_currencies(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&map)?)
    }

    async fn handle_get_skills(&self, args: &serde_json::Value) -> Result<String, CallError> {
        let ids = parse_required_id_array(args, "ids", SkillId::new)?;
        let map = self
            .service
            .get_skills(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&map)?)
    }

    async fn handle_get_traits(&self, args: &serde_json::Value) -> Result<String, CallError> {
        let ids = parse_required_id_array(args, "ids", TraitId::new)?;
        let map = self
            .service
            .get_traits(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&map)?)
    }

    async fn handle_get_specializations(
        &self,
        args: &serde_json::Value,
    ) -> Result<String, CallError> {
        let ids = parse_required_id_array(args, "ids", SpecializationId::new)?;
        let map = self
            .service
            .get_specializations(&ids)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&map)?)
    }

    async fn handle_get_character_build(
        &self,
        args: &serde_json::Value,
    ) -> Result<String, CallError> {
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
        let snap = self
            .service
            .get_character_build(&key, &name)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&snap)?)
    }

    fn handle_decode_build_code_sync(&self, args: &serde_json::Value) -> Result<String, CallError> {
        let raw = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("code"))?;
        let code = BuildChatCode::new(raw).map_err(CallError::Domain)?;
        let value = self
            .service
            .decode_build_code(&code)
            .map_err(CallError::Service)?;
        Ok(serde_json::to_string_pretty(&value)?)
    }

    fn handle_list_build_sources(&self) -> Result<String, CallError> {
        let names = self.service.list_catalogs();
        Ok(serde_json::to_string_pretty(&names)?)
    }

    async fn handle_list_recommended_builds(
        &self,
        args: &serde_json::Value,
    ) -> Result<String, CallError> {
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
        Ok(serde_json::to_string_pretty(&summaries)?)
    }

    async fn handle_get_recommended_build(
        &self,
        args: &serde_json::Value,
    ) -> Result<String, CallError> {
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
        Ok(serde_json::to_string_pretty(&detail)?)
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
        ServerInfo {
            protocol_version: rmcp::model::ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "gw2-mcp".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            },
            instructions: Some(
                "Guild Wars 2 MCP server. Tools: wiki_search (search the GW2 wiki), \
                 get_wallet (returns the user's wallet — requires a GW2 API key with \
                 'wallet' scope), get_currencies (currency metadata; pass `ids` for a \
                 subset, omit for all). Resource: gw2://currencies (full currency list)."
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

        let outcome = match request.name.as_ref() {
            "wiki_search" => self.handle_wiki_search(&args).await,
            "get_wallet" => self.handle_get_wallet(&args).await,
            "get_currencies" => self.handle_get_currencies(&args).await,
            other => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "unknown tool: {other}"
                ))]));
            }
        };

        match outcome {
            Ok(text) => {
                let mut result = CallToolResult::success(vec![Content::text(text.clone())]);
                result.structured_content = serde_json::from_str(&text).ok();
                Ok(result)
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut raw = RawResource::new(CURRENCIES_RESOURCE_URI, "Guild Wars 2 Currencies");
        raw.description = Some(
            "Complete list of Guild Wars 2 currencies with metadata (name, description, icon, \
             order)."
                .to_owned(),
        );
        raw.mime_type = Some("application/json".to_owned());
        Ok(ListResourcesResult::with_all_items(vec![Annotated::new(
            raw, None,
        )]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        if request.uri == CURRENCIES_RESOURCE_URI {
            let map = self
                .service
                .get_currencies(&[])
                .await
                .map_err(|e| ErrorData::internal_error(format!("{e}"), None))?;
            let body = serde_json::to_string_pretty(&map)
                .map_err(|e| ErrorData::internal_error(format!("{e}"), None))?;
            Ok(ReadResourceResult {
                contents: vec![ResourceContents::text(body, request.uri)],
            })
        } else {
            Err(ErrorData::resource_not_found(
                format!("resource_not_found: {}", request.uri),
                None,
            ))
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
                "character": { "type": "string", "description": "Character name (case-sensitive)." }
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
            "profession": { "type": "string", "description": "Optional profession filter (e.g. 'guardian')." },
            "gamemode": { "type": "string", "description": "Optional game-mode filter (e.g. 'fractals', 'raids')." },
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
        ),
        Tool::new(
            "get_wallet",
            "Fetch the user's wallet, including currency metadata. Requires an API key.",
            get_wallet,
        ),
        Tool::new(
            "get_currencies",
            "Fetch Guild Wars 2 currency metadata. Pass `ids` for specific currencies; omit to \
             fetch all.",
            get_currencies,
        ),
        Tool::new(
            "get_skills",
            "Resolve GW2 skill ids (e.g. those returned by get_character_build) into name + description + facts.",
            by_required_ids.clone(),
        ),
        Tool::new(
            "get_traits",
            "Resolve GW2 trait ids into name + description + facts.",
            by_required_ids.clone(),
        ),
        Tool::new(
            "get_specializations",
            "Resolve GW2 specialization ids (core + elite) into name, profession, and minor/major trait ids.",
            by_required_ids,
        ),
        Tool::new(
            "get_character_build",
            "Fetch every build/equipment tab for a character. Requires an API key with 'builds' scope.",
            get_character_build,
        ),
        Tool::new(
            "decode_build_code",
            "Decode a `[&Dw...]` build chat code into structured JSON. No auth required.",
            decode_build_code,
        ),
        Tool::new(
            "list_build_sources",
            "List the registered curated-build sources (Discretize, MetaBattle, Snow Crows, …).",
            empty_args,
        ),
        Tool::new(
            "list_recommended_builds",
            "List builds from a curated source. Returns lightweight summaries; use get_recommended_build for full details.",
            list_recommended,
        ),
        Tool::new(
            "get_recommended_build",
            "Fetch full details for a specific curated build by source + slug.",
            get_recommended,
        ),
    ]
}
