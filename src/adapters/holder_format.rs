//! On-disk file format used by the in-bottle Mumble Link holder
//! (`gw2-mcp-holder.exe`) to mirror snapshots out to a host-visible file.
//!
//! The holder is the writer; `FileMumbleLink` (when probing macOS bottles)
//! is the reader. Format is a small fixed header followed by the raw 5460
//! bytes of GW2's `LinkedMem`.
//!
//! ```text
//! offset  size  field
//! 0       8     magic = b"GW2MUMBL"
//! 8       4     holder_pid (u32 LE)         — debug aid; reader only logs
//! 12      4     write_unix_seconds (u32 LE) — staleness check (>5s = dead)
//! 16      5460  LinkedMem (raw, as published by GW2 via Mumble Link)
//! ```
//!
//! Format is shared verbatim between the holder bin (writer) and
//! [`crate::adapters::mumble_link::FileMumbleLink`] (reader). `u32` for
//! the timestamp is enough through 2106 and only delta-from-now matters.

pub const HOLDER_MAGIC: &[u8; 8] = b"GW2MUMBL";

/// Length of the metadata header preceding the `LinkedMem` payload.
pub const HOLDER_HEADER_LEN: usize = 16;

/// Length of the `LinkedMem` payload itself, as published by GW2 via Mumble
/// Link. This matches the widely-documented 5460-byte total of the spec
/// struct (header + identity + context + description). We mirror the
/// whole thing even though we only parse the first portion in the reader.
pub const HOLDER_PAYLOAD_LEN: usize = 5460;

/// Total file size: header + payload.
pub const HOLDER_FILE_LEN: usize = HOLDER_HEADER_LEN + HOLDER_PAYLOAD_LEN;

/// Maximum age (seconds) of `write_unix_seconds` before the reader treats
/// the file as stale and refuses to publish a snapshot. Tuned to be
/// comfortably larger than the holder's poll interval (50ms) plus typical
/// scheduler jitter, but small enough to detect a dead holder within a
/// few seconds.
pub const HOLDER_STALE_AFTER_SECONDS: u64 = 5;

/// Path under the bottle's `drive_c/users/Public/` where the holder
/// binary is installed and writes its mirror file. Kept under
/// `users/Public` so it's host-visible at `<bottle>/drive_c/users/Public/gw2-mcp/`
/// and ours-only — we don't pollute any application's per-user dir.
pub const HOLDER_SUBDIR: &str = "users/Public/gw2-mcp";

/// Name of the holder binary as installed inside the bottle.
pub const HOLDER_EXE_NAME: &str = "holder.exe";

/// Name of the mirror file the holder writes to.
pub const HOLDER_BIN_NAME: &str = "mumble.bin";

/// Build the on-disk header bytes. Caller appends the 5460-byte payload.
#[must_use]
pub fn header_bytes(holder_pid: u32, write_unix_seconds: u32) -> [u8; HOLDER_HEADER_LEN] {
    let mut out = [0u8; HOLDER_HEADER_LEN];
    out[..8].copy_from_slice(HOLDER_MAGIC);
    out[8..12].copy_from_slice(&holder_pid.to_le_bytes());
    out[12..16].copy_from_slice(&write_unix_seconds.to_le_bytes());
    out
}

#[derive(Debug)]
pub struct HolderHeader {
    pub holder_pid: u32,
    pub write_unix_seconds: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    #[error("file too small: {actual} bytes, need at least {expected}")]
    TooSmall { actual: usize, expected: usize },
    #[error("bad magic: expected {expected:?}, got {got:?}")]
    BadMagic { expected: [u8; 8], got: [u8; 8] },
}

/// Parse the 16-byte header at the start of a holder mirror file.
pub fn parse_header(bytes: &[u8]) -> Result<HolderHeader, HeaderError> {
    if bytes.len() < HOLDER_HEADER_LEN {
        return Err(HeaderError::TooSmall {
            actual: bytes.len(),
            expected: HOLDER_HEADER_LEN,
        });
    }
    let mut got = [0u8; 8];
    got.copy_from_slice(&bytes[..8]);
    if &got != HOLDER_MAGIC {
        return Err(HeaderError::BadMagic {
            expected: *HOLDER_MAGIC,
            got,
        });
    }
    let mut pid_b = [0u8; 4];
    pid_b.copy_from_slice(&bytes[8..12]);
    let mut ts_b = [0u8; 4];
    ts_b.copy_from_slice(&bytes[12..16]);
    Ok(HolderHeader {
        holder_pid: u32::from_le_bytes(pid_b),
        write_unix_seconds: u32::from_le_bytes(ts_b),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        let bytes = header_bytes(4242, 1_700_000_000);
        let h = parse_header(&bytes).expect("parse");
        assert_eq!(h.holder_pid, 4242);
        assert_eq!(h.write_unix_seconds, 1_700_000_000);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = header_bytes(1, 1);
        bytes[0] = b'X';
        assert!(matches!(
            parse_header(&bytes),
            Err(HeaderError::BadMagic { .. })
        ));
    }

    #[test]
    fn rejects_too_small() {
        assert!(matches!(
            parse_header(&[0u8; 8]),
            Err(HeaderError::TooSmall { .. })
        ));
    }

    #[test]
    fn file_len_is_header_plus_payload() {
        assert_eq!(HOLDER_FILE_LEN, HOLDER_HEADER_LEN + HOLDER_PAYLOAD_LEN);
    }
}
