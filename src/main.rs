//! Binary entrypoint. Wires concrete adapters into a [`Service`] and serves
//! it over MCP/stdio. This is the *only* place that picks adapters — every
//! other module sees only ports.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gw2_mcp::adapters::probe_default;
use gw2_mcp::adapters::{
    ChatrDecoder, DiscretizeCatalog, HolderSupervisor, HolderSupervisorOpts, HttpGw2Api,
    HttpMapData, HttpWiki, INDEX_FILE_NAME, McpServer, MemoryCache, MetaBattleCatalog,
    SnowCrowsCatalog, SqliteSearchIndex, SystemClock,
};
use gw2_mcp::cli::{doctor, print_config};
use gw2_mcp::domain::ApiKey;
use gw2_mcp::indexing::{IndexingOpts, IndexingPipeline};
use gw2_mcp::ports::{
    BuildCatalog, BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, MapData, MumbleLink,
    SearchIndex, Wiki,
};
use gw2_mcp::service::Service;
use tracing_subscriber::EnvFilter;

/// Top-level subcommands. Default (none) runs the MCP server over stdio
/// — the historical entrypoint. `doctor` and `print-config` are
/// non-server utilities that print to stdout and exit.
#[derive(Subcommand, Debug)]
enum Command {
    /// Run platform diagnostics for Mumble Link on macOS. Prints a
    /// structured report and exits non-zero if any check fails.
    Doctor,
    /// Print a Claude Desktop config snippet for this binary, ready
    /// to paste into `claude_desktop_config.json`.
    #[command(name = "print-config")]
    PrintConfig {
        /// Inject `GW2_API_KEY=<key>` into the snippet's env block.
        #[arg(long)]
        api_key: Option<String>,
        /// Inject `GW2_BOTTLE=<name>` into the snippet's env block —
        /// pins the bottle name when CrossOver/Whisky auto-discovery
        /// would pick the wrong one.
        #[arg(long)]
        bottle: Option<String>,
    },
}

// Cli boolean flags are clap-style on/off switches; they don't represent
// state transitions and refactoring to an enum buys nothing.
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug)]
#[command(name = "gw2-mcp", version, about = "Guild Wars 2 MCP server (stdio).")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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

    /// Disable the Mumble Link reader. Useful for headless/Docker
    /// deployments where GW2 isn't running on the same host. The
    /// navigation tools (`get_my_location`, `describe_facing`,
    /// `find_nearby` with `here`, `get_directions` with `here`) will
    /// return a clear "not supported" error; everything else keeps
    /// working.
    #[arg(long, default_value_t = false)]
    no_mumble_link: bool,

    /// (macOS only) Skip auto-spawning the in-bottle Mumble Link holder
    /// (`gw2-mcp-holder.exe` via cxstart). Use this if you're not running
    /// GW2 in `CrossOver`, or if you're managing the holder yourself.
    /// Has no effect on Linux/Windows where Mumble Link is read directly.
    #[arg(long, default_value_t = false)]
    no_mumble_holder: bool,

    /// Override the cache directory for the on-disk search index. By
    /// default the OS-standard cache dir is used (e.g.
    /// `~/Library/Caches/net.adamcharnock.gw2-mcp/` on macOS or
    /// `~/.cache/gw2-mcp/` on Linux). The actual `SQLite` file is named
    /// `index.sqlite` inside this directory.
    #[arg(long, env = "GW2_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Disable the on-disk search index entirely. The `search_*` tools will
    /// return a "search disabled" error. Useful for ephemeral / read-only
    /// environments where you don't want to write to disk.
    #[arg(long, env = "GW2_NO_SEARCH_INDEX", default_value_t = false)]
    no_search_index: bool,

    /// Include items in the background indexing pass. Heavy: ~85k entries,
    /// ~5 minutes of API calls, ~50 MB on disk. Off by default; opt in
    /// when you actually want item search.
    #[arg(long, env = "GW2_WITH_ITEMS", default_value_t = false)]
    with_items: bool,

    /// Force a full re-index on startup even when the cached build number
    /// matches the live `/v2/build`. Useful after schema changes or to
    /// rebuild a corrupt index.
    #[arg(long, env = "GW2_REBUILD_INDEX", default_value_t = false)]
    rebuild_index: bool,
}

// main() is the *only* place that picks adapters and wires them into the
// service. Splitting it into helpers fragments that picture and creates
// argument-passing noise without buying clarity. Keep it linear.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();

    // Subcommands short-circuit the server path. They print to stdout
    // and exit — no MCP stdio, no logging setup beyond what they need.
    if let Some(cmd) = cli.command {
        return Ok(match cmd {
            Command::Doctor => doctor::run(),
            Command::PrintConfig { api_key, bottle } => print_config::run(print_config::Args {
                api_key,
                bottle,
                binary_path: None,
            }),
        });
    }

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

    // On macOS, GW2 runs in CrossOver and Mumble Link only publishes when
    // something pre-creates the named mapping. We launch an in-bottle
    // holder via cxstart that does so and mirrors snapshots out to a
    // host-visible file the FileMumbleLink reader picks up. The supervisor
    // owns the child process — when it drops at process exit, the holder
    // dies with it. On Linux/Windows this is a no-op.
    let _holder_supervisor: HolderSupervisor = if cli.no_mumble_link || cli.no_mumble_holder {
        HolderSupervisor::disabled()
    } else {
        HolderSupervisor::spawn(HolderSupervisorOpts::default())
    };

    // Mumble Link adapter — auto-probe unless --no-mumble-link is set.
    // probe_default never fails: it returns a stub on missing region.
    let mumble: Arc<dyn MumbleLink> = probe_default(cli.no_mumble_link);
    match mumble.snapshot() {
        Ok(snap) => tracing::info!(
            ui_tick = snap.ui_tick,
            map_id = snap.context.map_id,
            "mumble link adapter: connected"
        ),
        Err(e) => tracing::info!(
            error = %e,
            "mumble link adapter: stub (navigation tools will return this error until GW2 is running on the same host)"
        ),
    }

    let maps: Arc<dyn MapData> = Arc::new(HttpMapData::new()?);

    let mut service = Service::new(
        gw2.clone(),
        wiki,
        cache,
        clock,
        build_decoder,
        catalogs,
        mumble,
        maps,
    );
    if let Some(k) = default_api_key {
        service = service.with_default_api_key(k);
    }

    // Wire the on-disk search index unless the user opted out. Failing to
    // open is logged and the server continues without search — the binary
    // remains useful for non-search tools (get_*, wiki, catalogs).
    let search_enabled = !cli.no_search_index;
    if search_enabled {
        match resolve_index_path(cli.cache_dir.as_deref()) {
            Ok(path) => match SqliteSearchIndex::open(&path) {
                Ok(idx_concrete) => {
                    tracing::info!(path = %path.display(), "search index opened");
                    let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
                    service = service.with_search_index(idx.clone());
                    let pipeline = IndexingPipeline::new(
                        gw2.clone(),
                        idx,
                        IndexingOpts {
                            include_items: cli.with_items,
                            force_rebuild: cli.rebuild_index,
                        },
                    );
                    let _handle = pipeline.spawn_background();
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "failed to open search index; running without it");
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "could not resolve cache directory; running without search index");
            }
        }
    } else {
        tracing::info!("--no-search-index set; search tools will return SearchDisabled");
    }

    let server = McpServer::new(service);

    tracing::info!("starting gw2-mcp server (stdio)");
    server.serve_stdio().await?;
    tracing::info!("gw2-mcp server exited cleanly");
    Ok(ExitCode::SUCCESS)
}

/// Resolve the on-disk path of the search index file from the optional
/// `--cache-dir` override. Falls back to the OS-standard cache directory
/// reported by the `directories` crate. Returns an error only when neither
/// is available (rare — would mean `$HOME` is unset on a Unix system).
fn resolve_index_path(override_dir: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir.join(INDEX_FILE_NAME));
    }
    let proj = directories::ProjectDirs::from("net", "adamcharnock", "gw2-mcp")
        .ok_or_else(|| anyhow::anyhow!("no OS cache directory available"))?;
    Ok(proj.cache_dir().join(INDEX_FILE_NAME))
}
