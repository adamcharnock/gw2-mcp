//! Binary entrypoint. Wires concrete adapters into a [`Service`] and serves
//! it over MCP/stdio. This is the *only* place that picks adapters — every
//! other module sees only ports.

use std::sync::Arc;

use clap::Parser;
use gw2_mcp::adapters::{
    ChatrDecoder, DiscretizeCatalog, HttpGw2Api, HttpWiki, McpServer, MemoryCache,
    MetaBattleCatalog, SnowCrowsCatalog, SystemClock,
};
use gw2_mcp::domain::ApiKey;
use gw2_mcp::ports::{BuildCatalog, BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, Wiki};
use gw2_mcp::service::Service;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "gw2-mcp", version, about = "Guild Wars 2 MCP server (stdio).")]
struct Cli {
    /// Override the GW2 API base URL (useful for tests / proxies).
    #[arg(long, env = "GW2_API_BASE_URL")]
    gw2_api_url: Option<String>,

    /// Override the GW2 wiki API URL.
    #[arg(long, env = "GW2_WIKI_API_URL")]
    wiki_api_url: Option<String>,

    /// Default Guild Wars 2 API key. When set, MCP tools that take an
    /// `api_key` argument (`get_wallet`, `get_character_build`) fall back
    /// to this value if the caller omits the argument. Per-call args still
    /// override. The flag is hidden from `--help` env display so it doesn't
    /// echo the secret on machines where help is captured into logs.
    #[arg(long, env = "GW2_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Tracing filter (e.g. `debug` or `gw2_mcp=debug,reqwest=info`).
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // MCP uses stdout for the protocol. Logs go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(clock.clone()));

    let gw2: Arc<dyn Gw2Api> = match cli.gw2_api_url {
        Some(url) => Arc::new(HttpGw2Api::with_base_url(url)?),
        None => Arc::new(HttpGw2Api::new()?),
    };
    let wiki: Arc<dyn Wiki> = match cli.wiki_api_url {
        Some(url) => Arc::new(HttpWiki::with_base_url(url)?),
        None => Arc::new(HttpWiki::new()?),
    };

    let build_decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);

    let discretize: Arc<dyn BuildCatalog> = Arc::new(DiscretizeCatalog::new()?);
    let metabattle: Arc<dyn BuildCatalog> = Arc::new(MetaBattleCatalog::new()?);
    let snowcrows: Arc<dyn BuildCatalog> = Arc::new(SnowCrowsCatalog::new()?);
    let catalogs = Arc::new(
        CatalogRegistry::new()
            .with(discretize)
            .with(metabattle)
            .with(snowcrows),
    );

    // Default API key from --api-key / GW2_API_KEY env var, validated up
    // front so misconfiguration fails the binary instead of every tool call.
    // Failed validation is logged and treated as "no default" — the tools
    // still work when the caller passes `api_key` explicitly.
    let default_api_key = match cli.api_key {
        Some(raw) if !raw.trim().is_empty() => match ApiKey::new(raw) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring GW2_API_KEY: validation failed");
                None
            }
        },
        _ => None,
    };

    let mut service = Service::new(gw2, wiki, cache, clock, build_decoder, catalogs);
    if let Some(k) = default_api_key {
        service = service.with_default_api_key(k);
    }
    let server = McpServer::new(service);

    tracing::info!("starting gw2-mcp server (stdio)");
    server.serve_stdio().await?;
    tracing::info!("gw2-mcp server exited cleanly");
    Ok(())
}
