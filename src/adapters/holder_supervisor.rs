//! macOS-only supervisor for the in-bottle Mumble Link holder.
//!
//! On startup the native gw2-mcp binary instantiates this supervisor.
//! It detects the user's `CrossOver` bottle (env override or default
//! `Guild Wars 2` name), copies the bundled `gw2-mcp-holder.exe` into
//! `<bottle>/drive_c/users/Public/gw2-mcp/holder.exe` (replacing it on
//! sha256 mismatch so a new gw2-mcp release ships an updated holder
//! transparently), then spawns it via `cxstart`.
//!
//! The spawned holder is owned by this supervisor — `Drop` sends it
//! `SIGTERM` so it doesn't outlive the MCP server.
//!
//! On non-macOS platforms this module compiles to a no-op stub so
//! callers in `main.rs` don't need cfg gymnastics.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
use crate::adapters::holder_format;

/// Tunable knobs for the supervisor. Defaults match the documented
/// `CrossOver` convention (`~/Library/Application Support/CrossOver/Bottles/Guild Wars 2`)
/// and `cxstart` install location. The `GW2_BOTTLE` env var overrides
/// the bottle name.
#[derive(Debug, Clone)]
pub struct HolderSupervisorOpts {
    /// Bottle name to look up under `~/Library/Application Support/CrossOver/Bottles/`.
    /// Override via `GW2_BOTTLE` env var.
    pub bottle_name: String,
    /// Filesystem path to the cxstart launcher. Defaults to the standard
    /// `CrossOver` install location.
    pub cxstart_path: PathBuf,
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
        let bottle_name = std::env::var("GW2_BOTTLE").unwrap_or_else(|_| "Guild Wars 2".to_owned());
        Self {
            bottle_name,
            cxstart_path: PathBuf::from(
                "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/cxstart",
            ),
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
            Ok(child) => {
                tracing::info!(
                    bottle = %opts.bottle_name,
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
                    bottle = %opts.bottle_name,
                    "could not start in-bottle Mumble Link holder; navigation tools \
                     will return NotConnected until the holder is reachable"
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

#[cfg(target_os = "macos")]
fn spawn_macos(opts: &HolderSupervisorOpts) -> anyhow::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    if !opts.cxstart_path.exists() {
        anyhow::bail!(
            "cxstart not found at {} — is CrossOver installed?",
            opts.cxstart_path.display()
        );
    }

    let bottle_dir = bottle_dir_for(&opts.bottle_name)?;
    let dest_dir = bottle_dir
        .join("drive_c")
        .join(holder_format::HOLDER_SUBDIR);
    std::fs::create_dir_all(&dest_dir)?;
    let dest_exe = dest_dir.join(holder_format::HOLDER_EXE_NAME);

    let source = locate_holder_source(opts.holder_exe_source.as_deref())?;
    install_if_changed(&source, &dest_exe)?;

    // Mirror file path in Windows convention. Public is host-visible at
    // <bottle>/drive_c/users/Public/.
    let win_subdir = holder_format::HOLDER_SUBDIR.replace('/', "\\");
    let bin_path_win = format!("C:\\{}\\{}", win_subdir, holder_format::HOLDER_BIN_NAME);
    let exe_path_win = format!("C:\\{}\\{}", win_subdir, holder_format::HOLDER_EXE_NAME);

    tracing::debug!(
        bottle = %opts.bottle_name,
        cxstart = %opts.cxstart_path.display(),
        exe = %dest_exe.display(),
        bin_path = %bin_path_win,
        "launching holder"
    );

    // --no-wait so cxstart returns control immediately (the holder runs
    // for the lifetime of gw2-mcp). Our Drop handler kills the child by
    // PID, which works because cxstart's exec child is the wine process
    // that becomes the parent of holder.exe — killing the immediate
    // child still triggers the wineserver-side cleanup.
    let child = Command::new(&opts.cxstart_path)
        .arg("--bottle")
        .arg(&opts.bottle_name)
        .arg("--no-wait")
        .arg(&exe_path_win)
        .arg("--bin-path")
        .arg(&bin_path_win)
        // stdout/stderr are inherited so `--log debug` users see holder
        // chatter in the same stream as the rest of gw2-mcp's stderr.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

#[cfg(target_os = "macos")]
fn bottle_dir_for(bottle_name: &str) -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")?;
    let dir = PathBuf::from(home)
        .join("Library/Application Support/CrossOver/Bottles")
        .join(bottle_name);
    if !dir.is_dir() {
        anyhow::bail!(
            "bottle {bottle_name:?} not found at {} — set GW2_BOTTLE to your bottle's name",
            dir.display()
        );
    }
    Ok(dir)
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
    fn missing_cxstart_yields_disabled_supervisor() {
        // Soft failure path — pointing at a non-existent cxstart should
        // log a warning and return a disabled supervisor, not panic.
        let opts = HolderSupervisorOpts {
            bottle_name: "DoesNotExist".to_owned(),
            cxstart_path: PathBuf::from("/no/such/cxstart"),
            holder_exe_source: None,
            soft_failure: true,
        };
        let s = HolderSupervisor::spawn(opts);
        assert!(!s.is_active());
    }
}
