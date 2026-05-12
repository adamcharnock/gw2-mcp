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
    MirrorRescuer, SnowCrowsCatalog, SqliteSearchIndex, SystemClock,
};
use gw2_mcp::cli::{doctor, print_config};
use gw2_mcp::domain::ApiKey;
use gw2_mcp::indexing::{IndexingOpts, IndexingPipeline};
use gw2_mcp::ports::{
    BuildCatalog, BuildCodeDecoder, Cache, CatalogRegistry, Clock, Gw2Api, MapData, MumbleLink,
    SearchIndex, Wiki,
};
use gw2_mcp::service::Service;

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

    /// Override the log directory for the per-invocation file log. By
    /// default uses a platform-standard location:
    /// `~/Library/Logs/gw2-mcp/` on macOS, `~/.local/state/gw2-mcp/logs/`
    /// on Linux, `%LOCALAPPDATA%\gw2-mcp\logs\` on Windows.
    #[arg(long, env = "GW2_LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Disable per-invocation file logging entirely. stderr logging is
    /// unchanged. Useful for CI / tests / read-only filesystems.
    #[arg(long, env = "GW2_NO_FILE_LOG", default_value_t = false)]
    no_file_log: bool,

    /// Disable the Mumble Link reader. Useful for headless / CI
    /// deployments where GW2 isn't running on the same host. The
    /// navigation tools (`get_my_location`, `describe_facing`,
    /// `find_nearby` with `here`, `get_directions` with `here`) will
    /// return a clear "not supported" error; everything else keeps
    /// working.
    #[arg(long, env = "GW2_NO_MUMBLE_LINK", default_value_t = false)]
    no_mumble_link: bool,

    /// (macOS only) Skip auto-spawning the in-bottle Mumble Link holder
    /// (`gw2-mcp-holder.exe` via cxstart). Use this if you're not running
    /// GW2 in `CrossOver`, or if you're managing the holder yourself.
    /// Has no effect on Linux/Windows where Mumble Link is read directly.
    #[arg(long, env = "GW2_NO_MUMBLE_HOLDER", default_value_t = false)]
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

    // MCP uses stdout for the protocol. Logs go to stderr AND a
    // per-invocation file under a platform-standard logs directory so
    // failed-connect debugging doesn't depend on the MCP client's
    // stderr capture (which is often discarded silently).
    let logging_init = gw2_mcp::logging::init(&cli.log, cli.log_dir.as_deref(), cli.no_file_log)?;
    if let Some(path) = &logging_init.log_path {
        tracing::info!(log_file = %path.display(), "per-invocation log file opened");
    }

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
    // owns the child process — when its last clone drops at process exit,
    // the holder dies with it. On Linux/Windows this is a no-op.
    //
    // Concurrent gw2-mcp instances against the same bottle coordinate via
    // an advisory flock on a per-bottle lockfile: whoever wins it owns
    // the holder; everyone else just reads the shared mirror. If the
    // leader exits, the next reader to see a stale mirror promotes
    // itself via the rescuer wired into FileMumbleLink below.
    let holder_supervisor: HolderSupervisor = if cli.no_mumble_link || cli.no_mumble_holder {
        HolderSupervisor::disabled()
    } else {
        // If we're running from a cargo workspace (e.g. `cargo run` in
        // this repo), cross-build gw2-mcp-holder.exe on demand so the
        // supervisor doesn't need a manually-built sibling binary. No-op
        // on Linux/Windows and outside dev workspaces. Toolchain
        // failures here are not soft — print the install hint and exit
        // so the contributor fixes the prereq before testing nav tools.
        let holder_exe_source = match gw2_mcp::dev_build::ensure_holder_built_if_dev() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: gw2-mcp-holder.exe auto-build: {e}");
                std::process::exit(1);
            }
        };
        let opts = HolderSupervisorOpts {
            holder_exe_source,
            ..HolderSupervisorOpts::default()
        };
        HolderSupervisor::spawn(opts)
    };

    // Mumble Link adapter — auto-probe unless --no-mumble-link is set.
    // probe_default never fails: it returns a stub on missing region.
    // The supervisor is also the MirrorRescuer the reader consults on
    // stale-mirror detection (macOS only — on Linux/Windows the rescuer
    // is unused even when passed).
    let rescuer: Option<Arc<dyn MirrorRescuer>> =
        Some(Arc::new(holder_supervisor.clone()) as Arc<dyn MirrorRescuer>);
    let mumble: Arc<dyn MumbleLink> = probe_default(cli.no_mumble_link, rescuer);

    // Diagnostic log at startup. When the in-bottle supervisor is active
    // (macOS + bottle detected), the helper has only just been spawned
    // and Wine hasn't yet had time to load it — any snapshot we take
    // here would read a stale leftover mirror file from a previous
    // session and report a misleading "helper appears to have crashed"
    // message. The supervisor's own log line is the proof of life;
    // skip the redundant probe here and let nav tools read live state
    // on demand. For non-macOS hosts (or when the supervisor is
    // disabled) we keep the snapshot as the primary startup diagnostic
    // for "did /dev/shm/MumbleLink / the Windows named mapping show up?".
    if holder_supervisor.is_active() {
        tracing::info!(
            "mumble link adapter wired via in-bottle helper; nav tools will read live state on demand"
        );
    } else {
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
    //
    // We hold on to the background indexer's `JoinHandle` so we can
    // `.abort()` it on shutdown. Without this, SIGTERM aborts the
    // indexer at an arbitrary point during the runtime's teardown —
    // safe today thanks to SQLite-WAL durability, but fragile to future
    // changes that introduce multi-statement transactions outside an
    // explicit `BEGIN`.
    let mut indexer_handle: Option<tokio::task::JoinHandle<()>> = None;
    let search_enabled = !cli.no_search_index;
    if search_enabled {
        match resolve_index_path(cli.cache_dir.as_deref()) {
            Ok(path) => match SqliteSearchIndex::open(&path).await {
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
                    indexer_handle = Some(pipeline.spawn_background());
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

    // We race the MCP serve loop against signal delivery so SIGINT/SIGTERM
    // cause a clean return from `main` instead of a hard process exit. The
    // clean return is what lets `holder_supervisor`'s Drop fire (and
    // therefore terminate the in-bottle holder). Without this, killing
    // gw2-mcp from a terminal would leave holder.exe orphaned until the
    // next gw2-mcp startup's orphan-sweep ran.
    tracing::info!("starting gw2-mcp server (stdio)");
    tokio::select! {
        res = server.serve_stdio() => res?,
        sig = shutdown_signal() => {
            tracing::info!(signal = sig, "shutdown signal received");
        }
    }
    // Abort the indexer before returning. Each indexing step writes to
    // SQLite-WAL synchronously and stamps the build number only after a
    // kind fully commits, so aborting mid-kind is safe — the kind will
    // simply re-run on next startup.
    if let Some(handle) = indexer_handle {
        handle.abort();
    }
    tracing::info!("gw2-mcp server exited cleanly");
    Ok(ExitCode::SUCCESS)
}

/// Await either SIGINT (Ctrl-C) or SIGTERM (`kill <pid>`) and report
/// which one fired. On non-Unix targets we fall back to Ctrl-C only.
/// SIGKILL is uncatchable by definition and bypasses this entirely —
/// orphan-cleanup at the next startup handles that case.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // .expect: SIGTERM/SIGINT handler installation only fails when
        // tokio's signal runtime isn't initialized, which we control.
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
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
