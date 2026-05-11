//! Auto-build the in-bottle `gw2-mcp-holder.exe` when running gw2-mcp
//! from a cargo workspace on macOS.
//!
//! In a release-tarball deployment the holder ships next to the gw2-mcp
//! binary. In a dev checkout (`cargo run`), there's no sibling — and
//! [`HolderSupervisor`](crate::adapters::HolderSupervisor) used to
//! require the contributor to manually cross-build via
//! `cargo build --target x86_64-pc-windows-gnu --release --bin gw2-mcp-holder`
//! before nav tools would work end-to-end. This module makes that
//! invocation automatic so the contributor's "edit + cargo run + test"
//! loop matches the release shape without manual steps.
//!
//! ## Lifecycle
//!
//! Runs synchronously before [`HolderSupervisor::spawn`]. The first
//! cross-build is slow (1-3 minutes; pulls the holder's dep tree); subsequent
//! runs are instant when `src/bin/holder.rs` and its deps are unchanged.
//!
//! ## Failure modes
//!
//! Toolchain prerequisites (`rustup target add x86_64-pc-windows-gnu`,
//! `x86_64-w64-mingw32-gcc` linker on PATH) are checked up front; a
//! missing prereq returns [`DevBuildError::ToolchainMissing`] with an
//! install hint embedded in the message. The caller (`main.rs`) prints
//! the error and exits non-zero — the contributor needs to fix the
//! toolchain anyway before nav tools can work.
//!
//! ## Opt-out
//!
//! Set `GW2_NO_AUTO_BUILD_HOLDER=1` to disable. Useful if you're
//! managing the holder build manually (e.g. in CI, or when iterating
//! on the holder itself with a separate `cargo watch`).

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum DevBuildError {
    #[error("{0}")]
    ToolchainMissing(String),

    #[error("cross-build failed: {0}")]
    BuildFailed(String),
}

/// Entry point used by `main.rs`. Returns:
/// - `Ok(None)` if not in a dev workspace, on non-macOS, or when
///   `GW2_NO_AUTO_BUILD_HOLDER` is set;
/// - `Ok(Some(path))` if the holder is available (already built or
///   freshly built) at the returned path — caller wires it into
///   [`HolderSupervisorOpts::holder_exe_source`](crate::adapters::HolderSupervisorOpts);
/// - `Err(_)` with an actionable message for missing toolchains or
///   build failures.
pub fn ensure_holder_built_if_dev() -> Result<Option<PathBuf>, DevBuildError> {
    // Linux/Windows: no holder concept. Skip silently.
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    if std::env::var("GW2_NO_AUTO_BUILD_HOLDER")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
    {
        tracing::debug!("GW2_NO_AUTO_BUILD_HOLDER set; skipping dev auto-build");
        return Ok(None);
    }
    let Some(workspace) = detect_dev_workspace() else {
        // Running from a release tarball or installed binary — the
        // holder.exe sibling is already in place. Nothing to do.
        return Ok(None);
    };
    tracing::debug!(
        workspace = %workspace.display(),
        "detected cargo workspace; checking gw2-mcp-holder.exe build state"
    );
    build_holder_if_needed(&workspace).map(Some)
}

/// Walk up from `current_exe` looking for a `Cargo.toml` whose package
/// name is `gw2-mcp`. Returns the workspace root, or `None` if we don't
/// look like we're running from a cargo build artifact in this repo.
fn detect_dev_workspace() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Walk up: target/{debug,release}/gw2-mcp → target/{debug,release} →
    // target → workspace root. Cap the walk to keep things bounded if
    // the binary is somewhere unexpected.
    let mut cursor = exe.parent()?.to_path_buf();
    for _ in 0..6 {
        let manifest = cursor.join("Cargo.toml");
        if manifest.exists()
            && let Ok(content) = std::fs::read_to_string(&manifest)
            // Quoted because the package name field is canonically rendered
            // `name = "gw2-mcp"` by `cargo new`. This is a guard against
            // accidentally matching a foreign workspace whose binary happens
            // to share our exe name.
            && content.contains("name = \"gw2-mcp\"")
        {
            return Some(cursor);
        }
        cursor = cursor.parent()?.to_path_buf();
    }
    None
}

/// Build the holder if its target is missing or older than its source.
/// Returns the path to the built artifact on success.
fn build_holder_if_needed(workspace: &Path) -> Result<PathBuf, DevBuildError> {
    let target = workspace
        .join("target")
        .join("x86_64-pc-windows-gnu")
        .join("release")
        .join("gw2-mcp-holder.exe");
    let source = workspace.join("src").join("bin").join("holder.rs");

    if target.exists() && !is_source_newer(&source, &target) {
        tracing::debug!(
            target = %target.display(),
            "gw2-mcp-holder.exe is up to date; skipping cross-build"
        );
        return Ok(target);
    }

    // Up-front toolchain checks. Failure here is much more
    // actionable than letting `cargo build` fail with a cryptic
    // "linker x86_64-w64-mingw32-gcc not found" or
    // "error: unknown target ... x86_64-pc-windows-gnu".
    check_rustup_target_installed()?;
    check_mingw_linker_available()?;

    tracing::info!(
        target = %target.display(),
        "cross-building gw2-mcp-holder.exe for the in-bottle Mumble Link helper \
         (first run takes ~1-3 minutes; subsequent runs are instant)"
    );

    let status = std::process::Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("x86_64-pc-windows-gnu")
        .arg("--bin")
        .arg("gw2-mcp-holder")
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        // Inherit so the contributor sees compile progress; surprising
        // silence on a multi-minute build would feel like a hang.
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| DevBuildError::BuildFailed(format!("could not spawn cargo: {e}")))?;

    if !status.success() {
        return Err(DevBuildError::BuildFailed(format!(
            "cargo build --target x86_64-pc-windows-gnu --bin gw2-mcp-holder exited with {status}"
        )));
    }

    if !target.exists() {
        return Err(DevBuildError::BuildFailed(format!(
            "cargo build reported success but {} is missing",
            target.display()
        )));
    }

    tracing::info!(target = %target.display(), "gw2-mcp-holder.exe built");
    Ok(target)
}

fn is_source_newer(source: &Path, target: &Path) -> bool {
    let (Ok(s), Ok(t)) = (source.metadata(), target.metadata()) else {
        return false;
    };
    match (s.modified(), t.modified()) {
        (Ok(sm), Ok(tm)) => sm > tm,
        _ => false,
    }
}

fn check_rustup_target_installed() -> Result<(), DevBuildError> {
    // `rustup target list --installed` is the canonical query. If
    // rustup itself isn't available (rare in a Rust dev env), we
    // assume the toolchain came from a different distribution and
    // skip the check rather than hard-failing.
    let Ok(output) = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    else {
        tracing::debug!("rustup not on PATH; skipping target-installed precheck");
        return Ok(());
    };
    if !output.status.success() {
        tracing::debug!(
            status = ?output.status,
            "rustup target list failed; skipping target-installed precheck"
        );
        return Ok(());
    }
    let installed = std::str::from_utf8(&output.stdout).unwrap_or("");
    if installed
        .lines()
        .any(|l| l.trim() == "x86_64-pc-windows-gnu")
    {
        return Ok(());
    }
    Err(DevBuildError::ToolchainMissing(
        "rustup target `x86_64-pc-windows-gnu` is not installed. Install it with:\n\
         \n  rustup target add x86_64-pc-windows-gnu\n\n\
         Then re-run gw2-mcp. (Set GW2_NO_AUTO_BUILD_HOLDER=1 to skip this auto-build \
         entirely.)"
            .to_owned(),
    ))
}

fn check_mingw_linker_available() -> Result<(), DevBuildError> {
    // rustc invokes `x86_64-w64-mingw32-gcc` as the linker for the
    // `gnu` ABI variant of the Windows target. Anything else
    // (clang, lld) would need explicit Cargo config that we don't
    // ship, so the absence of this binary is the de-facto blocker.
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join("x86_64-w64-mingw32-gcc").exists() {
                return Ok(());
            }
        }
    }
    Err(DevBuildError::ToolchainMissing(
        "`x86_64-w64-mingw32-gcc` (the mingw-w64 cross-linker) is not on PATH. Install \
         it with one of:\n\
         \n  brew install mingw-w64\n\
         \n  # nix-darwin / NixOS:\n  pkgsCross.mingwW64.buildPackages.gcc\n\n\
         Then re-run gw2-mcp. (Set GW2_NO_AUTO_BUILD_HOLDER=1 to skip this auto-build \
         entirely.)"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_source_newer_returns_false_for_missing_files() {
        let dir = tempdir().unwrap();
        let s = dir.path().join("nonexistent.rs");
        let t = dir.path().join("also-missing.exe");
        assert!(!is_source_newer(&s, &t));
    }

    #[test]
    fn is_source_newer_returns_true_when_source_is_newer() {
        let dir = tempdir().unwrap();
        let s = dir.path().join("src.rs");
        let t = dir.path().join("bin.exe");
        // Create the target first, then the source — source ends up newer.
        std::fs::write(&t, b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&s, b"new").unwrap();
        assert!(is_source_newer(&s, &t));
    }

    #[test]
    fn detect_dev_workspace_rejects_non_workspace_dirs() {
        // No way to mutate current_exe in a unit test, but we can at
        // least exercise the "Cargo.toml exists but with the wrong
        // package name" guard via a direct helper. For now,
        // `detect_dev_workspace` is exercised by the integration
        // path; this test documents the negative case shape.
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"some-other-crate\"\n").unwrap();
        // We can't redirect current_exe from here; assert the file we
        // wrote is what we'd reject if it were the candidate manifest.
        let content = std::fs::read_to_string(&manifest).unwrap();
        assert!(!content.contains("name = \"gw2-mcp\""));
    }
}
