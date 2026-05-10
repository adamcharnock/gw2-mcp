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

use std::path::PathBuf;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MumbleError {
    /// The shared-memory region was found but contains no live game data
    /// (`ui_tick == 0`), or the region itself wasn't found at all.
    #[error(
        "Mumble Link is not connected — start Guild Wars 2 on the same machine running this MCP \
         server, log a character in, and try again. (Detail: {0})"
    )]
    NotConnected(String),

    /// We're on a platform with no implementation. The error message
    /// names the platform and gives the user something actionable.
    #[error("Mumble Link is not supported in this configuration: {0}")]
    Unsupported(String),

    #[error("Mumble Link I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Mumble Link decode error: {0}")]
    Decode(String),
}

/// Read-only port for the live Mumble Link state.
pub trait MumbleLink: Send + Sync + 'static {
    fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError>;
}

// ---------------------------------------------------------------------------
// Public snapshot type — what callers see.
// ---------------------------------------------------------------------------

/// What we hand back to the service layer. This is *not* the raw 5876-
/// byte struct — only the fields we actually use, with the GW2 context
/// block and identity JSON pre-parsed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MumbleSnapshot {
    /// Mumble protocol version (always 1 for GW2).
    pub ui_version: u32,
    /// Frame counter; if 0 the client isn't writing yet.
    pub ui_tick: u32,
    /// 3D world position in metres, Z-up.
    pub avatar_position: [f32; 3],
    /// Unit vector for the direction the avatar is facing.
    pub avatar_front: [f32; 3],
    /// 3D camera position in metres.
    pub camera_position: [f32; 3],
    /// Unit vector for the camera-look direction.
    pub camera_front: [f32; 3],
    /// Parsed `identity` JSON (character name, profession id, …).
    pub identity: MumbleIdentity,
    /// Parsed GW2-specific context block.
    pub context: MumbleContext,
}

/// `identity` JSON shape published by GW2. Fields are left optional
/// because Mumble Link's identity is a freeform string and the publisher
/// has historically renamed keys across patches; we'd rather render
/// `null` than reject a snapshot.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MumbleIdentity {
    #[serde(default)]
    pub name: Option<String>,
    /// 1..=9 — see `domain::bearing` neighbouring code in service.rs for the
    /// profession byte → name map.
    #[serde(default)]
    pub profession: Option<u8>,
    /// Elite specialisation id (0 if none equipped).
    #[serde(default)]
    pub spec: Option<u32>,
    /// Race id (1..=5 in current patches).
    #[serde(default)]
    pub race: Option<u8>,
    #[serde(default)]
    pub map_id: Option<u32>,
    #[serde(default)]
    pub world_id: Option<u64>,
    #[serde(default)]
    pub team_color_id: Option<u32>,
    #[serde(default)]
    pub commander: Option<bool>,
    #[serde(default)]
    pub fov: Option<f32>,
    #[serde(default)]
    pub uisz: Option<u8>,
}

/// GW2 binary `context` block — the only bit of "context" we care about.
/// `player_x` and `player_y` are the **2D map coordinates** the LLM wants
/// for navigation; they are *not* the same as the 3D position above.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MumbleContext {
    pub server_address: [u8; 28],
    pub map_id: u32,
    pub map_type: u32,
    pub shard_id: u32,
    pub instance: u32,
    pub build_id: u32,
    pub ui_state: u32,
    pub compass_width: u16,
    pub compass_height: u16,
    pub compass_rotation: f32,
    /// 2D map x — the coord to use for navigation.
    pub player_x: f32,
    /// 2D map y — the coord to use for navigation.
    pub player_y: f32,
    pub map_center_x: f32,
    pub map_center_y: f32,
    pub map_scale: f32,
    pub process_id: u32,
    pub mount_index: u8,
}

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

/// Raw bytes for the GW2 context portion (256 bytes).
const GW2_CONTEXT_LEN: usize = 256;

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
    let context = parse_gw2_context(&header.context, header.context_len)?;

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

fn parse_gw2_context(buf: &[u8; 256], context_len: u32) -> Result<MumbleContext, MumbleError> {
    // The GW2 context is fixed-size in practice (~85 bytes of real
    // fields). Our `RawGw2Context` is 88 bytes because Rust's natural
    // alignment pads out `mount_index: u8` to a 4-byte boundary; the
    // last 3 bytes are explicit trailing padding (`_pad`) that we never
    // read. `context_len` from the client tells us how many *real* bytes
    // are populated — anything ≥ the field count is enough.
    const REAL_FIELD_BYTES: usize = 85;
    let usable = (context_len as usize).min(GW2_CONTEXT_LEN);
    if usable < REAL_FIELD_BYTES {
        return Err(MumbleError::Decode(format!(
            "GW2 context block too small: {usable} bytes available, need {REAL_FIELD_BYTES}"
        )));
    }
    let struct_size = std::mem::size_of::<RawGw2Context>();
    let raw: &RawGw2Context = bytemuck::from_bytes(&buf[..struct_size]);
    Ok(MumbleContext {
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
    })
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
/// this returns an empty Vec.
fn unix_candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Linux + Steam Proton: tmpfs handle exposed by the Wine server.
    out.push(PathBuf::from("/dev/shm/MumbleLink"));
    // macOS Wine bottles vary; we *probe* common spots but make no promises.
    if cfg!(target_os = "macos")
        && let Ok(home) = std::env::var("HOME")
    {
        for suffix in [
            // CrossOver default bottles dir
            "Library/Application Support/CrossOver/Bottles",
            // Whisky (legacy + new) — bottle dir varies by version
            "Library/Containers/com.isaacmarovitz.Whisky/Bottles",
            "Library/Application Support/Whisky/Bottles",
        ] {
            let dir = PathBuf::from(&home).join(suffix);
            if dir.is_dir()
                && let Ok(entries) = std::fs::read_dir(&dir)
            {
                for e in entries.flatten() {
                    out.push(e.path().join("dosdevices/MumbleLink"));
                }
            }
        }
    }
    out
}

/// File-backed reader: opens (and re-opens) `path` on every snapshot.
/// memmap2 internally uses platform mmap calls (no `unsafe` exposed in
/// our codebase) and this works for `/dev/shm/MumbleLink` on Linux.
#[cfg(unix)]
pub struct FileMumbleLink {
    path: PathBuf,
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
            let reader = Self { path: p.clone() };
            match reader.snapshot() {
                // Either it's working OR `NotConnected` (the file
                // exists but `ui_tick==0` because the player hasn't
                // logged in yet). Either way, adopt this path — the
                // next call picks up live data.
                Ok(_) | Err(MumbleError::NotConnected(_)) => return Ok(reader),
                Err(_) => {}
            }
        }
        Err(MumbleError::NotConnected(format!(
            "no MumbleLink shared-memory region found at any candidate path. \
             Probed: {:?}. On Linux/Wine, ensure GW2 is running. On macOS, \
             only CrossOver/Whisky setups can expose this; Parallels VMs cannot.",
            unix_candidate_paths()
        )))
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
        if mmap.len() < MUMBLE_HEADER_LEN {
            return Err(MumbleError::Decode(format!(
                "mapped region too small: {} bytes (need {})",
                mmap.len(),
                MUMBLE_HEADER_LEN
            )));
        }
        parse_header(&mmap[..MUMBLE_HEADER_LEN])
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
pub fn probe_default(disabled: bool) -> Arc<dyn MumbleLink> {
    if disabled {
        return StubMumbleLink::unsupported("--no-mumble-link flag set").into_arc();
    }

    #[cfg(unix)]
    {
        match FileMumbleLink::probe() {
            Ok(r) => Arc::new(r),
            Err(e) => StubMumbleLink::new(format!("auto-probe failed: {e}")).into_arc(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        match windows_impl::WindowsMumbleLink::probe() {
            Ok(r) => Arc::new(r),
            Err(e) => StubMumbleLink::new(format!("auto-probe failed: {e}")).into_arc(),
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
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
        let adapter = probe_default(true);
        let err = adapter.snapshot().unwrap_err();
        assert!(matches!(err, MumbleError::Unsupported(_)));
    }
}
