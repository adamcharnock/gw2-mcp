//! MCP transport adapter — exposes the [`Service`](crate::service::Service)
//! over the Model Context Protocol via stdio (the standard MCP transport).
//!
//! This is the only place that imports `rmcp`. New transports (HTTP/SSE,
//! daemon mode) would be additional adapters wrapping the same `Service`.

mod parsing;
mod prompts;
mod resources;
mod tools;

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, GetPromptRequestParams, GetPromptResult,
    Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, Prompt, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};

use parsing::{
    CallError, CursorPayload, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, annotate_endpoint,
    catalog_filter_hash, decode_cursor, encode_cursor, parse_id_array, parse_location_ref,
    parse_nearby_filter, parse_optional_str, parse_optional_u32, parse_required_id_array,
    parse_search_limit, parse_search_query, parse_summary, parse_tab_selector,
};
use prompts::{PromptError, build_prompts, render_prompt};
use resources::{
    BUILDS_DISCRETIZE_URI, BUILDS_METABATTLE_URI, BUILDS_SNOWCROWS_URI, CURRENCIES_RESOURCE_URI,
    ITEMS_PREFIX, RESOURCE_JSON_MIME, SKILLS_PREFIX, SPECS_PREFIX, TRAITS_PREFIX,
    concrete_resources, parse_build_uri, parse_typed_id_uri, resource_templates,
};
use tools::build_tools;

use crate::domain::{
    ApiKey, BuildChatCode, BuildSlug, CharacterName, CurrencyId, ItemId, SearchLimit, SearchQuery,
    SkillId, SpecializationId, TraitId,
};
use crate::ports::{
    AchievementSearchFilter, CatalogFilter, ItemSearchFilter, SkillSearchFilter, SpecSearchFilter,
    TraitSearchFilter,
};
use crate::service::{DailiesWhich, Service};

/// Server runbook — what good clients put in the system prompt and what
/// `get_info` returns verbatim. Keep both call sites pointing here so the
/// surface stays single-source.
///
/// Plain markdown, ~600-1200 words, written for an LLM that has not
/// previously seen GW2.
const SERVER_RUNBOOK: &str = include_str!("../../../assets/runbook.md");

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
            "list_catalog_sources" => self.handle_list_catalog_sources(),
            "list_catalog_builds" => self.handle_list_catalog_builds(&args).await,
            "get_catalog_build" => self.handle_get_catalog_build(&args).await,
            "get_account" => self.handle_get_account(&args).await,
            "list_characters" => self.handle_list_characters(&args).await,
            "get_account_achievements" => self.handle_get_account_achievements(&args).await,
            "get_account_masteries" => self.handle_get_account_masteries(&args).await,
            "get_account_raids" => self.handle_get_account_raids(&args).await,
            "get_account_dungeons" => self.handle_get_account_dungeons(&args).await,
            "get_dailies" => self.handle_get_dailies(&args).await,
            "get_my_location" => self.handle_get_my_location().await,
            "get_directions" => self.handle_get_directions(&args).await,
            "find_nearby" => self.handle_find_nearby(&args).await,
            "list_maps_in_region" => self.handle_list_maps_in_region(&args).await,
            "get_map_neighbors" => self.handle_get_map_neighbors(&args),
            "describe_facing" => self.handle_describe_facing().await,
            "search_skills" => self.handle_search_skills(&args).await,
            "search_traits" => self.handle_search_traits(&args).await,
            "search_specializations" => self.handle_search_specializations(&args).await,
            "search_items" => self.handle_search_items(&args).await,
            "search_achievements" => self.handle_search_achievements(&args).await,
            "get_index_status" => self.handle_get_index_status().await,
            "get_info" => Ok(serde_json::Value::String(SERVER_RUNBOOK.to_owned())),
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
            let (source, slug_raw) = parsed?;
            let slug = BuildSlug::new(slug_raw).map_err(|e| e.to_string())?;
            let detail = self
                .service
                .get_catalog_build(source, &slug)
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
        let key = self.resolve_api_key(args)?;
        let wallet = self
            .service
            .get_wallet(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "get_wallet"))?;
        Ok(serde_json::to_value(&wallet)?)
    }

    /// Resolve the API key for an authed tool call: explicit `api_key`
    /// argument takes priority; if absent, fall back to the service's
    /// default (loaded from `--api-key` / `GW2_API_KEY` at startup);
    /// if neither, raise [`CallError::NoApiKey`] so the caller sees a
    /// single actionable message.
    fn resolve_api_key(&self, args: &serde_json::Value) -> Result<ApiKey, CallError> {
        if let Some(raw) = args.get("api_key").and_then(|v| v.as_str())
            && !raw.trim().is_empty()
        {
            return ApiKey::new(raw).map_err(CallError::Domain);
        }
        self.service
            .default_api_key()
            .cloned()
            .ok_or(CallError::NoApiKey)
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
        let raw_name = args
            .get("character")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("character"))?;
        let key = self.resolve_api_key(args)?;
        let name = CharacterName::new(raw_name).map_err(CallError::Domain)?;
        let tab = parse_tab_selector(args)?;
        let snap = self
            .service
            .get_character_build(&key, &name, tab)
            .await
            .map_err(|e| annotate_endpoint(e, "get_character_build"))?;
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

    fn handle_list_catalog_sources(&self) -> Result<serde_json::Value, CallError> {
        let names = self.service.list_catalogs();
        Ok(serde_json::to_value(&names)?)
    }

    async fn handle_list_catalog_builds(
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
            // `limit` is intentionally not threaded into CatalogFilter on the
            // MCP layer any more — pagination supersedes it. Internally the
            // service still honours it (some tests pass a filter directly),
            // but the MCP tool only exposes `cursor` + `page_size`.
            limit: None,
        };

        // Page size: default 25, clamp to 100 (silently — clients should not
        // pay for guessing wrong, the server picks a safe ceiling).
        let page_size = args
            .get("page_size")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_PAGE_SIZE, |n| {
                u32::try_from(n).unwrap_or(MAX_PAGE_SIZE).min(MAX_PAGE_SIZE)
            })
            .max(1);

        let filter_hash = catalog_filter_hash(source, &filter);
        let offset = match args.get("cursor").and_then(|v| v.as_str()) {
            None => 0u32,
            Some(c) => {
                let decoded = decode_cursor(c).map_err(|reason| CallError::BadCursor { reason })?;
                if decoded.filter_hash != filter_hash {
                    return Err(CallError::BadCursor {
                        reason: "cursor was minted with different (source, profession, gamemode) — \
                             restart pagination from the beginning",
                    });
                }
                decoded.offset
            }
        };

        let summaries = self
            .service
            .list_catalog_builds(source, filter)
            .await
            .map_err(CallError::Service)?;

        let total = u32::try_from(summaries.len()).unwrap_or(u32::MAX);
        let start = offset.min(total) as usize;
        let end = (offset.saturating_add(page_size)).min(total) as usize;
        let items: Vec<_> = summaries
            .into_iter()
            .skip(start)
            .take(end - start)
            .collect();

        let next_offset = u32::try_from(end).unwrap_or(u32::MAX);
        let next_cursor = if next_offset < total {
            Some(encode_cursor(&CursorPayload {
                filter_hash,
                offset: next_offset,
            }))
        } else {
            None
        };

        Ok(serde_json::json!({
            "items": items,
            "next_cursor": next_cursor,
        }))
    }

    async fn handle_get_catalog_build(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("source"))?;
        let slug_raw = args
            .get("slug")
            .and_then(|v| v.as_str())
            .ok_or(CallError::MissingArg("slug"))?;
        let slug = BuildSlug::new(slug_raw).map_err(CallError::Domain)?;
        let detail = self
            .service
            .get_catalog_build(source, &slug)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&detail)?)
    }

    // -------------------------------------------------------------------
    // Tier 6A — account / progression / dailies handlers.
    // -------------------------------------------------------------------

    async fn handle_get_account(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let acc = self
            .service
            .get_account(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "get_account"))?;
        Ok(serde_json::to_value(&acc)?)
    }

    async fn handle_list_characters(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let v = self
            .service
            .list_characters(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "list_characters"))?;
        Ok(serde_json::to_value(&v)?)
    }

    async fn handle_get_account_achievements(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let summary = parse_summary(args);
        let v = self
            .service
            .get_account_achievements(&key, summary)
            .await
            .map_err(|e| annotate_endpoint(e, "get_account_achievements"))?;
        Ok(serde_json::to_value(&v)?)
    }

    async fn handle_get_account_masteries(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let v = self
            .service
            .get_account_masteries(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "get_account_masteries"))?;
        Ok(serde_json::to_value(&v)?)
    }

    async fn handle_get_account_raids(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let v = self
            .service
            .get_account_raids(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "get_account_raids"))?;
        Ok(serde_json::to_value(&v)?)
    }

    async fn handle_get_account_dungeons(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let v = self
            .service
            .get_account_dungeons(&key)
            .await
            .map_err(|e| annotate_endpoint(e, "get_account_dungeons"))?;
        Ok(serde_json::to_value(&v)?)
    }

    async fn handle_get_dailies(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let key = self.resolve_api_key(args)?;
        let which = match args.get("which").and_then(|v| v.as_str()) {
            None => DailiesWhich::Daily,
            Some(s) if s.eq_ignore_ascii_case("daily") => DailiesWhich::Daily,
            Some(s) if s.eq_ignore_ascii_case("weekly") => DailiesWhich::Weekly,
            Some(s) if s.eq_ignore_ascii_case("special") => DailiesWhich::Special,
            Some(_) => {
                return Err(CallError::BadArg {
                    name: "which",
                    expected: "\"daily\", \"weekly\", or \"special\"",
                });
            }
        };
        let d = self
            .service
            .get_dailies(&key, which)
            .await
            .map_err(|e| annotate_endpoint(e, "get_dailies"))?;
        Ok(serde_json::to_value(&d)?)
    }

    // -- Tier 6B navigation tools --------------------------------------

    async fn handle_get_my_location(&self) -> Result<serde_json::Value, CallError> {
        let snap = self
            .service
            .get_my_location()
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&snap)?)
    }

    async fn handle_get_directions(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let from = parse_location_ref(args.get("from"), "from")?;
        let to = parse_location_ref(args.get("to"), "to")?;
        let res = self
            .service
            .get_directions(from, to)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&res)?)
    }

    async fn handle_find_nearby(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let filter = parse_nearby_filter(args.get("filter"))?;
        let around = match args.get("around") {
            Some(v) if !v.is_null() => parse_location_ref(Some(v), "around")?,
            _ => crate::service::LocationRef::Here,
        };
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5usize, |n| usize::try_from(n).unwrap_or(5))
            .clamp(1, 25);
        let res = self
            .service
            .find_nearby(filter, around, limit)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&res)?)
    }

    fn handle_get_map_neighbors(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let map_id = args
            .get("map_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or(CallError::MissingArg("map_id"))?;
        let map_id = u32::try_from(map_id).map_err(|_| CallError::BadArg {
            name: "map_id",
            expected: "u32",
        })?;
        let res = self
            .service
            .get_map_neighbors(map_id)
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&res)?)
    }

    async fn handle_list_maps_in_region(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let region_arg = args.get("region").ok_or(CallError::MissingArg("region"))?;
        let query = if let Some(id) = region_arg.get("id").and_then(serde_json::Value::as_u64) {
            crate::service::RegionQuery::Id(u32::try_from(id).map_err(|_| CallError::BadArg {
                name: "region.id",
                expected: "u32",
            })?)
        } else if let Some(name) = region_arg.get("name").and_then(|v| v.as_str()) {
            crate::service::RegionQuery::Name(name.to_owned())
        } else {
            return Err(CallError::BadArg {
                name: "region",
                expected: "{\"id\": <int>} or {\"name\": <string>}",
            });
        };
        let res = self
            .service
            .list_maps_in_region(query)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&res)?)
    }

    async fn handle_describe_facing(&self) -> Result<serde_json::Value, CallError> {
        let res = self
            .service
            .describe_facing()
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&res)?)
    }

    // -- search tools (Tier 6C) ---------------------------------------

    async fn handle_search_skills(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query = parse_search_query(args)?;
        let limit = parse_search_limit(args);
        let filter = SkillSearchFilter {
            profession: parse_optional_str(args, "profession"),
            slot: parse_optional_str(args, "slot"),
            weapon_type: parse_optional_str(args, "weapon_type"),
        };
        let results = self
            .service
            .search_skills(&query, limit, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&results)?)
    }

    async fn handle_search_traits(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query = parse_search_query(args)?;
        let limit = parse_search_limit(args);
        let filter = TraitSearchFilter {
            specialization: parse_optional_u32(args, "specialization"),
            tier: parse_optional_u32(args, "tier"),
        };
        let results = self
            .service
            .search_traits(&query, limit, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&results)?)
    }

    async fn handle_search_specializations(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query = parse_search_query(args)?;
        let limit = parse_search_limit(args);
        let filter = SpecSearchFilter {
            profession: parse_optional_str(args, "profession"),
            elite: args.get("elite").and_then(serde_json::Value::as_bool),
        };
        let results = self
            .service
            .search_specializations(&query, limit, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&results)?)
    }

    async fn handle_search_items(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query = parse_search_query(args)?;
        let limit = parse_search_limit(args);
        let filter = ItemSearchFilter {
            item_type: parse_optional_str(args, "type"),
            rarity: parse_optional_str(args, "rarity"),
            min_level: parse_optional_u32(args, "min_level"),
            max_level: parse_optional_u32(args, "max_level"),
            weight_class: parse_optional_str(args, "weight_class"),
        };
        let results = self
            .service
            .search_items(&query, limit, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&results)?)
    }

    async fn handle_search_achievements(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let query = parse_search_query(args)?;
        let limit = parse_search_limit(args);
        let filter = AchievementSearchFilter {
            achievement_type: parse_optional_str(args, "type"),
        };
        let results = self
            .service
            .search_achievements(&query, limit, filter)
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&results)?)
    }

    async fn handle_get_index_status(&self) -> Result<serde_json::Value, CallError> {
        let status = self
            .service
            .get_index_status()
            .await
            .map_err(CallError::Service)?;
        Ok(serde_json::to_value(&status)?)
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
            // Single source of truth — same string is returned verbatim by
            // the `get_info` tool for clients that drop or truncate
            // `initialize.instructions`.
            instructions: Some(SERVER_RUNBOOK.to_owned()),
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

// ---------------------------------------------------------------------------
// Resources & resource templates
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `with_output_schema::<T>()` panics if `T`'s schemars-generated schema
    /// doesn't have root `type: "object"`. Make that failure show up at
    /// `cargo test` instead of at server startup.
    #[test]
    fn build_tools_constructs_without_panicking() {
        let tools = build_tools();
        assert_eq!(
            tools.len(),
            32,
            "tier-6 (a + b + c): 12 base + get_info + 7 account/coaching + 6 navigation + 6 search = 32"
        );
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
    fn server_runbook_constant_is_substantial_and_mentions_key_tools() {
        // Sanity check on the runbook constant — both `instructions` and
        // `get_info` ride on this. If someone empties or shrinks the file
        // accidentally, this catches it before MCP clients see a stub.
        assert!(SERVER_RUNBOOK.len() > 500, "runbook is too short");
        for token in [
            "get_character_build",
            "decode_build_code",
            "list_catalog_builds",
            "get_catalog_build",
            "wiki_search",
            "get_wallet",
            "https://account.arena.net/applications",
        ] {
            assert!(
                SERVER_RUNBOOK.contains(token),
                "runbook missing key token `{token}`"
            );
        }
    }

    #[test]
    fn get_info_tool_input_schema_takes_no_args() {
        let tools = build_tools();
        let info = tools
            .iter()
            .find(|t| t.name == "get_info")
            .expect("get_info tool present");
        let schema_value = serde_json::to_value(&*info.input_schema).unwrap();
        // No required fields and no additional ones either — strict empty.
        assert_eq!(schema_value["type"], "object");
        assert_eq!(schema_value["additionalProperties"], false);
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
            "get_catalog_build",
            "get_my_location",
            "get_directions",
            "describe_facing",
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
