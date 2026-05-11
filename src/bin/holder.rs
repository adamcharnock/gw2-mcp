//! `gw2-mcp-holder.exe` — in-bottle Mumble Link holder.
//!
//! GW2's Mumble Link writer is *opener-only*: it calls `OpenFileMappingW`
//! and writes if the named mapping already exists, but never creates it
//! itself. On Windows the Mumble voice client (or anything else) creates
//! it. On Linux/Wine, Burrito and jokolink play that role. On macOS no
//! one does — until this binary is launched in the bottle.
//!
//! Procedure:
//!   1. `CreateFileMappingW(name="MumbleLink", size=5460, RW)` and hold
//!      the handle for the process lifetime.
//!   2. Map a read view (we only read after GW2 starts writing).
//!   3. Every ~50ms, copy the 5460 bytes out, prepend the metadata
//!      header (magic + pid + write timestamp), atomically rewrite the
//!      mirror file at `--bin-path`. Native gw2-mcp on macOS reads from
//!      that file via memmap.
//!   4. On process exit (Ctrl-C, parent death) the OS reclaims the
//!      mapping; the macOS supervisor handles relaunch on next startup.
//!
//! Non-Windows builds compile to a stub `main()` that exits with a
//! message — keeps the workspace target list flat without per-target
//! Cargo gymnastics.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "gw2-mcp-holder is a Windows-only helper. Run it inside a CrossOver/Wine bottle, \
         not natively on Linux/macOS. The macOS gw2-mcp binary launches it for you via cxstart."
    );
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::ptr::NonNull;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use gw2_mcp::adapters::holder_format::{
        HOLDER_FILE_LEN, HOLDER_HEADER_LEN, HOLDER_PAYLOAD_LEN, header_bytes,
    };
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE,
    };
    use windows::core::PCWSTR;

    const MAPPING_NAME: &str = "MumbleLink";
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    #[derive(Parser, Debug)]
    #[command(
        name = "gw2-mcp-holder",
        version,
        about = "In-bottle Mumble Link holder for GW2 on macOS/CrossOver"
    )]
    struct Cli {
        /// Where to mirror the LinkedMem snapshot. Path is in *Windows*
        /// convention (e.g. `C:\users\Public\gw2-mcp\mumble.bin`) — must
        /// resolve to a host-visible location inside the bottle's drive_c.
        #[arg(long)]
        bin_path: PathBuf,

        /// Poll interval in milliseconds. Default 50ms ≈ 20Hz, well below
        /// GW2's write rate (~60Hz) and good enough for nav-tool latency.
        #[arg(long, default_value_t = 50)]
        poll_ms: u64,

        /// Print verbose state every N polls (0 = silent past startup).
        #[arg(long, default_value_t = 0)]
        log_every: u32,
    }

    pub fn run() -> anyhow::Result<()> {
        let cli = Cli::parse();
        let poll = if cli.poll_ms == 0 {
            POLL_INTERVAL
        } else {
            Duration::from_millis(cli.poll_ms)
        };

        eprintln!(
            "[gw2-mcp-holder] starting; bin_path={:?} poll={:?}",
            cli.bin_path, poll
        );

        if let Some(parent) = cli.bin_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Pre-create the mapping. If GW2 was started first this returns
        // the existing handle (`ERROR_ALREADY_EXISTS` is set but ignored);
        // either way we have a valid handle.
        let map = create_mapping()?;
        let view = map_view(map)?;
        eprintln!("[gw2-mcp-holder] mapping live; mirror file is {HOLDER_FILE_LEN} bytes");

        let pid = std::process::id();
        let mut last_tick: u32 = u32::MAX;
        let mut iter: u32 = 0;
        loop {
            let payload = read_payload(view);
            let ts = unix_seconds_now();
            write_mirror(&cli.bin_path, pid, ts, &payload)?;

            let tick = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
            if cli.log_every > 0 && iter % cli.log_every == 0 && tick != last_tick {
                eprintln!(
                    "[gw2-mcp-holder] iter={iter} tick={tick} ts={ts} (last_tick={last_tick})"
                );
            }
            last_tick = tick;
            iter = iter.wrapping_add(1);
            std::thread::sleep(poll);
        }
    }

    #[allow(unsafe_code)]
    fn create_mapping() -> anyhow::Result<HANDLE> {
        let mut wide: Vec<u16> = MAPPING_NAME.encode_utf16().collect();
        wide.push(0);
        // SAFETY: the FFI call only reads from `wide` for the duration of
        // the call (CreateFileMappingW copies the name into kernel space)
        // and `INVALID_HANDLE_VALUE` is the documented sentinel for
        // "back this mapping with the system pagefile, no underlying file".
        // Returns an owned HANDLE that the caller closes (or which the OS
        // reclaims on process exit — this binary holds it for life).
        let h = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                HOLDER_PAYLOAD_LEN as u32,
                PCWSTR(wide.as_ptr()),
            )?
        };
        if h.is_invalid() {
            anyhow::bail!("CreateFileMappingW returned invalid handle");
        }
        Ok(h)
    }

    #[allow(unsafe_code)]
    fn map_view(map: HANDLE) -> anyhow::Result<NonNull<c_void>> {
        // SAFETY: `map` is a live HANDLE returned by CreateFileMappingW
        // (validated by the caller's `is_invalid` check). We request a
        // read-only view of the full payload — the kernel verifies the
        // size against the underlying section. The returned pointer is
        // valid for HOLDER_PAYLOAD_LEN bytes until UnmapViewOfFile is
        // called (which we defer to OS-level cleanup at process exit).
        let view: MEMORY_MAPPED_VIEW_ADDRESS =
            unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, HOLDER_PAYLOAD_LEN) };
        NonNull::new(view.Value).ok_or_else(|| anyhow::anyhow!("MapViewOfFile returned null"))
    }

    #[allow(unsafe_code)]
    fn read_payload(view: NonNull<c_void>) -> [u8; HOLDER_PAYLOAD_LEN] {
        // SAFETY: `view` is a non-null pointer into a kernel mapping of
        // exactly HOLDER_PAYLOAD_LEN bytes (returned by `map_view`). The
        // destination is a fully-initialized stack array of the same size.
        // GW2 writes the struct every frame; the consumer side
        // double-checks ui_tick to detect torn reads, so we don't
        // synchronize here.
        let mut out = [0u8; HOLDER_PAYLOAD_LEN];
        unsafe {
            std::ptr::copy_nonoverlapping(
                view.as_ptr() as *const u8,
                out.as_mut_ptr(),
                HOLDER_PAYLOAD_LEN,
            );
        }
        out
    }

    fn write_mirror(
        path: &std::path::Path,
        pid: u32,
        write_unix_seconds: u32,
        payload: &[u8; HOLDER_PAYLOAD_LEN],
    ) -> std::io::Result<()> {
        // Truncate-and-rewrite is fine for a 5476-byte file polled at 20Hz.
        // FILE_SHARE_READ so the macOS reader can hold a memmap concurrently.
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)?;
        let header = header_bytes(pid, write_unix_seconds);
        debug_assert_eq!(header.len(), HOLDER_HEADER_LEN);
        f.write_all(&header)?;
        f.write_all(payload)?;
        Ok(())
    }

    fn unix_seconds_now() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0))
    }
}
