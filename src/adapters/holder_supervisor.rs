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
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
use crate::adapters::{bottle_discovery, holder_format, holder_lock::HolderLock};

use crate::adapters::mumble_link::MirrorRescuer;

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

/// Length of the random session-token in bytes (encoded as hex it
/// becomes 16 chars — 64 bits of entropy, more than enough to uniquely
/// identify our holder among all host processes).
#[cfg(target_os = "macos")]
const SESSION_TOKEN_BYTES: usize = 8;

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

/// Multi-instance coordinator for the in-bottle Mumble Link holder.
///
/// **Cloneable.** Internals are `Arc<Mutex<State>>` so the supervisor can
/// be shared between `main` (which holds it as a lifetime guard) and the
/// `FileMumbleLink` reader (which calls `try_promote` on stale-mirror).
/// The holder is killed when the last clone drops.
///
/// ## Multi-instance model
///
/// One holder per bottle, regardless of how many gw2-mcp instances are
/// running. Coordination is via an advisory `flock` on a per-bottle
/// lockfile:
/// `<bottle>/drive_c/users/Public/gw2-mcp/holder.lock`. Whoever wins
/// the lock at startup is the leader and spawns the holder; everyone
/// else is a follower and just reads the shared mirror file. If the
/// leader exits (clean or crash), the kernel releases the flock and
/// the next reader to see a stale mirror promotes itself via
/// [`HolderSupervisor::try_promote`].
///
/// The previous (now-defunct) leader's orphan holder process — if any
/// survived — is identified by the session token recorded in the
/// lockfile and cleaned up with a `pgrep -f <token>` kill chain.
/// That replaces the old "kill anything matching `holder.exe`" sweep,
/// which would false-positive on a *live* concurrent leader's holder.
#[derive(Clone)]
pub struct HolderSupervisor {
    inner: Arc<Mutex<State>>,
}

/// Internal state. `Drop` on this enum (not on `HolderSupervisor`) means
/// the holder is terminated exactly once, when the last `Arc` clone
/// goes away.
enum State {
    /// No coordination. Used on non-macOS, when the user opted out, or
    /// when bottle discovery failed at startup (in which case
    /// `try_promote` does nothing — restart is required).
    Disabled,

    #[cfg(target_os = "macos")]
    /// Lock was contended at startup, OR a promotion attempt failed.
    /// Carries the opts and lock path so a subsequent promotion can
    /// retry without re-running bottle discovery.
    Follower {
        opts: HolderSupervisorOpts,
        lock_path: PathBuf,
    },

    #[cfg(target_os = "macos")]
    /// We are the active holder owner. Drop kills the child + chases
    /// the holder by session token + releases the flock (via `_lock`).
    Leader {
        child: std::process::Child,
        session_token: String,
        /// Holds the flock open. Dropped → flock released → next
        /// reader's `try_promote` can succeed.
        _lock: HolderLock,
        /// Retained so a future "leader → follower → leader" cycle is
        /// possible (e.g. if we ever add health-check-driven demotion).
        /// Today only Drop reads this — feel free to remove if the
        /// state machine stays purely upward-monotonic.
        #[allow(dead_code)]
        opts: HolderSupervisorOpts,
        #[allow(dead_code)]
        lock_path: PathBuf,
    },
}

impl Drop for State {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let State::Leader {
            child,
            session_token,
            ..
        } = self
        {
            // Step 1: try to kill the immediate child we spawned. For
            // Whisky (direct `wine64` invocation) this IS the holder, so
            // this is sufficient. For CrossOver (`cxstart --no-wait`) the
            // child is the launcher process which has long since exited;
            // this kill is a no-op there.
            let _ = child.kill();
            let _ = child.wait();

            // Step 2: chase the real holder via its session token, which
            // we passed via `--session-token` and which is visible in the
            // host process's argv. This is the reliable path for both
            // runners — orphans cannot survive it short of pgrep itself
            // being unavailable on the host (extremely unlikely on macOS).
            let killed = kill_processes_matching(session_token);
            if killed > 0 {
                tracing::debug!(
                    session = %session_token,
                    count = killed,
                    "terminated in-bottle holder"
                );
            } else {
                tracing::debug!(
                    session = %session_token,
                    "no holder process matched session token at Drop time \
                     (already exited?)"
                );
            }
            // `_lock` drops naturally here, releasing the flock so the
            // next leader can claim it.
        }
    }
}

impl HolderSupervisor {
    /// No-op supervisor used when we deliberately don't want to manage a
    /// holder (e.g. user passed `--no-mumble-holder`, or platform isn't
    /// macOS).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::Disabled)),
        }
    }

    /// Spawn the in-bottle holder. On non-macOS targets this returns
    /// `Self::disabled()` immediately — Linux + Windows MCP servers can
    /// reach Mumble Link directly and don't need a helper.
    #[cfg(not(target_os = "macos"))]
    pub fn spawn(_opts: HolderSupervisorOpts) -> Self {
        Self::disabled()
    }

    /// Spawn the in-bottle holder. Pure best-effort: every failure mode
    /// is logged and downgrades when `soft_failure = true`.
    ///
    /// Flow:
    /// 1. Discover the bottle. Failure here is terminal (Disabled) —
    ///    the user needs to install GW2 / set `GW2_BOTTLE` and restart.
    /// 2. Attempt the per-bottle flock. If contended, become a Follower
    ///    (another gw2-mcp instance owns the holder; we just read its
    ///    mirror).
    /// 3. If the lock is ours, kill any orphans from the previous
    ///    leader (identified by the token recorded in the lockfile),
    ///    then spawn a fresh holder. If the spawn fails, downgrade to
    ///    Follower so `try_promote` may succeed later.
    #[cfg(target_os = "macos")]
    pub fn spawn(opts: HolderSupervisorOpts) -> Self {
        let Some(bottle) = bottle_discovery::pick_gw2_bottle(opts.bottle_override.as_deref())
        else {
            let err = no_bottle_error(&opts);
            if opts.soft_failure {
                tracing::warn!(
                    error = ?err,
                    bottle_override = ?opts.bottle_override,
                    "could not start in-bottle Mumble Link holder; navigation tools \
                     will return NotConnected until a bottle is reachable. \
                     Run `gw2-mcp doctor` to diagnose."
                );
                return Self::disabled();
            }
            panic!("holder supervisor: {err}");
        };
        let lock_path = compute_lock_path(&bottle);

        let state = acquire_lock_or_follow(&opts, &lock_path);
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    /// Returns `true` if this supervisor currently owns the in-bottle
    /// holder process (i.e. is the Leader). Informational.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let state = self.inner.lock().expect("HolderSupervisor mutex poisoned");
        #[cfg(target_os = "macos")]
        return matches!(*state, State::Leader { .. });
        #[cfg(not(target_os = "macos"))]
        {
            let _ = state;
            false
        }
    }

    /// Attempt to promote this supervisor from Follower → Leader. Called
    /// by [`FileMumbleLink`](crate::adapters::mumble_link::FileMumbleLink)
    /// on stale-mirror detection.
    ///
    /// Returns `true` if a fresh holder was spawned (caller should
    /// surface a transient "restarting" message and retry), `false` if
    /// another instance still owns the lock or the spawn itself
    /// failed (caller should surface the existing stale-mirror error).
    pub fn try_promote(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            let mut state = self.inner.lock().expect("HolderSupervisor mutex poisoned");
            match &*state {
                State::Leader { .. } => true, // Already leader; nothing to do.
                State::Disabled => false,     // Terminal state; no retry path.
                State::Follower { opts, lock_path } => {
                    let opts = opts.clone();
                    let lock_path = lock_path.clone();
                    let new_state = acquire_lock_or_follow(&opts, &lock_path);
                    let promoted = matches!(new_state, State::Leader { .. });
                    *state = new_state;
                    promoted
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        false
    }
}

impl MirrorRescuer for HolderSupervisor {
    fn try_rescue(&self) -> bool {
        self.try_promote()
    }
}

#[cfg(target_os = "macos")]
fn compute_lock_path(bottle: &bottle_discovery::Bottle) -> PathBuf {
    bottle
        .root
        .join("drive_c")
        .join(holder_format::HOLDER_SUBDIR)
        .join("holder.lock")
}

/// Try to acquire the per-bottle lock; on success, sweep previous-leader
/// orphans and spawn a fresh holder. On any failure (contention or spawn
/// error) returns a Follower state so a subsequent `try_promote` may
/// succeed.
#[cfg(target_os = "macos")]
fn acquire_lock_or_follow(opts: &HolderSupervisorOpts, lock_path: &Path) -> State {
    match HolderLock::try_acquire(lock_path) {
        Ok(Some(lock)) => match become_leader_macos(opts, lock, lock_path) {
            Ok((child, session_token, lock)) => State::Leader {
                child,
                session_token,
                _lock: lock,
                opts: opts.clone(),
                lock_path: lock_path.to_path_buf(),
            },
            Err(e) => {
                if opts.soft_failure {
                    tracing::warn!(
                        error = ?e,
                        bottle_override = ?opts.bottle_override,
                        "leader spawn failed; falling back to follower mode (next stale-mirror \
                         detection will retry promotion)"
                    );
                    State::Follower {
                        opts: opts.clone(),
                        lock_path: lock_path.to_path_buf(),
                    }
                } else {
                    panic!("holder supervisor: {e}");
                }
            }
        },
        Ok(None) => {
            tracing::info!(
                lock = %lock_path.display(),
                "another gw2-mcp instance owns the in-bottle holder for this bottle; \
                 this instance will read its mirror file directly"
            );
            State::Follower {
                opts: opts.clone(),
                lock_path: lock_path.to_path_buf(),
            }
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                lock = %lock_path.display(),
                "lockfile error; in-bottle holder coordination unavailable"
            );
            State::Disabled
        }
    }
}

/// Lock-in-hand → spawned-holder transition. Reads the previous
/// leader's session token from the lockfile, kills any matching
/// orphans, generates a new token, writes it back, and launches the
/// holder. Returns the new (child, token, lock) trio on success.
#[cfg(target_os = "macos")]
fn become_leader_macos(
    opts: &HolderSupervisorOpts,
    mut lock: HolderLock,
    lock_path: &Path,
) -> anyhow::Result<(std::process::Child, String, HolderLock)> {
    // Fingerprint-scoped sweep of the previous leader's orphans.
    if let Ok(Some(prev_token)) = lock.read_previous_token() {
        let killed = kill_processes_matching(&prev_token);
        if killed > 0 {
            tracing::info!(
                prev_session = %prev_token,
                killed,
                "cleaned up {killed} orphan holder process(es) from a previous leader"
            );
        }
    }

    let session_token = generate_session_token();
    // Record the new token *before* spawning so a crash mid-spawn still
    // leaves the next leader with a known token to clean up.
    if let Err(e) = lock.write_token(&session_token) {
        tracing::warn!(
            error = ?e,
            lock = %lock_path.display(),
            "failed to record session token in lockfile; coordination still works \
             but future orphan-sweep will skip this run's holder"
        );
    }

    let SpawnResult { child, bottle } = spawn_macos(opts, &session_token)?;
    tracing::info!(
        bottle = %bottle.name,
        runner = bottle.runner.as_str(),
        pid = child.id(),
        session = %session_token,
        "in-bottle Mumble Link holder spawned (leader)"
    );
    Ok((child, session_token, lock))
}

/// What [`spawn_macos`] returns on success: the running child plus the
/// bottle we resolved (so the caller can log the runner and name).
#[cfg(target_os = "macos")]
struct SpawnResult {
    child: std::process::Child,
    bottle: bottle_discovery::Bottle,
}

#[cfg(target_os = "macos")]
fn spawn_macos(opts: &HolderSupervisorOpts, session_token: &str) -> anyhow::Result<SpawnResult> {
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
        session = %session_token,
        "launching holder"
    );

    let child = match bottle.runner {
        bottle_discovery::Runner::CrossOver => spawn_crossover(
            &opts.cxstart_path,
            &bottle.name,
            &exe_path_win,
            &bin_path_win,
            session_token,
        )?,
        bottle_discovery::Runner::Whisky => spawn_whisky(
            &opts.whisky_wine_path,
            &bottle.root,
            &dest_exe,
            &bin_path_win,
            session_token,
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
    session_token: &str,
) -> anyhow::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    if !cxstart_path.exists() {
        anyhow::bail!(
            "cxstart not found at {} — is CrossOver installed?",
            cxstart_path.display()
        );
    }
    // --no-wait so cxstart returns control immediately. The actual
    // holder.exe runs under the wineserver and gets reparented to PID 1
    // shortly after launch, so the Rust `Child` we get back becomes
    // useless within milliseconds. The supervisor's Drop relies on the
    // `--session-token` argv marker, not this `Child`, to terminate the
    // holder cleanly.
    let child = Command::new(cxstart_path)
        .arg("--bottle")
        .arg(bottle_name)
        .arg("--no-wait")
        .arg(exe_path_win)
        .arg("--bin-path")
        .arg(bin_path_win)
        .arg("--session-token")
        .arg(session_token)
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
    session_token: &str,
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
    // wine64 takes the .exe as a host path (it auto-translates). The
    // wine64 process exec's into the holder, so killing the returned
    // child IS killing the holder for the Whisky runner — but we still
    // pass the session token so Drop's fall-back cleanup works uniformly.
    let child = Command::new(wine_path)
        .env("WINEPREFIX", bottle_root)
        // Suppress fixup-on-first-launch chatter when possible. Wine
        // still writes some diagnostics to stderr; we inherit it so
        // they end up in the same stream as gw2-mcp logs.
        .env("WINEDEBUG", "-all")
        .arg(holder_host_path)
        .arg("--bin-path")
        .arg(bin_path_win)
        .arg("--session-token")
        .arg(session_token)
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

/// Generate a fresh per-supervisor session token. Pure random bytes
/// rendered as hex — no system identifiers that might leak about the
/// user, no clock skew or PID-recycling concerns.
#[cfg(target_os = "macos")]
fn generate_session_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; SESSION_TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Find every host process whose full argv contains `pattern`, send it
/// SIGTERM, give it a 200ms grace period, then SIGKILL anything still
/// alive. Returns the number of distinct PIDs we sent at least one
/// signal to. Best-effort: if `pgrep` itself is unavailable we log at
/// debug-level and return 0, letting the caller continue.
///
/// Used in two places, always with a session token as the pattern:
/// 1. [`become_leader_macos`] — sweeps the *previous leader's* holder
///    using the session token recorded in the lockfile. Won't match
///    any concurrent live leader's holder because the lockfile is
///    rewritten before the new spawn.
/// 2. [`State`]'s `Drop` — kills the specific holder we own by matching
///    the per-instance session token we passed via `--session-token`.
///
/// Lives on macOS only because that's the only platform where the
/// holder runs and the in-bottle/host PID gap exists. POSIX `pgrep` is
/// also present on Linux, so the function would work there too — we
/// just have no reason to call it.
///
/// ## Threading constraint — call at task root only
///
/// This function uses **synchronous** `std::process::Command` and
/// `std::thread::sleep(200ms)`. It must therefore only be invoked from
/// code paths that are NOT inside a live tokio task — specifically:
/// from `HolderSupervisor::spawn` (called from `main` before the
/// runtime starts dispatching tool calls) and from `Drop` (which runs
/// when the runtime is already shutting down). Calling it from a tool
/// handler or any other async context would block the executor for
/// 200ms. If you need this from inside an async task in the future,
/// wrap it in `tokio::task::spawn_blocking`.
#[cfg(target_os = "macos")]
fn kill_processes_matching(pattern: &str) -> usize {
    use std::process::Command;

    let Ok(output) = Command::new("pgrep").arg("-f").arg(pattern).output() else {
        tracing::debug!(
            pattern,
            "pgrep unavailable; cannot enumerate matching processes"
        );
        return 0;
    };
    let pids: Vec<String> = std::str::from_utf8(&output.stdout)
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect();
    if pids.is_empty() {
        return 0;
    }
    // SIGTERM first — gives the holder a chance to clean up even though
    // it has no real cleanup to do. Anyone reading logs sees TERM, not
    // KILL, which is the conventional "asked nicely" signal.
    for pid in &pids {
        let _ = Command::new("kill").arg("-TERM").arg(pid).status();
    }
    // Brief grace period before escalating. 200ms is well over the
    // holder's typical 50ms poll cycle, so anything that's going to
    // exit on TERM has already done so.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Anything still alive: SIGKILL. `kill -KILL` on an already-dead
    // PID just errors silently — we don't bother distinguishing.
    for pid in &pids {
        let _ = Command::new("kill").arg("-KILL").arg(pid).status();
    }
    pids.len()
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

    #[cfg(target_os = "macos")]
    #[test]
    fn session_tokens_are_unique_and_well_formed() {
        let a = generate_session_token();
        let b = generate_session_token();
        assert_ne!(a, b, "two tokens should be different");
        assert_eq!(
            a.len(),
            SESSION_TOKEN_BYTES * 2,
            "hex doubles the byte count"
        );
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "token should be pure hex"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kill_processes_matching_terminates_processes_with_token_in_argv() {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        // Use perl as the long-running stand-in. It's pre-installed on
        // every macOS, behaves predictably (no `bash -c` exec-optimization,
        // no coreutils-multicall argv[0] dispatching that some users have
        // on PATH via Homebrew), and the token rides through as a trailing
        // positional arg that's visible to both `ps` and `pgrep -f`.
        // This mirrors what happens at runtime: our holder accepts
        // `--session-token <token>` and the token lands in argv.
        let token = generate_session_token();
        let mut child = Command::new("perl")
            .arg("-e")
            .arg("sleep 60")
            .arg(&token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn perl");

        // pgrep needs a beat to see the new process.
        std::thread::sleep(Duration::from_millis(150));

        let killed = kill_processes_matching(&token);
        assert!(killed >= 1, "expected at least one matching process");

        // Give the kill chain (TERM + 200ms + KILL) time to land.
        std::thread::sleep(Duration::from_millis(500));

        // `try_wait()` is the right liveness check here. `kill -0 <pid>`
        // returns success for zombies too — and the perl we killed
        // becomes a zombie because *this* test process is its parent
        // and Rust hasn't reaped it. try_wait reaps and tells us
        // whether the child has actually exited.
        let exit = child
            .try_wait()
            .expect("try_wait should succeed on owned child");
        assert!(
            exit.is_some(),
            "perl child should have exited after kill_processes_matching"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kill_processes_matching_returns_zero_when_no_match() {
        // A token that can't possibly match any real process.
        let bogus = format!("--no-match-pattern-{}", generate_session_token());
        let killed = kill_processes_matching(&bogus);
        assert_eq!(killed, 0);
    }
}
