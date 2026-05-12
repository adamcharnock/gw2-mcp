//! Tracing setup with two sinks: stderr (where it has always gone) and a
//! per-invocation file in a platform-standard logs directory.
//!
//! Motivation: when an MCP client (Claude Desktop / Claude Code) reports
//! "Failed to reconnect to gw2", the stderr stream from the server has
//! already vanished into the client's process — no way to see why. A
//! file-side log captures every run unconditionally so the user can
//! tail the most recent one and figure out what broke.
//!
//! Each gw2-mcp invocation opens its own file. Files share a directory
//! but not handles — concurrent invocations don't interleave lines.
//! A `gw2-mcp-latest.log` symlink (Unix only) always points at the
//! current run's file so `tail -f $(symlink)` "just works".
//!
//! Defaults:
//! - macOS:   `~/Library/Logs/gw2-mcp/`         (Console.app indexes this)
//! - Linux:   `$XDG_STATE_HOME/gw2-mcp/logs/`   (or `~/.local/state/...`)
//! - Windows: `%LOCALAPPDATA%\gw2-mcp\logs\`    (via `ProjectDirs::data_local_dir`)
//! - Fallback: `$cache_dir/logs/` if the platform-preferred path can't
//!   be resolved (e.g. `directories` returns None for `state_dir`).
//!
//! Both `--log-dir <path>` and `GW2_LOG_DIR=<path>` override the default;
//! `--no-file-log` (or `GW2_NO_FILE_LOG=1`) disables file logging entirely
//! and leaves only stderr. File logging never aborts startup — a write
//! permission error degrades to stderr-only with a warning.

use std::path::{Path, PathBuf};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// Boxed, type-erased layer over the root `Registry` subscriber. We
/// collect both the stderr and (optional) file layers into a `Vec` of
/// these so they share one concrete type — without erasure, the
/// stderr-only and stderr+file shapes are different and the
/// `.init()` call site fails to type-check.
type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// Outcome of `init`. The caller holds onto this for the lifetime of the
/// process so the `WorkerGuard`-less file appender stays open. The
/// `log_path` is exposed so `main` can log it (visible in both stderr
/// and the file itself).
pub struct LoggingInit {
    pub log_path: Option<PathBuf>,
}

/// Set up tracing with stderr + optional per-invocation file sink.
///
/// `filter` is the `RUST_LOG`-style filter expression. `log_dir_override`
/// forces the file into a specific directory; pass `None` to use the
/// platform default. `disable_file` skips file logging entirely (stderr
/// only) — useful for CI / tests / paranoid offline runs.
///
/// Errors only on truly unrecoverable cases (the stderr subscriber itself
/// fails to install). File-side problems degrade silently with a stderr
/// warning so a read-only filesystem doesn't break the server.
pub fn init(
    filter: &str,
    log_dir_override: Option<&Path>,
    disable_file: bool,
) -> anyhow::Result<LoggingInit> {
    let make_filter = || EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers: Vec<BoxedLayer> = vec![
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_filter(make_filter())
            .boxed(),
    ];

    // Try to open the file appender. If anything fails, warn on stderr
    // and continue with stderr-only logging — file logging is a
    // convenience, not load-bearing.
    let log_path = if disable_file {
        None
    } else {
        match open_file_layer(log_dir_override, make_filter()) {
            Ok((layer, path)) => {
                layers.push(layer);
                Some(path)
            }
            Err(e) => {
                eprintln!("gw2-mcp: file logging disabled: {e:#}");
                None
            }
        }
    };

    tracing_subscriber::registry().with(layers).init();

    Ok(LoggingInit { log_path })
}

/// Open the per-invocation log file inside the resolved logs directory
/// (or `log_dir_override` if set), and return a boxed fmt-Layer wrapping
/// it together with the path that was opened.
fn open_file_layer(
    log_dir_override: Option<&Path>,
    filter: EnvFilter,
) -> anyhow::Result<(BoxedLayer, PathBuf)> {
    let dir = match log_dir_override {
        Some(d) => d.to_path_buf(),
        None => default_log_dir()?,
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create log dir {}: {e}", dir.display()))?;

    let filename = log_filename();
    let full_path = dir.join(&filename);

    // tracing-appender's `never` rotation opens the named file in the
    // given dir and returns a `RollingFileAppender` that impls
    // `MakeWriter`. Blocking writes are fine — log volume is < a few
    // lines per second.
    let appender = tracing_appender::rolling::never(&dir, &filename);
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(appender)
        .with_ansi(false)
        .with_filter(filter)
        .boxed();

    // Best-effort: maintain a `gw2-mcp-latest.log` symlink (Unix only).
    // If symlinking fails — e.g. on a filesystem that doesn't support
    // them, or on Windows where we'd need elevated privileges — we drop
    // the convenience but keep going. The user can still find the file
    // via the path we log at startup.
    update_latest_symlink(&dir, &filename);

    Ok((layer, full_path))
}

/// Per-platform default log directory. Falls back to
/// `cache_dir().join("logs")` when the platform-preferred state directory
/// isn't resolvable (e.g. `directories` returns None for `state_dir` on
/// non-Linux platforms — that's by design, we route those elsewhere).
fn default_log_dir() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        // macOS convention: ~/Library/Logs/<app>/. Console.app indexes
        // this location automatically, so users get a graphical log
        // viewer for free.
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow::anyhow!("HOME environment variable unset"))?;
        return Ok(PathBuf::from(home).join("Library/Logs/gw2-mcp"));
    }
    let proj = directories::ProjectDirs::from("net", "adamcharnock", "gw2-mcp")
        .ok_or_else(|| anyhow::anyhow!("no project directories available for this platform"))?;
    // Linux: state_dir() returns Some (~/.local/state/gw2-mcp). Other
    // platforms get None and we fall back to cache_dir/logs.
    if let Some(state) = proj.state_dir() {
        Ok(state.join("logs"))
    } else {
        Ok(proj.cache_dir().join("logs"))
    }
}

/// `gw2-mcp-YYYYMMDD-HHMMSS-<pid>.log`. The timestamp + pid combination
/// is unique even for concurrent invocations within the same second.
fn log_filename() -> String {
    let now = chrono::Utc::now();
    let pid = std::process::id();
    format!("gw2-mcp-{}-{pid}.log", now.format("%Y%m%d-%H%M%S"))
}

/// Refresh the `gw2-mcp-latest.log` symlink so `tail -f` works against a
/// stable name. Silently no-ops on Windows (and on any FS error) — this
/// is a convenience, not a correctness requirement.
fn update_latest_symlink(dir: &Path, target_filename: &str) {
    #[cfg(unix)]
    {
        let latest = dir.join("gw2-mcp-latest.log");
        // Remove the previous symlink (or file) if it exists — symlink()
        // fails if the destination already exists.
        let _ = std::fs::remove_file(&latest);
        let _ = std::os::unix::fs::symlink(target_filename, &latest);
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, target_filename);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filename_has_expected_shape() {
        let name = log_filename();
        assert!(name.starts_with("gw2-mcp-"), "got: {name}");
        assert!(
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log")),
            "got: {name}"
        );
        // gw2-mcp-YYYYMMDD-HHMMSS-PID.log: 4 hyphen-separated segments.
        assert_eq!(name.matches('-').count(), 4, "got: {name}");
    }
}
