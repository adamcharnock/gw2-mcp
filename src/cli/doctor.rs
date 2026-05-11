//! `gw2-mcp doctor` — platform diagnostics for the macOS Mumble Link
//! holder path.
//!
//! Runs a sequence of named checks against the live filesystem and
//! prints a structured report. Exits non-zero if any check fails so
//! shell pipelines (`gw2-mcp doctor && launch-claude`) work as expected.
//!
//! Each check is a small function that returns a [`CheckResult`]; the
//! report renderer is the only thing that touches stdout. This split
//! lets us unit-test the diagnostic logic with synthetic inputs while
//! `run` stays a thin orchestrator.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::adapters::bottle_discovery::{self, Bottle};
use crate::adapters::{holder_format, mumble_link};

const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RESET: &str = "\x1b[0m";

/// Default install location of `cxstart`, the `CrossOver` launcher.
const DEFAULT_CXSTART_PATH: &str =
    "/Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin/cxstart";

/// Default install location of the Whisky app bundle.
const DEFAULT_WHISKY_APP_PATH: &str = "/Applications/Whisky.app";

/// Mirror file mtime threshold for "fresh". The holder writes every
/// ~50ms so anything older than this is symptomatic of a crashed
/// helper. A bit looser than [`holder_format::HOLDER_STALE_AFTER_SECONDS`]
/// so we don't flake when the holder is mid-spawn.
const MIRROR_FRESH_SECONDS: u64 = 10;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CheckKind {
    Ok,
    Warn,
    Fail,
}

/// One named check's outcome.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub kind: CheckKind,
    pub title: String,
    pub detail: String,
    /// Multi-line hint shown indented below the title when not `Ok`.
    pub hint: Option<String>,
}

impl CheckResult {
    fn ok(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Ok,
            title: title.into(),
            detail: detail.into(),
            hint: None,
        }
    }
    fn warn(title: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Warn,
            title: title.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    fn fail(title: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Fail,
            title: title.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Environment-derived configuration for the doctor. Built once at
/// `run()` and passed to the checks.
#[derive(Debug, Clone)]
pub struct DoctorContext {
    pub cxstart_path: PathBuf,
    pub whisky_app_path: PathBuf,
    pub holder_source: PathBuf,
    pub bottle_override: Option<String>,
}

impl DoctorContext {
    /// Build from the current process's env. Picks up `GW2_BOTTLE` for
    /// the bottle override and `current_exe()` for the holder source.
    pub fn from_env() -> Self {
        let holder_source = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
            .map_or_else(
                || PathBuf::from("gw2-mcp-holder.exe"),
                |dir| dir.join("gw2-mcp-holder.exe"),
            );
        Self {
            cxstart_path: PathBuf::from(DEFAULT_CXSTART_PATH),
            whisky_app_path: PathBuf::from(DEFAULT_WHISKY_APP_PATH),
            holder_source,
            bottle_override: std::env::var("GW2_BOTTLE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

/// Run the diagnostic suite and write a report to stdout. Returns
/// exit code 1 when any check fails, 0 otherwise.
pub fn run() -> ExitCode {
    let ctx = DoctorContext::from_env();
    let results = collect_results(&ctx);
    let ansi = std::io::stdout().is_terminal();
    let mut any_fail = false;
    for r in &results {
        println!("{}", render(r, ansi));
        if r.kind == CheckKind::Fail {
            any_fail = true;
        }
    }
    println!();
    println!("{}", verdict_line(&results, ansi));
    if any_fail {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn collect_results(ctx: &DoctorContext) -> Vec<CheckResult> {
    if !cfg!(target_os = "macos") {
        return vec![CheckResult::ok(
            "Platform",
            "gw2-mcp doctor is macOS-only. On Linux/Windows the in-bottle \
             helper is not used — Mumble Link is read directly.",
        )];
    }
    let mut out = Vec::new();
    out.push(check_crossover(&ctx.cxstart_path));
    out.push(check_whisky(&ctx.whisky_app_path));

    let bottles = bottle_discovery::discover_bottles();
    out.push(check_bottles_enumerated(&bottles));

    let selected = pick_for_report(&bottles, ctx.bottle_override.as_deref());
    out.push(check_selected_bottle(
        selected.as_ref(),
        &bottles,
        ctx.bottle_override.as_deref(),
    ));

    out.push(check_holder_source(&ctx.holder_source));

    if let Some(bottle) = selected {
        out.push(check_holder_installed(&bottle, &ctx.holder_source));
        let mirror = bottle
            .root
            .join("drive_c")
            .join(holder_format::HOLDER_SUBDIR)
            .join(holder_format::HOLDER_BIN_NAME);
        let fresh = check_mirror_fresh(&mirror);
        let mirror_is_fresh = fresh.kind == CheckKind::Ok;
        out.push(fresh);
        if mirror_is_fresh {
            out.push(check_payload_populated(&mirror));
        }
    }

    out
}

// ----- individual checks (testable in isolation) ---------------------------

pub fn check_crossover(cxstart_path: &Path) -> CheckResult {
    if cxstart_path.is_file() {
        CheckResult::ok(
            "CrossOver detected",
            format!("cxstart found at {}", cxstart_path.display()),
        )
    } else {
        CheckResult::warn(
            "CrossOver not detected",
            format!("cxstart not found at {}", cxstart_path.display()),
            "Install CrossOver from https://www.codeweavers.com/crossover, \
             or use Whisky instead (https://getwhisky.app/). One of the two \
             is required to run Guild Wars 2 on macOS.",
        )
    }
}

pub fn check_whisky(whisky_app_path: &Path) -> CheckResult {
    if whisky_app_path.is_dir() {
        CheckResult::ok(
            "Whisky detected",
            format!("Whisky.app found at {}", whisky_app_path.display()),
        )
    } else {
        // Informational — Whisky is optional if CrossOver is present.
        CheckResult::ok(
            "Whisky not installed",
            "Whisky.app not present (this is fine if you use CrossOver).".to_owned(),
        )
    }
}

pub fn check_bottles_enumerated(bottles: &[Bottle]) -> CheckResult {
    if bottles.is_empty() {
        return CheckResult::fail(
            "No bottles found",
            "No CrossOver or Whisky bottles were discovered on this system.".to_owned(),
            "Install Guild Wars 2 in a CrossOver or Whisky bottle. The bottle \
             does not need a specific name — gw2-mcp will find it as long as \
             Gw2-64.exe is at the standard 'Program Files' location.",
        );
    }
    let summary = bottles
        .iter()
        .map(|b| {
            format!(
                "{} ({}){}",
                b.name,
                b.runner.as_str(),
                if b.has_gw2 { " — GW2 detected" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    CheckResult::ok(format!("Found {} bottle(s)", bottles.len()), summary)
}

pub fn check_selected_bottle(
    selected: Option<&Bottle>,
    bottles: &[Bottle],
    env_override: Option<&str>,
) -> CheckResult {
    match (selected, env_override) {
        (Some(b), _) => CheckResult::ok(
            "Bottle selected",
            format!("{} ({}) at {}", b.name, b.runner.as_str(), b.root.display()),
        ),
        (None, Some(name)) => {
            let available: Vec<String> = bottles
                .iter()
                .map(|b| format!("{} ({})", b.name, b.runner.as_str()))
                .collect();
            CheckResult::fail(
                "Bottle override does not match",
                format!("GW2_BOTTLE={name:?} did not match any bottle."),
                format!(
                    "Available bottles: {available:?}. Either fix GW2_BOTTLE to \
                     match one of these names exactly, or unset it to use \
                     auto-discovery."
                ),
            )
        }
        (None, None) => CheckResult::fail(
            "No bottle with GW2 found",
            "Auto-discovery did not find a bottle containing Gw2-64.exe.".to_owned(),
            "Install GW2 in a CrossOver/Whisky bottle, or — if it is installed \
             in a non-standard location — set GW2_BOTTLE=<your-bottle-name> \
             to pin it explicitly."
                .to_owned(),
        ),
    }
}

pub fn check_holder_source(path: &Path) -> CheckResult {
    if path.is_file() {
        CheckResult::ok(
            "Holder binary present",
            format!("gw2-mcp-holder.exe found at {}", path.display()),
        )
    } else {
        CheckResult::fail(
            "Holder binary missing",
            format!("Expected gw2-mcp-holder.exe at {}", path.display()),
            "Reinstall gw2-mcp from the release tarball — it ships both \
             `gw2-mcp` and `gw2-mcp-holder.exe` side-by-side. If you built \
             from source, run `cargo build --release --target \
             x86_64-pc-windows-gnu --bin gw2-mcp-holder` and copy the .exe \
             next to your gw2-mcp binary.",
        )
    }
}

pub fn check_holder_installed(bottle: &Bottle, source: &Path) -> CheckResult {
    let dest = bottle
        .root
        .join("drive_c")
        .join(holder_format::HOLDER_SUBDIR)
        .join(holder_format::HOLDER_EXE_NAME);
    if !dest.is_file() {
        return CheckResult::warn(
            "Holder not yet installed in bottle",
            format!("Expected at {}", dest.display()),
            "The supervisor copies it on first launch — start gw2-mcp once and \
             re-run doctor. If you never run gw2-mcp, this is informational.",
        );
    }
    if !source.is_file() {
        return CheckResult::warn(
            "Cannot compare installed holder to source",
            format!("Source binary missing at {}", source.display()),
            "Reinstall gw2-mcp from the release tarball to restore the source binary.",
        );
    }
    match (file_sha256(source), file_sha256(&dest)) {
        (Ok(a), Ok(b)) if a == b => CheckResult::ok(
            "Holder installed in bottle",
            format!("Up to date at {}", dest.display()),
        ),
        (Ok(_), Ok(_)) => CheckResult::warn(
            "Holder in bottle is stale",
            format!("sha256 mismatch between source and {}", dest.display()),
            "Restart gw2-mcp — the supervisor will replace the installed copy \
             on next launch.",
        ),
        (Err(e), _) | (_, Err(e)) => CheckResult::warn(
            "Could not hash holder binaries",
            format!("sha256 failed: {e}"),
            "This is non-fatal; the supervisor still works.",
        ),
    }
}

pub fn check_mirror_fresh(mirror_path: &Path) -> CheckResult {
    if !mirror_path.is_file() {
        return CheckResult::warn(
            "Mumble Link mirror missing",
            format!("Expected at {}", mirror_path.display()),
            "Start gw2-mcp — the in-bottle helper will create the mirror file. \
             If you have started gw2-mcp and the file still does not appear, \
             the helper may be failing to launch (see other checks).",
        );
    }
    let age = match mirror_path.metadata().and_then(|m| m.modified()) {
        Ok(mtime) => std::time::SystemTime::now()
            .duration_since(mtime)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX),
        Err(e) => {
            return CheckResult::warn(
                "Cannot stat Mumble Link mirror",
                format!("{e}"),
                "File may be in an inconsistent state; restart gw2-mcp.",
            );
        }
    };
    if age <= MIRROR_FRESH_SECONDS {
        CheckResult::ok(
            "Mumble Link mirror is fresh",
            format!("{} (mtime {age}s ago)", mirror_path.display()),
        )
    } else {
        CheckResult::warn(
            "Mumble Link mirror is stale",
            format!(
                "{} last written {age}s ago (threshold: {MIRROR_FRESH_SECONDS}s)",
                mirror_path.display()
            ),
            "The in-bottle helper appears to have crashed. Restart gw2-mcp \
             to relaunch it.",
        )
    }
}

pub fn check_payload_populated(mirror_path: &Path) -> CheckResult {
    let bytes = match std::fs::read(mirror_path) {
        Ok(b) => b,
        Err(e) => {
            return CheckResult::warn(
                "Cannot read Mumble Link mirror",
                format!("{e}"),
                "Check filesystem permissions on the bottle's Public dir.",
            );
        }
    };
    if bytes.len() < holder_format::HOLDER_HEADER_LEN + mumble_link::MUMBLE_HEADER_LEN {
        return CheckResult::warn(
            "Mumble Link mirror is truncated",
            format!(
                "File is {} bytes; expected at least {}",
                bytes.len(),
                holder_format::HOLDER_HEADER_LEN + mumble_link::MUMBLE_HEADER_LEN
            ),
            "Restart gw2-mcp to recreate the mirror with the correct size.",
        );
    }
    let payload = &bytes[holder_format::HOLDER_HEADER_LEN..];
    match mumble_link::parse_header(&payload[..mumble_link::MUMBLE_HEADER_LEN]) {
        Ok(snap) => CheckResult::ok(
            "Mumble Link payload is live",
            format!(
                "ui_tick={}, map_id={}, character={}",
                snap.ui_tick,
                snap.context.map_id,
                snap.identity.name.as_deref().unwrap_or("(none)")
            ),
        ),
        Err(crate::ports::MumbleError::NotConnected(_)) => CheckResult::warn(
            "Mumble Link mapping not yet populated",
            "The in-bottle helper is writing, but GW2 has not started writing \
             live frames into the shared mapping."
                .to_owned(),
            "Launch Guild Wars 2 in your bottle and log a character in, then \
             re-run doctor.",
        ),
        Err(e) => CheckResult::warn(
            "Mumble Link payload failed to decode",
            format!("{e}"),
            "Restart gw2-mcp; if this persists, file a bug report including \
             the doctor output and gw2-mcp version.",
        ),
    }
}

// ----- rendering -----------------------------------------------------------

fn pick_for_report(bottles: &[Bottle], env_override: Option<&str>) -> Option<Bottle> {
    // Mirror of bottle_discovery::pick_gw2_bottle, but takes a precomputed
    // list so doctor only enumerates once.
    if let Some(name) = env_override {
        return bottles.iter().find(|b| b.name == name).cloned();
    }
    let with_gw2 = || bottles.iter().filter(|b| b.has_gw2);
    with_gw2()
        .find(|b| b.runner == bottle_discovery::Runner::CrossOver && b.name == "Guild Wars 2")
        .or_else(|| with_gw2().find(|b| b.runner == bottle_discovery::Runner::CrossOver))
        .or_else(|| with_gw2().find(|b| b.runner == bottle_discovery::Runner::Whisky))
        .cloned()
}

pub fn render(r: &CheckResult, ansi: bool) -> String {
    let (glyph, color) = match r.kind {
        CheckKind::Ok => ("✓", ANSI_GREEN),
        CheckKind::Warn => ("⚠", ANSI_YELLOW),
        CheckKind::Fail => ("✗", ANSI_RED),
    };
    let mut out = if ansi {
        format!(
            "{color}{glyph}{ANSI_RESET} {ANSI_BOLD}{title}{ANSI_RESET} — {detail}",
            title = r.title,
            detail = r.detail,
        )
    } else {
        format!(
            "{glyph} {title} — {detail}",
            title = r.title,
            detail = r.detail
        )
    };
    if let Some(hint) = &r.hint {
        use std::fmt::Write as _;
        let indented = hint
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        if ansi {
            // write! to a String never fails (infallible), and avoids
            // the intermediate allocation clippy's format_push_string lint
            // flags.
            let _ = write!(out, "\n{ANSI_DIM}{indented}{ANSI_RESET}");
        } else {
            out.push('\n');
            out.push_str(&indented);
        }
    }
    out
}

fn verdict_line(results: &[CheckResult], ansi: bool) -> String {
    let any_fail = results.iter().any(|r| r.kind == CheckKind::Fail);
    let any_warn = results.iter().any(|r| r.kind == CheckKind::Warn);
    let (text, color) = if any_fail {
        (
            "Mumble Link is NOT working — see ✗ entries above.",
            ANSI_RED,
        )
    } else if any_warn {
        (
            "Mumble Link is partially working — see ⚠ entries above.",
            ANSI_YELLOW,
        )
    } else {
        ("Mumble Link is healthy ✓", ANSI_GREEN)
    };
    if ansi {
        format!("{color}{ANSI_BOLD}{text}{ANSI_RESET}")
    } else {
        text.to_owned()
    }
}

fn file_sha256(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bottle(name: &str, runner: bottle_discovery::Runner, has_gw2: bool) -> Bottle {
        Bottle {
            runner,
            name: name.to_owned(),
            root: PathBuf::from(format!("/tmp/fake/{name}")),
            has_gw2,
        }
    }

    #[test]
    fn check_crossover_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cxstart");
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        let r = check_crossover(&p);
        assert_eq!(r.kind, CheckKind::Ok);
    }

    #[test]
    fn check_crossover_missing() {
        let r = check_crossover(Path::new("/nonexistent/cxstart"));
        assert_eq!(r.kind, CheckKind::Warn);
        assert!(r.hint.as_deref().unwrap().contains("Whisky"));
    }

    #[test]
    fn check_whisky_absent_is_ok_informational() {
        let r = check_whisky(Path::new("/nonexistent/Whisky.app"));
        assert_eq!(r.kind, CheckKind::Ok);
    }

    #[test]
    fn check_bottles_enumerated_empty_is_fail() {
        let r = check_bottles_enumerated(&[]);
        assert_eq!(r.kind, CheckKind::Fail);
    }

    #[test]
    fn check_bottles_enumerated_summary_lists_all() {
        let bottles = vec![
            make_bottle("Guild Wars 2", bottle_discovery::Runner::CrossOver, true),
            make_bottle("Old", bottle_discovery::Runner::Whisky, false),
        ];
        let r = check_bottles_enumerated(&bottles);
        assert_eq!(r.kind, CheckKind::Ok);
        assert!(r.detail.contains("Guild Wars 2"));
        assert!(r.detail.contains("Old"));
        assert!(r.detail.contains("GW2 detected"));
        assert!(
            r.detail.contains(';'),
            "expected '; ' separator: {}",
            r.detail
        );
    }

    #[test]
    fn check_selected_bottle_with_override_no_match() {
        let bottles = vec![make_bottle(
            "Guild Wars 2",
            bottle_discovery::Runner::CrossOver,
            true,
        )];
        let r = check_selected_bottle(None, &bottles, Some("Nope"));
        assert_eq!(r.kind, CheckKind::Fail);
        assert!(r.hint.as_deref().unwrap().contains("auto-discovery"));
    }

    #[test]
    fn check_selected_bottle_no_gw2_anywhere_is_fail() {
        let bottles = vec![make_bottle(
            "Office",
            bottle_discovery::Runner::CrossOver,
            false,
        )];
        let r = check_selected_bottle(None, &bottles, None);
        assert_eq!(r.kind, CheckKind::Fail);
        assert!(r.hint.as_deref().unwrap().contains("GW2_BOTTLE"));
    }

    #[test]
    fn check_selected_bottle_with_pick_is_ok() {
        let b = make_bottle("Guild Wars 2", bottle_discovery::Runner::CrossOver, true);
        let r = check_selected_bottle(Some(&b), std::slice::from_ref(&b), None);
        assert_eq!(r.kind, CheckKind::Ok);
        assert!(r.detail.contains("Guild Wars 2"));
    }

    #[test]
    fn check_holder_source_missing_is_fail() {
        let r = check_holder_source(Path::new("/nonexistent/gw2-mcp-holder.exe"));
        assert_eq!(r.kind, CheckKind::Fail);
    }

    #[test]
    fn check_holder_source_present_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gw2-mcp-holder.exe");
        std::fs::write(&p, b"MZ").unwrap();
        let r = check_holder_source(&p);
        assert_eq!(r.kind, CheckKind::Ok);
    }

    #[test]
    fn check_holder_installed_matches() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.exe");
        std::fs::write(&source, b"binary contents").unwrap();
        let bottle_root = dir.path().join("bottle");
        let dest = bottle_root
            .join("drive_c")
            .join(holder_format::HOLDER_SUBDIR)
            .join(holder_format::HOLDER_EXE_NAME);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"binary contents").unwrap();
        let b = Bottle {
            runner: bottle_discovery::Runner::CrossOver,
            name: "test".to_owned(),
            root: bottle_root,
            has_gw2: true,
        };
        let r = check_holder_installed(&b, &source);
        assert_eq!(r.kind, CheckKind::Ok);
    }

    #[test]
    fn check_holder_installed_sha_mismatch_warns() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.exe");
        std::fs::write(&source, b"NEW version").unwrap();
        let bottle_root = dir.path().join("bottle");
        let dest = bottle_root
            .join("drive_c")
            .join(holder_format::HOLDER_SUBDIR)
            .join(holder_format::HOLDER_EXE_NAME);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"OLD version").unwrap();
        let b = Bottle {
            runner: bottle_discovery::Runner::CrossOver,
            name: "test".to_owned(),
            root: bottle_root,
            has_gw2: true,
        };
        let r = check_holder_installed(&b, &source);
        assert_eq!(r.kind, CheckKind::Warn);
        assert!(r.detail.contains("sha256"));
    }

    #[test]
    fn check_mirror_fresh_when_recently_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mumble.bin");
        std::fs::write(&p, b"x").unwrap();
        let r = check_mirror_fresh(&p);
        assert_eq!(r.kind, CheckKind::Ok);
    }

    #[test]
    fn check_mirror_fresh_when_missing_warns() {
        let r = check_mirror_fresh(Path::new("/nonexistent/mumble.bin"));
        assert_eq!(r.kind, CheckKind::Warn);
        assert!(r.hint.as_deref().unwrap().contains("Start gw2-mcp"));
    }

    #[test]
    fn check_payload_populated_with_ui_tick_zero_warns() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mumble.bin");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u32, |d| {
                u32::try_from(d.as_secs() & 0xFFFF_FFFF).unwrap_or(0)
            });
        let header = holder_format::header_bytes(123, now);
        let mut payload = vec![0u8; holder_format::HOLDER_PAYLOAD_LEN];
        // Set ui_version=1, ui_tick=0 explicitly.
        payload[0..4].copy_from_slice(&1u32.to_le_bytes());
        let mut bytes = Vec::with_capacity(header.len() + payload.len());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&payload);
        std::fs::write(&p, &bytes).unwrap();
        let r = check_payload_populated(&p);
        assert_eq!(r.kind, CheckKind::Warn);
        assert!(r.hint.as_deref().unwrap().contains("Launch"));
    }

    #[test]
    fn render_includes_glyph_and_title() {
        let r = CheckResult::warn("Title", "Detail", "Hint line");
        let plain = render(&r, false);
        assert!(plain.contains("⚠"));
        assert!(plain.contains("Title"));
        assert!(plain.contains("Hint line"));
    }

    #[test]
    fn render_indents_multiline_hint() {
        let r = CheckResult::fail("T", "D", "line one\nline two");
        let plain = render(&r, false);
        assert!(plain.contains("    line one"));
        assert!(plain.contains("    line two"));
    }

    #[test]
    fn verdict_picks_red_when_any_fail() {
        let results = vec![
            CheckResult::ok("a", "ok"),
            CheckResult::fail("b", "bad", "fix it"),
        ];
        let line = verdict_line(&results, false);
        assert!(line.contains("NOT working"));
    }

    #[test]
    fn verdict_picks_yellow_when_only_warns() {
        let results = vec![
            CheckResult::ok("a", "ok"),
            CheckResult::warn("b", "meh", "maybe fix"),
        ];
        let line = verdict_line(&results, false);
        assert!(line.contains("partially"));
    }

    #[test]
    fn verdict_green_when_all_ok() {
        let results = vec![CheckResult::ok("a", "ok"), CheckResult::ok("b", "ok")];
        let line = verdict_line(&results, false);
        assert!(line.contains("healthy"));
    }

    #[test]
    fn pick_for_report_matches_pick_gw2_bottle_logic() {
        // Same precedence rules as bottle_discovery::pick_gw2_bottle.
        let bottles = vec![
            make_bottle("Whisky", bottle_discovery::Runner::Whisky, true),
            make_bottle("Guild Wars 2", bottle_discovery::Runner::CrossOver, true),
        ];
        let pick = pick_for_report(&bottles, None).unwrap();
        assert_eq!(pick.name, "Guild Wars 2");
        assert_eq!(pick.runner, bottle_discovery::Runner::CrossOver);
    }
}
