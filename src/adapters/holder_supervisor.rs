//! macOS-only supervisor for the in-bottle Mumble Link holder.
//!
//! On startup the native gw2-mcp binary instantiates this supervisor.
//! It picks a bottle via [`bottle_discovery::pick_gw2_bottle`] — by
//! default the first one containing `Gw2-64.exe`, or whichever bottle
//! name the user pinned via `GW2_BOTTLE`. The bottle's runner
//! ([`bottle_discovery::Runner`]) decides how we launch:
//!
//! - `CrossOver` → `cxstart --bottle <name> --no-wait …`
//! - `Whisky` → bundled `wine64` binary invoked directly with
//!   `WINEPREFIX=<bottle-root>`
//!
//! Before launching, the supervisor copies the bundled
//! `gw2-mcp-holder.exe` into `<bottle>/drive_c/users/Public/gw2-mcp/holder.exe`
//! (replacing it on sha256 mismatch so a new gw2-mcp release ships an
//! updated helper transparently).
//!
//! The spawned holder is owned by this supervisor — `Drop` kills it so
//! it doesn't outlive the MCP server.
//!
//! On non-macOS platforms this module compiles to a no-op stub so
//! callers in `main.rs` don't need cfg gymnastics.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
use crate::adapters::{bottle_discovery, holder_format};

/// Default install location of Whisky's bundled wine binary. Used when
/// the supervisor decides a bottle is a Whisky bottle. Override via
/// [`HolderSupervisorOpts::whisky_wine_path`].
#[cfg(target_os = "macos")]
const DEFAULT_WHISKY_WINE_PATH: &str =
    "/Applications/Whisky.app/Contents/Resources/Libraries/Wine/bin/wine64";

/// Default install location of `cxstart`. Override via
/// [`HolderSupervisorOpts::cxstart_path`].
const DEFAULT_CXSTART_PATH: &str =
    "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/cxstart";

/// Tunable knobs for the supervisor. Defaults match documented Wine
/// wrapper install locations; the `GW2_BOTTLE` env var pins a specific
/// bottle name across both runners.
#[derive(Debug, Clone)]
pub struct HolderSupervisorOpts {
    /// Override the bottle selection. When `Some(name)`, the supervisor
    /// picks the bottle (across `CrossOver` and Whisky) whose dir name
    /// matches exactly. When `None`, auto-discovery picks the first
    /// bottle containing `Gw2-64.exe`. Sourced from `GW2_BOTTLE`.
    pub bottle_override: Option<String>,
    /// Filesystem path to the `cxstart` launcher. Used only when a
    /// `CrossOver` bottle is selected.
    pub cxstart_path: PathBuf,
    /// Filesystem path to Whisky's bundled `wine64` binary. Used only
    /// when a Whisky bottle is selected.
    pub whisky_wine_path: PathBuf,
    /// Filesystem path of the source `gw2-mcp-holder.exe` to install
    /// into the bottle. Defaults to `<exe-dir>/gw2-mcp-holder.exe` (the
    /// release tarball ships both binaries side-by-side).
    pub holder_exe_source: Option<PathBuf>,
    /// If `true`, the supervisor logs warnings on missing prerequisites
    /// (no bottle, no cxstart) and continues with a stub. If `false`,
    /// startup errors propagate up and abort the binary. Always
    /// `true` from main.rs — nav tools degrade to `NotConnected`, the
    /// rest of the MCP server keeps working.
    pub soft_failure: bool,
}

impl Default for HolderSupervisorOpts {
    fn default() -> Self {
        let bottle_override = std::env::var("GW2_BOTTLE")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            bottle_override,
            cxstart_path: PathBuf::from(DEFAULT_CXSTART_PATH),
            #[cfg(target_os = "macos")]
            whisky_wine_path: PathBuf::from(DEFAULT_WHISKY_WINE_PATH),
            // On non-macOS this field is never read but still needs a
            // value for the Default impl. Use a clearly-bogus sentinel.
            #[cfg(not(target_os = "macos"))]
            whisky_wine_path: PathBuf::from("/nonexistent/whisky-wine64"),
            holder_exe_source: None,
            soft_failure: true,
        }
    }
}

/// Owns the spawned holder child process; kills it on Drop.
#[derive(Debug)]
pub struct HolderSupervisor {
    inner: Option<Inner>,
}

#[derive(Debug)]
#[cfg(target_os = "macos")]
struct Inner {
    child: std::process::Child,
}

#[derive(Debug)]
#[cfg(not(target_os = "macos"))]
struct Inner;

impl HolderSupervisor {
    /// No-op supervisor used when we deliberately don't want to manage a
    /// holder (e.g. user passed `--no-mumble-holder`, or platform isn't
    /// macOS). Drop is a no-op.
    #[must_use]
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Spawn the in-bottle holder. On non-macOS targets this returns
    /// `Self::disabled()` immediately — Linux + Windows MCP servers can
    /// reach Mumble Link directly and don't need a helper.
    #[cfg(not(target_os = "macos"))]
    pub fn spawn(_opts: HolderSupervisorOpts) -> Self {
        Self::disabled()
    }

    /// Spawn the in-bottle holder. Pure-best-effort: every failure mode
    /// is logged and downgrades to a disabled supervisor when
    /// `soft_failure = true`. Otherwise the error propagates.
    #[cfg(target_os = "macos")]
    pub fn spawn(opts: HolderSupervisorOpts) -> Self {
        match spawn_macos(&opts) {
            Ok(SpawnResult { child, bottle }) => {
                tracing::info!(
                    bottle = %bottle.name,
                    runner = bottle.runner.as_str(),
                    pid = child.id(),
                    "in-bottle Mumble Link holder spawned"
                );
                Self {
                    inner: Some(Inner { child }),
                }
            }
            Err(e) if opts.soft_failure => {
                // ?e (Debug) preserves the anyhow source chain in the
                // log so users see the full reason, not just the outermost
                // wrapper. Per CLAUDE.rust.md error-logging rules.
                tracing::warn!(
                    error = ?e,
                    bottle_override = ?opts.bottle_override,
                    "could not start in-bottle Mumble Link holder; navigation tools \
                     will return NotConnected until the holder is reachable. \
                     Run `gw2-mcp doctor` to diagnose."
                );
                Self::disabled()
            }
            Err(e) => {
                // Hard-failure path; only used by tests / explicit callers.
                panic!("holder supervisor: {e}");
            }
        }
    }

    /// Returns `true` if a child process was spawned and is being managed
    /// by this supervisor (informational; the holder lifetime is tied to
    /// the supervisor's `Drop`).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }
}

impl Drop for HolderSupervisor {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(mut inner) = self.inner.take() {
            // SIGTERM via std::process::Child::kill (sends SIGKILL on Unix
            // — we accept that; the holder writes idempotent files and
            // the OS drops the named mapping when the process dies).
            if let Err(e) = inner.child.kill() {
                tracing::warn!(error = ?e, "failed to terminate holder child");
            }
            // Reap the child to avoid a zombie. We ignore the wait result
            // intentionally: by the time we reach this branch we've
            // already killed the process, so the only thing wait() tells
            // us is "yes, dead" — which we already know.
            let _ = inner.child.wait();
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.inner = None;
        }
    }
}

/// What [`spawn_macos`] returns on success: the running child plus the
/// bottle we resolved (so the caller can log the runner and name).
#[cfg(target_os = "macos")]
struct SpawnResult {
    child: std::process::Child,
    bottle: bottle_discovery::Bottle,
}

#[cfg(target_os = "macos")]
fn spawn_macos(opts: &HolderSupervisorOpts) -> anyhow::Result<SpawnResult> {
    let bottle = bottle_discovery::pick_gw2_bottle(opts.bottle_override.as_deref())
        .ok_or_else(|| no_bottle_error(opts))?;

    let dest_dir = bottle
        .root
        .join("drive_c")
        .join(holder_format::HOLDER_SUBDIR);
    std::fs::create_dir_all(&dest_dir)?;
    let dest_exe = dest_dir.join(holder_format::HOLDER_EXE_NAME);

    let source = locate_holder_source(opts.holder_exe_source.as_deref())?;
    install_if_changed(&source, &dest_exe)?;

    // Mirror file path in Windows convention. `users/Public` is host-
    // visible at <bottle>/drive_c/users/Public/.
    let win_subdir = holder_format::HOLDER_SUBDIR.replace('/', "\\");
    let bin_path_win = format!("C:\\{}\\{}", win_subdir, holder_format::HOLDER_BIN_NAME);
    let exe_path_win = format!("C:\\{}\\{}", win_subdir, holder_format::HOLDER_EXE_NAME);

    tracing::debug!(
        bottle = %bottle.name,
        runner = bottle.runner.as_str(),
        exe = %dest_exe.display(),
        bin_path = %bin_path_win,
        "launching holder"
    );

    let child = match bottle.runner {
        bottle_discovery::Runner::CrossOver => spawn_crossover(
            &opts.cxstart_path,
            &bottle.name,
            &exe_path_win,
            &bin_path_win,
        )?,
        bottle_discovery::Runner::Whisky => spawn_whisky(
            &opts.whisky_wine_path,
            &bottle.root,
            &dest_exe,
            &bin_path_win,
        )?,
    };
    Ok(SpawnResult { child, bottle })
}

/// Build a helpful error when bottle selection fails. Lists what we
/// did see so the user knows whether the issue is "wrong env var" vs
/// "no bottles installed at all".
#[cfg(target_os = "macos")]
fn no_bottle_error(opts: &HolderSupervisorOpts) -> anyhow::Error {
    let bottles = bottle_discovery::discover_bottles();
    let names: Vec<String> = bottles
        .iter()
        .map(|b| format!("{} ({})", b.name, b.runner.as_str()))
        .collect();
    match (opts.bottle_override.as_deref(), names.as_slice()) {
        (Some(name), []) => anyhow::anyhow!(
            "GW2_BOTTLE={name:?} set, but no CrossOver/Whisky bottles found at all. \
             Install Guild Wars 2 in a bottle, or unset GW2_BOTTLE."
        ),
        (Some(name), available) => anyhow::anyhow!(
            "GW2_BOTTLE={name:?} did not match any bottle. Available: {available:?}"
        ),
        (None, []) => anyhow::anyhow!(
            "no CrossOver or Whisky bottles found. Install Guild Wars 2 in a \
             bottle, or run `gw2-mcp doctor` to diagnose."
        ),
        (None, available) => anyhow::anyhow!(
            "no bottle contains Gw2-64.exe at the standard install path. \
             Available bottles: {available:?}. If GW2 is installed in one of \
             them at a non-standard path, set GW2_BOTTLE=<name> to pin it."
        ),
    }
}

#[cfg(target_os = "macos")]
fn spawn_crossover(
    cxstart_path: &Path,
    bottle_name: &str,
    exe_path_win: &str,
    bin_path_win: &str,
) -> anyhow::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    if !cxstart_path.exists() {
        anyhow::bail!(
            "cxstart not found at {} — is CrossOver installed?",
            cxstart_path.display()
        );
    }
    // --no-wait so cxstart returns control immediately (the holder runs
    // for the lifetime of gw2-mcp). Our Drop handler kills the child by
    // PID, which works because cxstart's exec child is the wine process
    // that becomes the parent of holder.exe — killing the immediate
    // child still triggers the wineserver-side cleanup.
    let child = Command::new(cxstart_path)
        .arg("--bottle")
        .arg(bottle_name)
        .arg("--no-wait")
        .arg(exe_path_win)
        .arg("--bin-path")
        .arg(bin_path_win)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

#[cfg(target_os = "macos")]
fn spawn_whisky(
    wine_path: &Path,
    bottle_root: &Path,
    holder_host_path: &Path,
    bin_path_win: &str,
) -> anyhow::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    if !wine_path.exists() {
        anyhow::bail!(
            "Whisky's bundled wine64 not found at {} — is Whisky installed? \
             (Override with HolderSupervisorOpts::whisky_wine_path.)",
            wine_path.display()
        );
    }
    // Whisky bottles don't have a `cxstart`-like wrapper; we invoke the
    // bundled wine64 directly. WINEPREFIX points at the bottle root;
    // wine64 takes the .exe as a host path (it auto-translates).
    let child = Command::new(wine_path)
        .env("WINEPREFIX", bottle_root)
        // Suppress fixup-on-first-launch chatter when possible. Wine
        // still writes some diagnostics to stderr; we inherit it so
        // they end up in the same stream as gw2-mcp logs.
        .env("WINEDEBUG", "-all")
        .arg(holder_host_path)
        .arg("--bin-path")
        .arg(bin_path_win)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

/// Find the bundled `gw2-mcp-holder.exe`. Strategy:
/// 1. Explicit override (test / advanced user).
/// 2. Sibling of the running executable.
/// 3. `target/.../release/gw2-mcp-holder.exe` for `cargo run` dev loops.
#[cfg(target_os = "macos")]
fn locate_holder_source(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        anyhow::bail!("holder_exe_source override does not exist: {}", p.display());
    }
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current exe has no parent"))?;
    let sibling = dir.join("gw2-mcp-holder.exe");
    if sibling.exists() {
        return Ok(sibling);
    }
    // Dev fallback: target/x86_64-pc-windows-gnu/release/gw2-mcp-holder.exe
    if let Some(workspace) = dir.parent().and_then(|p| p.parent()) {
        let candidate = workspace
            .join("x86_64-pc-windows-gnu")
            .join("release")
            .join("gw2-mcp-holder.exe");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not find gw2-mcp-holder.exe next to {} (or in the cargo target dir). \
         Reinstall gw2-mcp from the release tarball, which ships both binaries together.",
        exe.display()
    );
}

#[cfg(target_os = "macos")]
fn install_if_changed(source: &Path, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() && file_sha256(source)? == file_sha256(dest)? {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, dest)?;
    tracing::info!(
        source = %source.display(),
        dest = %dest.display(),
        "installed gw2-mcp-holder.exe into bottle"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn file_sha256(path: &Path) -> anyhow::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_supervisor_drops_cleanly() {
        let s = HolderSupervisor::disabled();
        assert!(!s.is_active());
        drop(s);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_bottle_override_yields_disabled_supervisor() {
        // Soft failure path — naming a nonexistent bottle should log
        // a warning and return a disabled supervisor, not panic.
        let opts = HolderSupervisorOpts {
            bottle_override: Some("DefinitelyDoesNotExist-zzz".to_owned()),
            cxstart_path: PathBuf::from("/no/such/cxstart"),
            whisky_wine_path: PathBuf::from("/no/such/wine64"),
            holder_exe_source: None,
            soft_failure: true,
        };
        let s = HolderSupervisor::spawn(opts);
        assert!(!s.is_active());
    }
}
