//! Discover `CrossOver` and `Whisky` bottles on macOS, and decide which
//! one holds the user's Guild Wars 2 install.
//!
//! Used by both:
//! - [`crate::adapters::holder_supervisor`] — pick where to spawn the
//!   in-bottle helper.
//! - [`crate::adapters::mumble_link`] — derive the candidate paths to
//!   probe for the helper's mirror file.
//!
//! On Linux/Windows [`discover_bottles`] returns an empty vector so
//! callers don't need cfg gymnastics. The pure helpers (`pick_from`,
//! `enumerate`, `bottle_has_gw2`) compile on every platform and are
//! exercised by unit tests against synthetic tempdir layouts.

use std::path::{Path, PathBuf};

/// Which Wine wrapper a bottle belongs to. Determines how we launch
/// Windows binaries inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// Codeweavers `CrossOver` — bottles live under
    /// `~/Library/Application Support/CrossOver/Bottles/`. Launch via
    /// `cxstart --bottle <name>`.
    CrossOver,
    /// `Whisky` (open-source) — bottles live under one of two roots
    /// depending on Whisky version (legacy Containers/ vs current
    /// Application Support/). Launch by invoking the bundled
    /// `wine64` binary with `WINEPREFIX=<bottle-root>`.
    Whisky,
}

impl Runner {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CrossOver => "CrossOver",
            Self::Whisky => "Whisky",
        }
    }
}

/// A single discovered bottle.
#[derive(Debug, Clone)]
pub struct Bottle {
    pub runner: Runner,
    /// User-visible bottle identifier. For `CrossOver` this is the human
    /// bottle name (the dir is named after it). For Whisky it's the
    /// directory basename — often a UUID, which is ugly but stable; we
    /// don't parse `Metadata.plist` to extract a friendlier name (would
    /// add a `plist` dep for purely cosmetic value).
    pub name: String,
    /// The bottle's root directory — contains `drive_c/`.
    pub root: PathBuf,
    /// `true` when `Gw2-64.exe` was found in a known Program Files
    /// location inside this bottle. Selection prefers these.
    pub has_gw2: bool,
}

/// Enumerate all `CrossOver` and Whisky bottles visible on the current
/// system. Returns an empty vector on non-macOS targets.
#[must_use]
pub fn discover_bottles() -> Vec<Bottle> {
    #[cfg(target_os = "macos")]
    {
        discover_bottles_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Pick the bottle most likely to host Guild Wars 2.
///
/// Resolution order:
/// 1. If `env_override.is_some()`: exact-name match across all runners.
///    Returns `None` if no bottle matches the requested name (caller
///    surfaces that as an error so the user notices their typo).
/// 2. Among bottles where `has_gw2`: prefer `CrossOver` named exactly
///    `"Guild Wars 2"` → any other `CrossOver` → any Whisky.
/// 3. `None` otherwise.
#[must_use]
pub fn pick_gw2_bottle(env_override: Option<&str>) -> Option<Bottle> {
    pick_from(&discover_bottles(), env_override).cloned()
}

/// Pure variant of [`pick_gw2_bottle`] testable on any platform.
fn pick_from<'a>(bottles: &'a [Bottle], env_override: Option<&str>) -> Option<&'a Bottle> {
    if let Some(name) = env_override {
        return bottles.iter().find(|b| b.name == name);
    }
    let with_gw2 = || bottles.iter().filter(|b| b.has_gw2);
    with_gw2()
        .find(|b| b.runner == Runner::CrossOver && b.name == "Guild Wars 2")
        .or_else(|| with_gw2().find(|b| b.runner == Runner::CrossOver))
        .or_else(|| with_gw2().find(|b| b.runner == Runner::Whisky))
}

#[cfg(target_os = "macos")]
fn discover_bottles_macos() -> Vec<Bottle> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let mut out = Vec::new();
    enumerate(
        &home.join("Library/Application Support/CrossOver/Bottles"),
        Runner::CrossOver,
        &mut out,
    );
    // Whisky 0.x kept bottles inside its sandbox container; 1.x moved
    // them out to Application Support/Whisky. We probe both so a user
    // upgrading mid-stream still gets detected.
    enumerate(
        &home.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles"),
        Runner::Whisky,
        &mut out,
    );
    enumerate(
        &home.join("Library/Application Support/Whisky/Bottles"),
        Runner::Whisky,
        &mut out,
    );
    out
}

// Used on macOS and in tests on every platform. On Windows/Linux non-test
// builds the function genuinely has no callers, so allow dead_code there
// rather than wrapping the whole module in cfg gates.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn enumerate(root: &Path, runner: Runner, out: &mut Vec<Bottle>) {
    if !root.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let bottle_root = e.path();
        if !bottle_root.is_dir() {
            continue;
        }
        let Some(name) = bottle_root
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
        else {
            continue;
        };
        let has_gw2 = bottle_has_gw2(&bottle_root);
        out.push(Bottle {
            runner,
            name,
            root: bottle_root,
            has_gw2,
        });
    }
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn bottle_has_gw2(bottle_root: &Path) -> bool {
    const CANDIDATES: &[&str] = &[
        "drive_c/Program Files/Guild Wars 2/Gw2-64.exe",
        "drive_c/Program Files (x86)/Guild Wars 2/Gw2-64.exe",
    ];
    CANDIDATES.iter().any(|c| bottle_root.join(c).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(runner: Runner, name: &str, has_gw2: bool) -> Bottle {
        Bottle {
            runner,
            name: name.to_owned(),
            root: PathBuf::from(format!("/tmp/fake/{name}")),
            has_gw2,
        }
    }

    #[test]
    fn pick_from_empty_returns_none() {
        assert!(pick_from(&[], None).is_none());
        assert!(pick_from(&[], Some("anything")).is_none());
    }

    #[test]
    fn env_override_exact_match_wins() {
        let bottles = vec![
            b(Runner::CrossOver, "Guild Wars 2", true),
            b(Runner::Whisky, "My Custom Name", true),
        ];
        let pick = pick_from(&bottles, Some("My Custom Name")).expect("found");
        assert_eq!(pick.name, "My Custom Name");
        assert_eq!(pick.runner, Runner::Whisky);
    }

    #[test]
    fn env_override_no_match_returns_none() {
        let bottles = vec![b(Runner::CrossOver, "Guild Wars 2", true)];
        assert!(pick_from(&bottles, Some("Nonexistent")).is_none());
    }

    #[test]
    fn prefers_crossover_named_guild_wars_2() {
        let bottles = vec![
            b(Runner::Whisky, "Guild Wars 2", true),
            b(Runner::CrossOver, "Other GW2", true),
            b(Runner::CrossOver, "Guild Wars 2", true),
        ];
        let pick = pick_from(&bottles, None).expect("found");
        assert_eq!(pick.runner, Runner::CrossOver);
        assert_eq!(pick.name, "Guild Wars 2");
    }

    #[test]
    fn falls_back_to_any_crossover_then_whisky() {
        // No exact-name CrossOver — any CrossOver wins.
        let bottles_a = vec![
            b(Runner::Whisky, "GW2-bottle", true),
            b(Runner::CrossOver, "Renamed GW2", true),
        ];
        let pick = pick_from(&bottles_a, None).expect("found");
        assert_eq!(pick.runner, Runner::CrossOver);

        // No CrossOver at all — Whisky wins.
        let bottles_b = vec![b(Runner::Whisky, "GW2-bottle", true)];
        let pick = pick_from(&bottles_b, None).expect("found");
        assert_eq!(pick.runner, Runner::Whisky);
    }

    #[test]
    fn ignores_bottles_without_gw2() {
        let bottles = vec![
            b(Runner::CrossOver, "Steam Games", false),
            b(Runner::CrossOver, "Office", false),
        ];
        assert!(pick_from(&bottles, None).is_none());
    }

    #[test]
    fn env_override_works_even_when_no_gw2_marker() {
        // User explicitly named their bottle; even if our `has_gw2`
        // heuristic missed (e.g. they installed GW2 to a non-standard
        // location), the env override is authoritative.
        let bottles = vec![b(Runner::CrossOver, "Custom Bottle", false)];
        let pick = pick_from(&bottles, Some("Custom Bottle")).expect("found");
        assert_eq!(pick.name, "Custom Bottle");
    }

    #[test]
    fn bottle_has_gw2_detects_program_files_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bottle = dir.path();
        let exe = bottle.join("drive_c/Program Files/Guild Wars 2/Gw2-64.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).expect("mkdir");
        std::fs::write(&exe, b"fake").expect("write");
        assert!(bottle_has_gw2(bottle));
    }

    #[test]
    fn bottle_has_gw2_detects_x86_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bottle = dir.path();
        let exe = bottle.join("drive_c/Program Files (x86)/Guild Wars 2/Gw2-64.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).expect("mkdir");
        std::fs::write(&exe, b"fake").expect("write");
        assert!(bottle_has_gw2(bottle));
    }

    #[test]
    fn bottle_has_gw2_returns_false_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bottle = dir.path();
        std::fs::create_dir_all(bottle.join("drive_c")).expect("mkdir");
        assert!(!bottle_has_gw2(bottle));
    }

    #[test]
    fn enumerate_finds_bottles_in_synthetic_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bottles_root = dir.path().join("Bottles");
        // Bottle 1: has GW2.
        let b1 = bottles_root.join("Guild Wars 2");
        std::fs::create_dir_all(b1.join("drive_c/Program Files/Guild Wars 2")).expect("mkdir b1");
        std::fs::write(
            b1.join("drive_c/Program Files/Guild Wars 2/Gw2-64.exe"),
            b"fake",
        )
        .expect("write b1 exe");
        // Bottle 2: no GW2.
        let b2 = bottles_root.join("Steam Games");
        std::fs::create_dir_all(b2.join("drive_c")).expect("mkdir b2");
        // Stray file at the Bottles root level — must be ignored.
        std::fs::write(bottles_root.join(".DS_Store"), b"").expect("write ds_store");

        let mut out = Vec::new();
        enumerate(&bottles_root, Runner::CrossOver, &mut out);
        assert_eq!(out.len(), 2, "got: {out:?}");
        let gw2 = out
            .iter()
            .find(|b| b.name == "Guild Wars 2")
            .expect("Guild Wars 2 bottle");
        assert!(gw2.has_gw2);
        let steam = out
            .iter()
            .find(|b| b.name == "Steam Games")
            .expect("Steam Games bottle");
        assert!(!steam.has_gw2);
    }

    #[test]
    fn enumerate_on_missing_root_does_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does/not/exist");
        let mut out = Vec::new();
        enumerate(&missing, Runner::CrossOver, &mut out);
        assert!(out.is_empty());
    }
}
