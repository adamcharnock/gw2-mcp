//! Cross-platform reader for Guild Wars 2's [Mumble Link] live game state.
//!
//! GW2 (and many other games) writes a fixed C struct to a memory-mapped
//! region every frame while the client is running. We only need a small
//! subset:
//! - the avatar position + facing vector,
//! - the per-game `context` block (which carries the 2D *map* coords as
//!   `player_x`/`player_y`, plus map id, build id, etc.),
//! - the `identity` JSON blob (character name, profession, race, …).
//!
//! The shared-memory region lives at:
//! - **Windows**: a named file mapping called `MumbleLink`.
//! - **Linux/Wine** (Steam Proton, Lutris): a tmpfs file at
//!   `/dev/shm/MumbleLink`.
//! - **macOS**: GW2 doesn't run natively. With CrossOver/Whisky (both
//!   Wine-based) the file lands inside the wine prefix, e.g.
//!   `~/Library/Application Support/CrossOver/Bottles/<bottle>/dosdevices/`,
//!   but the path is unstable across versions. We probe a small set of
//!   common locations and otherwise return `Unsupported` so the user gets
//!   a clear error rather than a silent wrong answer.
//!
//! ## Manual verification protocol
//!
//! With GW2 running and `--no-mumble-link` *not* set, run:
//! ```text
//! cargo run -- --version
//! ```
//! At startup the binary logs `mumble link adapter: connected (ui_tick=N)`
//! when probing succeeds, or `mumble link adapter: stub (reason: …)` when
//! it falls back. To exercise a tool live, call `get_my_location` from any
//! MCP client; with the GW2 client foregrounded and a character logged in,
//! the response should change ~60 times per second.
//!
//! [Mumble Link]: https://wiki.guildwars2.com/wiki/API:MumbleLink

#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

// Port types — trait, error, and snapshot shape — live in `ports.rs` per
// the project's hexagonal architecture. The adapter only owns concrete
// impls (`StubMumbleLink`, `FileMumbleLink`, `WindowsMumbleLink`),
// the byte-level layout (`RawMumbleHeader`, `RawGw2Context`), and the
// parsing helper (`parse_header`).
#[cfg(unix)]
use crate::adapters::{bottle_discovery, holder_format};
use crate::ports::{MumbleContext, MumbleError, MumbleIdentity, MumbleLink, MumbleSnapshot};

// ---------------------------------------------------------------------------
// Raw layout — bytemuck-derived for zero-copy reinterpretation.
// ---------------------------------------------------------------------------

/// Raw Mumble Link header struct, up to and including the `context` block.
/// We don't bother modelling the `description` tail because we never read
/// it. `repr(C)` because we cast it from a memory-mapped region whose
/// layout is dictated by the Mumble spec.
///
/// The `name` and `identity` fields are UTF-16 strings. `Pod` requires
/// every field to itself be `Pod`; `[u16; 256]` is, so this works.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RawMumbleHeader {
    ui_version: u32,
    ui_tick: u32,
    f_avatar_position: [f32; 3],
    f_avatar_front: [f32; 3],
    f_avatar_top: [f32; 3],
    name: [u16; 256],
    f_camera_position: [f32; 3],
    f_camera_front: [f32; 3],
    f_camera_top: [f32; 3],
    identity: [u16; 256],
    context_len: u32,
    context: [u8; 256],
}

/// Required size of the mapped region we read. The full Mumble Link
/// struct is 5876 bytes (description tail brings it there); we only ever
/// need the first `MUMBLE_HEADER_LEN`.
pub const MUMBLE_HEADER_LEN: usize = std::mem::size_of::<RawMumbleHeader>();

/// Repr of the GW2 context block. Same `repr(C)` rules apply. The fields
/// total 85 bytes — the trailing 171 bytes of the 256-byte block are
/// padding / future use.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RawGw2Context {
    server_address: [u8; 28],
    map_id: u32,
    map_type: u32,
    shard_id: u32,
    instance: u32,
    build_id: u32,
    ui_state: u32,
    compass_width: u16,
    compass_height: u16,
    compass_rotation: f32,
    player_x: f32,
    player_y: f32,
    map_center_x: f32,
    map_center_y: f32,
    map_scale: f32,
    process_id: u32,
    mount_index: u8,
    /// Explicit trailing padding so `bytemuck::Pod` is satisfied (the
    /// struct's natural alignment is 4 because of the u32/f32 fields,
    /// so without this Rust would insert 3 hidden padding bytes after
    /// `mount_index` and `Pod`'s no-padding requirement would fail).
    _pad: [u8; 3],
}

// ---------------------------------------------------------------------------
// Parsing helpers — pure, exhaustively tested via fixtures.
// ---------------------------------------------------------------------------

/// Decode a header byte buffer into a [`MumbleSnapshot`]. The buffer
/// must be at least `MUMBLE_HEADER_LEN` bytes; longer is fine (we
/// ignore the tail). Returns `NotConnected` when `ui_tick == 0`,
/// regardless of the rest of the payload — that's the convention the
/// Mumble Link spec uses to signal "no game data yet".
pub fn parse_header(bytes: &[u8]) -> Result<MumbleSnapshot, MumbleError> {
    if bytes.len() < MUMBLE_HEADER_LEN {
        return Err(MumbleError::Decode(format!(
            "buffer too small: {} bytes, need at least {}",
            bytes.len(),
            MUMBLE_HEADER_LEN
        )));
    }
    let header: &RawMumbleHeader = bytemuck::from_bytes(&bytes[..MUMBLE_HEADER_LEN]);

    if header.ui_tick == 0 {
        return Err(MumbleError::NotConnected(
            "ui_tick=0 (Mumble region exists but no live game frames yet — \
             log a character in)"
                .to_owned(),
        ));
    }

    let identity = parse_identity_utf16(&header.identity)?;
    let context = parse_gw2_context(&header.context);

    Ok(MumbleSnapshot {
        ui_version: header.ui_version,
        ui_tick: header.ui_tick,
        avatar_position: header.f_avatar_position,
        avatar_front: header.f_avatar_front,
        camera_position: header.f_camera_position,
        camera_front: header.f_camera_front,
        identity,
        context,
    })
}

fn parse_identity_utf16(buf: &[u16; 256]) -> Result<MumbleIdentity, MumbleError> {
    // Truncate at first NUL — Mumble identity is a UTF-16 *string*.
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    if len == 0 {
        // Empty identity is fine — return defaults rather than error.
        return Ok(MumbleIdentity::default());
    }
    let s = String::from_utf16(&buf[..len])
        .map_err(|e| MumbleError::Decode(format!("identity is not valid UTF-16: {e}")))?;
    serde_json::from_str::<MumbleIdentity>(&s)
        .map_err(|e| MumbleError::Decode(format!("identity JSON parse: {e} (raw: {s:?})")))
}

fn parse_gw2_context(buf: &[u8; 256]) -> MumbleContext {
    // IMPORTANT: `context_len` is **not** a "how many bytes are populated"
    // indicator. Per the Mumble Link spec it's the prefix length Mumble
    // uses for proximity-voice grouping ("are these two players in the
    // same instance?"). GW2 sets it to 48 — the bytes of
    // server+map+shard+instance+build that uniquely identify an instance.
    // Everything past offset 48 (ui_state, compass, player_x/y, map
    // center/scale, process_id, mount_index) is per-player state that
    // GW2 writes regardless. An earlier version of this code gated on
    // `context_len >= 85` and surfaced character-select-style messages
    // even when the player was zoned in and producing live coords —
    // pure misdiagnosis. Our `RawGw2Context` (88 bytes incl. trailing
    // alignment padding) reads the canonical layout, so the buffer's
    // full size is always available.
    let struct_size = std::mem::size_of::<RawGw2Context>();
    let raw: &RawGw2Context = bytemuck::from_bytes(&buf[..struct_size]);
    MumbleContext {
        server_address: raw.server_address,
        map_id: raw.map_id,
        map_type: raw.map_type,
        shard_id: raw.shard_id,
        instance: raw.instance,
        build_id: raw.build_id,
        ui_state: raw.ui_state,
        compass_width: raw.compass_width,
        compass_height: raw.compass_height,
        compass_rotation: raw.compass_rotation,
        player_x: raw.player_x,
        player_y: raw.player_y,
        map_center_x: raw.map_center_x,
        map_center_y: raw.map_center_y,
        map_scale: raw.map_scale,
        process_id: raw.process_id,
        mount_index: raw.mount_index,
    }
}

// ---------------------------------------------------------------------------
// Stub adapter — used when --no-mumble-link is set or the platform has no
// reachable shared-memory region.
// ---------------------------------------------------------------------------

/// A `MumbleLink` that always returns the same error. Used both as the
/// `--no-mumble-link` opt-out and as the auto-fallback when probing
/// fails at startup.
pub struct StubMumbleLink {
    reason: String,
    /// If true, render the error as `Unsupported` rather than
    /// `NotConnected` — `--no-mumble-link` is a deliberate user choice,
    /// not a missing dependency.
    unsupported: bool,
}

impl StubMumbleLink {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            unsupported: false,
        }
    }

    #[must_use]
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            unsupported: true,
        }
    }

    /// Wrap as a port-safe trait object.
    #[must_use]
    pub fn into_arc(self) -> Arc<dyn MumbleLink> {
        Arc::new(self)
    }
}

impl MumbleLink for StubMumbleLink {
    fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError> {
        if self.unsupported {
            Err(MumbleError::Unsupported(self.reason.clone()))
        } else {
            Err(MumbleError::NotConnected(self.reason.clone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Live adapters — Linux/macOS via memmap2; Windows via OpenFileMappingW.
// ---------------------------------------------------------------------------

/// Returns the OS-specific candidate file paths to probe for a Mumble
/// Link mapping. On Windows the named-mapping path is used instead, so
/// this function isn't compiled there.
///
/// On macOS each candidate is a holder mirror file at
/// `<bottle>/drive_c/<HOLDER_SUBDIR>/<HOLDER_BIN_NAME>` — the file format
/// is the 16-byte holder header followed by the raw 5460-byte `LinkedMem`.
/// On Linux it's the bare `/dev/shm/MumbleLink` (no holder header). The
/// reader auto-detects which by checking the magic.
///
/// Bottle enumeration is delegated to [`bottle_discovery::discover_bottles`]
/// so the supervisor and the reader agree on what bottles exist.
#[cfg(unix)]
fn unix_candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Linux + Steam Proton: tmpfs handle exposed by the Wine server.
    out.push(PathBuf::from("/dev/shm/MumbleLink"));
    // macOS bottles — both CrossOver and Whisky. discover_bottles returns
    // an empty Vec on non-macOS so this is a no-op on Linux.
    for bottle in bottle_discovery::discover_bottles() {
        out.push(
            bottle
                .root
                .join("drive_c")
                .join(holder_format::HOLDER_SUBDIR)
                .join(holder_format::HOLDER_BIN_NAME),
        );
        // Legacy / hypothetical: a bare MumbleLink on the wineserver tmp,
        // kept for forward compat with jokolink-style helpers. Harmless
        // if absent.
        out.push(bottle.root.join("dosdevices/MumbleLink"));
    }
    out
}

/// Callback the reader uses when it finds the mirror file stale. Whoever
/// supplies this — currently `HolderSupervisor` on macOS — promises that
/// `try_rescue` will attempt to re-establish a fresh writer (e.g. by
/// re-electing this process as the in-bottle holder leader) and return
/// `true` if the attempt succeeded so the caller can surface a transient
/// "restarting" message instead of the permanent stale-mirror one.
///
/// Lives here rather than in the supervisor module to keep the
/// dependency direction clean: the reader is the consumer, the
/// supervisor is the implementor.
pub trait MirrorRescuer: Send + Sync {
    fn try_rescue(&self) -> bool;
}

/// File-backed reader: opens (and re-opens) `path` on every snapshot.
/// memmap2 internally uses platform mmap calls (no `unsafe` exposed in
/// our codebase) and this works for `/dev/shm/MumbleLink` on Linux.
#[cfg(unix)]
pub struct FileMumbleLink {
    path: PathBuf,
    /// Optional rescuer invoked on stale-mirror detection. Used to
    /// promote this gw2-mcp instance to in-bottle holder leader when
    /// the previous leader's process has died.
    rescuer: Option<Arc<dyn MirrorRescuer>>,
}

#[cfg(unix)]
impl FileMumbleLink {
    /// Probe the standard candidate paths and return the first reader
    /// whose mapping contains a non-zero `ui_tick`. Returns
    /// `MumbleError::NotConnected` if nothing is reachable.
    pub fn probe() -> Result<Self, MumbleError> {
        for p in unix_candidate_paths() {
            if !p.exists() {
                continue;
            }
            // Try reading once to confirm the region looks sane.
            let reader = Self {
                path: p.clone(),
                rescuer: None,
            };
            match reader.snapshot() {
                // Either it's working OR `NotConnected` (the file
                // exists but `ui_tick==0` because the player hasn't
                // logged in yet). Either way, adopt this path — the
                // next call picks up live data.
                Ok(_) | Err(MumbleError::NotConnected(_)) => return Ok(reader),
                Err(_) => {}
            }
        }
        let hint = if cfg!(target_os = "macos") {
            "No Mumble Link mirror found. No CrossOver/Whisky bottle with \
             Guild Wars 2 was detected. Run `gw2-mcp doctor` to diagnose, \
             or set GW2_BOTTLE to your bottle's name."
                .to_owned()
        } else {
            "No Mumble Link region found. Ensure GW2 is running on this \
             host (Linux/Wine writes to /dev/shm/MumbleLink)."
                .to_owned()
        };
        Err(MumbleError::NotConnected(format!(
            "{hint} (Probed: {:?})",
            unix_candidate_paths()
        )))
    }

    /// Attach a rescuer that's consulted when a stale mirror is detected.
    /// Builder so wiring stays a one-liner in main.rs.
    #[must_use]
    pub fn with_rescuer(mut self, rescuer: Arc<dyn MirrorRescuer>) -> Self {
        self.rescuer = Some(rescuer);
        self
    }
}

#[cfg(unix)]
impl MumbleLink for FileMumbleLink {
    fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError> {
        let file = std::fs::File::open(&self.path).map_err(MumbleError::Io)?;
        // SAFETY-OF-MAPPING: `memmap2::Mmap::map` is itself safe; the
        // unsafe contract (the file not being modified concurrently in
        // a way that breaks Rust's aliasing rules) is the *caller's*
        // problem on POSIX. For shared-memory regions written by another
        // process we accept the risk — there's no Rust-safe way to read
        // mmap'd shared memory, and the region is plain old data we
        // bytemuck-cast read-only. This adapter never writes.
        let mmap = unsafe_mmap_readonly(&file)?;

        // Two on-disk shapes are possible. macOS bottles use the holder
        // mirror format (16-byte header + 5460-byte LinkedMem); Linux
        // /dev/shm/MumbleLink is the raw LinkedMem with no header. We
        // auto-detect via the holder magic.
        let is_holder = mmap.len() >= holder_format::HOLDER_HEADER_LEN
            && &mmap[..holder_format::HOLDER_MAGIC.len()] == holder_format::HOLDER_MAGIC;
        let payload = if is_holder {
            match stripped_holder_payload(&mmap)? {
                StripResult::Fresh(p) => p,
                StripResult::Stale { age, pid } => {
                    return Err(self.build_stale_mirror_error(age, pid));
                }
            }
        } else {
            &mmap[..]
        };

        if payload.len() < MUMBLE_HEADER_LEN {
            return Err(MumbleError::Decode(format!(
                "mapped region too small: {} bytes (need {})",
                payload.len(),
                MUMBLE_HEADER_LEN
            )));
        }
        match parse_header(&payload[..MUMBLE_HEADER_LEN]) {
            // When reading a holder mirror, ui_tick=0 means the helper
            // is running but GW2 has not started writing yet — a very
            // different situation from "no GW2 at all", and the user
            // needs different advice. Override the generic message.
            Err(MumbleError::NotConnected(_)) if is_holder => {
                Err(MumbleError::NotConnected(format!(
                    "Mumble Link helper is running ({}), but GW2 has not started \
                     publishing live data yet. Is GW2 running and logged in to a \
                     character?",
                    self.path.display()
                )))
            }
            other => other,
        }
    }
}

/// Result of inspecting the holder-mirror header: either the inner
/// `LinkedMem` payload, or "stale" metadata for the caller to build a
/// failure message around. Pulled out so the snapshot path can consult
/// the optional [`MirrorRescuer`] before deciding what to tell the user.
#[cfg(unix)]
enum StripResult<'a> {
    Fresh(&'a [u8]),
    Stale { age: u64, pid: u32 },
}

/// Strip the 16-byte holder header. Returns [`StripResult::Stale`] when
/// the timestamp says the writing holder is gone (>5s since last write)
/// so the caller can attempt rescue + tailor the message; returns
/// [`MumbleError::Decode`] for bad-magic / malformed headers.
#[cfg(unix)]
fn stripped_holder_payload(mmap: &[u8]) -> Result<StripResult<'_>, MumbleError> {
    let h = holder_format::parse_header(mmap)
        .map_err(|e| MumbleError::Decode(format!("holder header: {e}")))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u64, |d| d.as_secs());
    let written = u64::from(h.write_unix_seconds);
    let age = now.saturating_sub(written);
    if age > holder_format::HOLDER_STALE_AFTER_SECONDS {
        return Ok(StripResult::Stale {
            age,
            pid: h.holder_pid,
        });
    }
    Ok(StripResult::Fresh(
        &mmap[holder_format::HOLDER_HEADER_LEN..],
    ))
}

#[cfg(unix)]
impl FileMumbleLink {
    /// Build the user-facing error for a stale mirror. If a rescuer is
    /// wired up *and* its attempt succeeds, return a transient
    /// "restarting" message so the LLM retries naturally; otherwise
    /// fall back to the original "crashed; restart gw2-mcp" message
    /// that points at `gw2-mcp doctor`.
    fn build_stale_mirror_error(&self, age: u64, pid: u32) -> MumbleError {
        let rescued = self.rescuer.as_ref().is_some_and(|r| r.try_rescue());
        let msg = if rescued {
            format!(
                "Mumble Link mirror at {} was stale (last write {age}s ago, previous holder \
                 pid={pid}). This gw2-mcp instance has just spawned a fresh in-bottle \
                 helper — retry the call in ~1s for live data.",
                self.path.display()
            )
        } else {
            format!(
                "Mumble Link mirror at {} is stale (last write {age}s ago, holder pid={pid}). \
                 The in-bottle helper appears to have crashed — restart gw2-mcp, or run \
                 `gw2-mcp doctor` for diagnostics.",
                self.path.display()
            )
        };
        MumbleError::NotConnected(msg)
    }
}

#[cfg(unix)]
fn unsafe_mmap_readonly(file: &std::fs::File) -> Result<memmap2::Mmap, MumbleError> {
    // `Mmap::map(&File)` is `unsafe` on memmap2 ≥ 0.5 because of the
    // aforementioned aliasing risk. We isolate the call here, document
    // it, and gate it behind a single `#[allow(unsafe_code)]` so the
    // crate-level `deny` lint stays loud everywhere else.
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(file) }.map_err(MumbleError::Io)?;
    Ok(mmap)
}

// ----- Windows live adapter -----------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Windows-specific Mumble Link reader. Opens the named file mapping
    //! `MumbleLink`, maps a view, and reads our header out. No Rust-safe
    //! crate currently wraps `OpenFileMappingW`, so this module owns the
    //! single `unsafe` block in the codebase.

    use super::{MUMBLE_HEADER_LEN, MumbleError, MumbleLink, MumbleSnapshot, parse_header};

    use std::ptr;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Memory::{
        FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    };
    use windows::core::PCWSTR;

    pub struct WindowsMumbleLink {
        // We re-open the mapping per-snapshot rather than holding a
        // long-lived handle — keeps lifetime management trivial and
        // matches the cost profile of the Linux path.
    }

    impl WindowsMumbleLink {
        pub fn probe() -> Result<Self, MumbleError> {
            // Cheap probe: try a snapshot once.
            let me = Self {};
            match me.snapshot() {
                Ok(_) => Ok(me),
                Err(MumbleError::NotConnected(_)) => Ok(me),
                Err(e) => Err(e),
            }
        }
    }

    impl MumbleLink for WindowsMumbleLink {
        fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError> {
            // Encode "MumbleLink\0" as UTF-16.
            let mut wide: Vec<u16> = "MumbleLink".encode_utf16().collect();
            wide.push(0);

            // SAFETY: the Win32 calls below all take valid pointers and
            // we obey their lifetime rules (CloseHandle / UnmapViewOfFile
            // before return). The mapped memory is read-only (FILE_MAP_READ);
            // we only `parse_header` over it, which is `bytemuck::from_bytes`
            // on a slice (no writes, no aliasing of mutable refs).
            #[allow(unsafe_code)]
            let snapshot = unsafe {
                let handle: HANDLE =
                    OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(wide.as_ptr())).map_err(
                        |e| {
                            MumbleError::NotConnected(format!(
                                "OpenFileMappingW(\"MumbleLink\") failed: {e}. \
                             Is Guild Wars 2 running on this machine?"
                            ))
                        },
                    )?;
                if handle.is_invalid() {
                    return Err(MumbleError::NotConnected(
                        "OpenFileMappingW returned an invalid handle".to_owned(),
                    ));
                }

                let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, MUMBLE_HEADER_LEN);
                if view.Value.is_null() {
                    let _ = CloseHandle(handle);
                    return Err(MumbleError::NotConnected(
                        "MapViewOfFile returned null".to_owned(),
                    ));
                }

                // Build a read-only slice over the mapped region. The
                // pointer is valid for `MUMBLE_HEADER_LEN` bytes (we just
                // asked for that size). The slice borrow ends before
                // UnmapViewOfFile/CloseHandle are called.
                let bytes = std::slice::from_raw_parts(view.Value.cast::<u8>(), MUMBLE_HEADER_LEN);
                let result = parse_header(bytes);

                // Bytes slice goes out of scope here; safe to unmap +
                // close. Errors from these are logged but don't change
                // the result we return — the parse already happened.
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
                let _ = ptr::read_volatile::<u8>(std::ptr::addr_of!(wide[0]) as *const u8);
                result
            }?;
            Ok(snapshot)
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::WindowsMumbleLink;

// ---------------------------------------------------------------------------
// Auto-probe entry point used by main.rs.
// ---------------------------------------------------------------------------

/// Pick the right adapter for the current platform. On success, returns
/// a working reader (which itself may currently report `NotConnected`).
/// On failure, returns the user-facing reason — caller should wire a
/// [`StubMumbleLink`] with that reason instead of aborting startup.
///
/// `disabled` short-circuits to a `Unsupported`-flavoured stub when the
/// user passed `--no-mumble-link`.
///
/// `rescuer` is consulted on macOS when a stale mirror is detected —
/// the supervisor uses it to re-elect a holder leader if the previous
/// one's process died. Pass `None` to opt out (Linux/Windows callers,
/// or `--no-mumble-holder` on macOS).
pub fn probe_default(
    disabled: bool,
    rescuer: Option<Arc<dyn MirrorRescuer>>,
) -> Arc<dyn MumbleLink> {
    if disabled {
        return StubMumbleLink::unsupported("--no-mumble-link flag set").into_arc();
    }

    #[cfg(unix)]
    {
        match FileMumbleLink::probe() {
            Ok(r) => match rescuer {
                Some(rs) => Arc::new(r.with_rescuer(rs)),
                None => Arc::new(r),
            },
            Err(e) => StubMumbleLink::new(format!("auto-probe failed: {e}")).into_arc(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // Rescuer is macOS-only — Windows has no in-bottle holder to re-elect.
        let _ = rescuer;
        match windows_impl::WindowsMumbleLink::probe() {
            Ok(r) => Arc::new(r),
            Err(e) => StubMumbleLink::new(format!("auto-probe failed: {e}")).into_arc(),
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = rescuer;
        StubMumbleLink::unsupported(format!(
            "no Mumble Link adapter for target_os = {}",
            std::env::consts::OS
        ))
        .into_arc()
    }
}

// ---------------------------------------------------------------------------
// Tests — fixture-based (no live shared memory needed).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fully-populated header byte buffer for fixtures. Returns
    /// `MUMBLE_HEADER_LEN` bytes; `ui_tick`, identity, and context can
    /// be tweaked per test.
    fn make_header_bytes(
        ui_tick: u32,
        identity_json: &str,
        player_x: f32,
        player_y: f32,
        map_id: u32,
    ) -> Vec<u8> {
        let mut header = RawMumbleHeader {
            ui_version: 1,
            ui_tick,
            f_avatar_position: [10.0, 20.0, 30.0],
            f_avatar_front: [0.0, 1.0, 0.0],
            f_avatar_top: [0.0, 0.0, 1.0],
            name: [0; 256],
            f_camera_position: [11.0, 21.0, 31.0],
            f_camera_front: [0.0, 1.0, 0.0],
            f_camera_top: [0.0, 0.0, 1.0],
            identity: [0; 256],
            context_len: 0,
            context: [0; 256],
        };

        // "Guild Wars 2" → UTF-16, null-terminated.
        let name_str: Vec<u16> = "Guild Wars 2".encode_utf16().collect();
        header.name[..name_str.len()].copy_from_slice(&name_str);

        // Identity JSON → UTF-16, null-terminated.
        let id_utf16: Vec<u16> = identity_json.encode_utf16().collect();
        assert!(id_utf16.len() < 256);
        header.identity[..id_utf16.len()].copy_from_slice(&id_utf16);

        // Build the GW2 context.
        let ctx = RawGw2Context {
            server_address: [0; 28],
            map_id,
            map_type: 5,
            shard_id: 0,
            instance: 0,
            build_id: 162_000,
            ui_state: 0,
            compass_width: 256,
            compass_height: 256,
            compass_rotation: 0.0,
            player_x,
            player_y,
            map_center_x: 0.0,
            map_center_y: 0.0,
            map_scale: 1.0,
            process_id: 1234,
            mount_index: 0,
            _pad: [0; 3],
        };
        let ctx_bytes = bytemuck::bytes_of(&ctx);
        header.context[..ctx_bytes.len()].copy_from_slice(ctx_bytes);
        header.context_len = u32::try_from(ctx_bytes.len()).expect("ctx fits in u32");

        bytemuck::bytes_of(&header).to_vec()
    }

    #[test]
    fn parses_a_well_formed_header() {
        let bytes = make_header_bytes(
            42,
            r#"{"name":"Hero","profession":1,"spec":62,"race":2,"map_id":15,"world_id":2202,"team_color_id":0,"commander":false,"fov":1.222,"uisz":1}"#,
            12345.5,
            -678.25,
            15,
        );
        let snap = parse_header(&bytes).expect("parse");
        assert_eq!(snap.ui_version, 1);
        assert_eq!(snap.ui_tick, 42);
        assert_eq!(snap.identity.name.as_deref(), Some("Hero"));
        assert_eq!(snap.identity.profession, Some(1));
        assert_eq!(snap.identity.spec, Some(62));
        assert_eq!(snap.identity.race, Some(2));
        assert_eq!(snap.identity.map_id, Some(15));
        assert_eq!(snap.context.map_id, 15);
        assert!((snap.context.player_x - 12345.5).abs() < 1e-6);
        assert!((snap.context.player_y - (-678.25)).abs() < 1e-6);
    }

    #[test]
    fn ui_tick_zero_is_not_connected() {
        let bytes = make_header_bytes(0, "{}", 0.0, 0.0, 0);
        let err = parse_header(&bytes).unwrap_err();
        assert!(matches!(err, MumbleError::NotConnected(_)), "got {err:?}");
    }

    #[test]
    fn rejects_too_small_buffer() {
        let bytes = vec![0u8; 100];
        let err = parse_header(&bytes).unwrap_err();
        assert!(matches!(err, MumbleError::Decode(_)));
    }

    #[test]
    fn empty_identity_returns_default_struct() {
        // Identity left all-zero — should parse to defaults, not error.
        let bytes = make_header_bytes(7, "", 0.0, 0.0, 0);
        let snap = parse_header(&bytes).expect("parse");
        assert!(snap.identity.name.is_none());
        assert!(snap.identity.profession.is_none());
    }

    #[test]
    fn malformed_identity_json_is_decode_error() {
        let bytes = make_header_bytes(7, "{not json", 0.0, 0.0, 0);
        let err = parse_header(&bytes).unwrap_err();
        match err {
            MumbleError::Decode(s) => assert!(s.contains("identity JSON parse"), "got: {s}"),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn stub_no_mumble_link_returns_unsupported() {
        let stub = StubMumbleLink::unsupported("--no-mumble-link flag set");
        let err = stub.snapshot().unwrap_err();
        assert!(matches!(err, MumbleError::Unsupported(_)));
    }

    #[test]
    fn stub_not_connected_returns_not_connected() {
        let stub = StubMumbleLink::new("nothing reachable");
        let err = stub.snapshot().unwrap_err();
        assert!(matches!(err, MumbleError::NotConnected(_)));
    }

    #[test]
    fn raw_header_size_matches_constant() {
        assert_eq!(std::mem::size_of::<RawMumbleHeader>(), MUMBLE_HEADER_LEN);
    }

    #[test]
    fn probe_default_disabled_returns_unsupported_stub() {
        let adapter = probe_default(true, None);
        let err = adapter.snapshot().unwrap_err();
        assert!(matches!(err, MumbleError::Unsupported(_)));
    }

    // ----- Holder mirror file roundtrip ------------------------------------
    //
    // These exercise the file-with-holder-header path (the macOS bottle
    // shape) without involving wine or a real bottle. We write a fixture
    // file to a tempdir, point a `FileMumbleLink` at it, and verify the
    // 16-byte header is correctly stripped before passing the LinkedMem
    // payload through `parse_header`.

    #[cfg(unix)]
    fn write_holder_mirror_fixture(
        path: &std::path::Path,
        write_unix_seconds: u32,
        ui_tick: u32,
    ) -> std::io::Result<()> {
        use std::io::Write;
        let header = holder_format::header_bytes(99, write_unix_seconds);
        let payload_short = make_header_bytes(ui_tick, "{}", 100.0, 200.0, 50);
        // Pad to the full 5460-byte LinkedMem footprint so the file
        // size matches what the real holder writes; reader only looks
        // at the first MUMBLE_HEADER_LEN bytes after the holder header.
        let mut payload = vec![0u8; holder_format::HOLDER_PAYLOAD_LEN];
        payload[..payload_short.len()].copy_from_slice(&payload_short);
        let mut f = std::fs::File::create(path)?;
        f.write_all(&header)?;
        f.write_all(&payload)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_strips_holder_header_and_decodes_linkedmem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mumble.bin");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        write_holder_mirror_fixture(&p, now, 42).expect("write fixture");

        let reader = FileMumbleLink {
            path: p,
            rescuer: None,
        };
        let snap = reader.snapshot().expect("decode");
        assert_eq!(snap.ui_tick, 42);
        assert!((snap.context.player_x - 100.0).abs() < 1e-6);
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_rejects_stale_holder_mirror() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mumble.bin");
        // Timestamp 1 hour in the past — well past HOLDER_STALE_AFTER_SECONDS.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        let stale = now - 3600;
        write_holder_mirror_fixture(&p, stale, 100).expect("write fixture");

        let reader = FileMumbleLink {
            path: p,
            rescuer: None,
        };
        let err = reader.snapshot().expect_err("expected stale rejection");
        match err {
            MumbleError::NotConnected(s) => {
                assert!(s.contains("stale"), "expected 'stale' substring; got: {s}");
            }
            other => panic!("expected NotConnected, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_treats_bare_linkedmem_as_no_holder() {
        // Linux /dev/shm/MumbleLink path — file starts directly with the
        // LinkedMem (no holder magic). Reader should bypass header
        // stripping and parse from offset 0.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("MumbleLink");
        let bytes = make_header_bytes(7, "{}", 1.0, 2.0, 3);
        std::fs::write(&p, &bytes).expect("write fixture");

        let reader = FileMumbleLink {
            path: p,
            rescuer: None,
        };
        let snap = reader.snapshot().expect("decode");
        assert_eq!(snap.ui_tick, 7);
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_overrides_uitick_zero_message_for_holder_mirror() {
        // Fresh holder mirror (timestamp = now) but ui_tick=0 means the
        // helper is alive and GW2 just hasn't started writing yet. The
        // snapshot path should override the generic "log a character in"
        // message with one that mentions the helper is running.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mumble.bin");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        write_holder_mirror_fixture(&p, now, 0).expect("write fixture");

        let reader = FileMumbleLink {
            path: p,
            rescuer: None,
        };
        let err = reader.snapshot().expect_err("ui_tick=0 should error");
        match err {
            MumbleError::NotConnected(s) => {
                assert!(
                    s.contains("helper is running"),
                    "expected 'helper is running' substring; got: {s}"
                );
                assert!(
                    s.contains("logged in"),
                    "expected 'logged in' substring; got: {s}"
                );
            }
            other => panic!("expected NotConnected, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_stale_message_mentions_doctor_and_path() {
        // Refines the existing stale test: ensure the new message points
        // the user at `gw2-mcp doctor` and includes the mirror file path,
        // so support threads can show the broken state at a glance.
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mumble.bin");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        write_holder_mirror_fixture(&p, now - 3600, 100).expect("write fixture");

        let reader = FileMumbleLink {
            path: p.clone(),
            rescuer: None,
        };
        let err = reader.snapshot().expect_err("expected stale rejection");
        let MumbleError::NotConnected(s) = err else {
            panic!("expected NotConnected");
        };
        assert!(s.contains("gw2-mcp doctor"), "got: {s}");
        assert!(s.contains(&p.display().to_string()), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn file_reader_consults_rescuer_on_stale_mirror() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct CountingRescuer {
            calls: AtomicUsize,
            succeed: AtomicBool,
        }
        impl MirrorRescuer for CountingRescuer {
            fn try_rescue(&self) -> bool {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.succeed.load(Ordering::SeqCst)
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mumble.bin");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        write_holder_mirror_fixture(&p, now - 3600, 100).expect("write fixture");

        // Failing rescue: caller sees the existing "crashed; restart" message.
        let rescuer = Arc::new(CountingRescuer {
            calls: AtomicUsize::new(0),
            succeed: AtomicBool::new(false),
        });
        let reader = FileMumbleLink {
            path: p.clone(),
            rescuer: Some(rescuer.clone() as Arc<dyn MirrorRescuer>),
        };
        let err = reader
            .snapshot()
            .expect_err("stale + failed-rescue should error");
        let MumbleError::NotConnected(s) = err else {
            panic!("expected NotConnected");
        };
        assert!(s.contains("crashed"), "expected crashed message; got: {s}");
        assert_eq!(
            rescuer.calls.load(Ordering::SeqCst),
            1,
            "rescuer called once"
        );

        // Succeeding rescue: caller sees the transient "restarting" message.
        rescuer.succeed.store(true, Ordering::SeqCst);
        let err = reader
            .snapshot()
            .expect_err("stale + ok-rescue still errors this call");
        let MumbleError::NotConnected(s) = err else {
            panic!("expected NotConnected");
        };
        assert!(
            s.contains("spawned a fresh in-bottle helper"),
            "expected restart message; got: {s}"
        );
        assert!(s.contains("retry"), "should advise retry; got: {s}");
        assert_eq!(
            rescuer.calls.load(Ordering::SeqCst),
            2,
            "rescuer called again"
        );
    }
}
