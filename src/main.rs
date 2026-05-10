//! Binary entrypoint. Wires concrete adapters into a [`Service`] and serves
//! it over MCP/stdio. This is the *only* place that picks adapters — every
//! other module sees only ports.

use std::sync::Arc;

use clap::Parser;
use gw2_mcp::adapters::{HttpGw2Api, HttpWiki, McpServer, MemoryCache, SystemClock};
use gw2_mcp::ports::{Cache, Clock, Gw2Api, Wiki};
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

    let service = Service::new(gw2, wiki, cache, clock);
    let server = McpServer::new(service);

    tracing::info!("starting gw2-mcp server (stdio)");
    server.serve_stdio().await?;
    tracing::info!("gw2-mcp server exited cleanly");
    Ok(())
}
