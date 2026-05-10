//! Account-state types from the GW2 `/v2/account*` family of endpoints.
//!
//! All of these are short-TTL data: the player's mid-session state changes
//! whenever they spend a coin, complete an achievement, run a dungeon, etc.
//! We model fields generously enough to be useful but lean on `extra` for
//! anything `ArenaNet` might add in the future, mirroring `domain::reference`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Account-level snapshot from `/v2/account`.
///
/// Required scope: `account`. This is the single most useful endpoint for
/// "what does this player have?" — it carries expansion access (`access`),
/// guild membership, fractal level, daily/monthly AP, `WvW` rank, and the
/// world the account is associated with.
///
/// Loose-typed: anything `ArenaNet` adds (e.g. new `access` flags) drops into
/// `extra` so deserialisation never breaks across API expansions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// ISO-8601 account creation timestamp.
    pub created: DateTime<Utc>,
    /// In-game age, in seconds played.
    pub age: u64,
    /// Numeric world id. Resolve via `/v2/worlds/:id` for a name (out of
    /// scope here — typically static and inferable from a small lookup).
    pub world: u64,
    #[serde(default)]
    pub guilds: Vec<String>,
    #[serde(default)]
    pub guild_leader: Vec<String>,
    /// Expansion / saga access flags — strings like `"GuildWars2"`,
    /// `"HeartOfThorns"`, `"PathOfFire"`, `"EndOfDragons"`, `"SecretsOfTheObscure"`,
    /// `"JanthirWilds"`, plus future additions.
    #[serde(default)]
    pub access: Vec<String>,
    #[serde(default)]
    pub commander: bool,
    #[serde(default)]
    pub fractal_level: Option<u32>,
    #[serde(default)]
    pub daily_ap: Option<u32>,
    #[serde(default)]
    pub monthly_ap: Option<u32>,
    #[serde(default)]
    pub wvw_rank: Option<u32>,
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub build_storage_slots: Option<u32>,
    /// Anything `ArenaNet` adds we don't model explicitly — round-trips verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One row from `/v2/account/achievements` — per-account progress on a single
/// achievement.
///
/// Fields are mostly optional because the GW2 API only includes a key when
/// it has something to say (e.g. `current` is missing for one-shot
/// achievements — `done` is true and that's it). Required scope:
/// `progression`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountAchievement {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeated: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlocked: Option<bool>,
}

impl AccountAchievement {
    /// True when the achievement has zero progress (LLM filter target).
    #[must_use]
    pub fn is_not_started(&self) -> bool {
        // `done == false` AND no current progress recorded.
        if self.done {
            return false;
        }
        match self.current {
            Some(0) | None => true,
            Some(_) => false,
        }
    }

    /// True when the achievement is fully complete and not repeatable
    /// progress to track (LLM filter target).
    #[must_use]
    pub fn is_completed(&self) -> bool {
        if self.done {
            return true;
        }
        matches!((self.current, self.max), (Some(c), Some(m)) if c >= m && m > 0)
    }
}

/// One row from `/v2/account/masteries` — progress on a single mastery track.
/// `level` is the highest tier unlocked (0 == unlocked but no tiers spent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccountMastery {
    pub id: u32,
    pub level: u32,
}

/// Today's daily achievement IDs partitioned by category. Public endpoint —
/// no API key required. Returned by `/v2/achievements/daily` (and the
/// identically-shaped `/v2/achievements/daily/tomorrow`).
///
/// Each entry is a wrapper around the achievement id plus its level
/// requirements; LLMs typically only care about `id`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Dailies {
    #[serde(default)]
    pub pve: Vec<DailyEntry>,
    #[serde(default)]
    pub pvp: Vec<DailyEntry>,
    #[serde(default)]
    pub wvw: Vec<DailyEntry>,
    #[serde(default)]
    pub fractals: Vec<DailyEntry>,
    #[serde(default)]
    pub special: Vec<DailyEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DailyEntry {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<DailyLevel>,
    /// `required_access` carries expansion gating like `{"product": "EndOfDragons", "condition": "HasAccess"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_access: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DailyLevel {
    pub min: i32,
    pub max: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_deserialises_minimal_payload() {
        let raw = r#"{
            "id": "ABCD-1234",
            "name": "Player.1234",
            "age": 12345,
            "created": "2018-01-01T00:00:00Z",
            "world": 2202,
            "guilds": [],
            "access": ["GuildWars2", "HeartOfThorns"],
            "commander": true,
            "fractal_level": 100,
            "daily_ap": 14000,
            "monthly_ap": 200,
            "wvw_rank": 500
        }"#;
        let acc: Account = serde_json::from_str(raw).unwrap();
        assert_eq!(acc.name, "Player.1234");
        assert_eq!(acc.fractal_level, Some(100));
        assert_eq!(acc.access.len(), 2);
        assert!(acc.commander);
    }

    #[test]
    fn account_achievement_filters() {
        let not_started = AccountAchievement {
            id: 1,
            current: Some(0),
            max: Some(10),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        };
        assert!(not_started.is_not_started());
        assert!(!not_started.is_completed());

        let in_progress = AccountAchievement {
            id: 2,
            current: Some(5),
            max: Some(10),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        };
        assert!(!in_progress.is_not_started());
        assert!(!in_progress.is_completed());

        let completed = AccountAchievement {
            id: 3,
            current: Some(10),
            max: Some(10),
            done: false,
            bits: None,
            repeated: None,
            unlocked: None,
        };
        assert!(completed.is_completed());

        let done_flag = AccountAchievement {
            id: 4,
            current: None,
            max: None,
            done: true,
            bits: None,
            repeated: None,
            unlocked: None,
        };
        assert!(done_flag.is_completed());
        assert!(!done_flag.is_not_started());
    }

    #[test]
    fn dailies_deserialises_partial_payload() {
        let raw = r#"{
            "pve": [{"id": 100, "level": {"min": 1, "max": 80}}],
            "pvp": [],
            "wvw": [],
            "fractals": [{"id": 200, "level": {"min": 80, "max": 80}}],
            "special": []
        }"#;
        let d: Dailies = serde_json::from_str(raw).unwrap();
        assert_eq!(d.pve.len(), 1);
        assert_eq!(d.pve[0].id, 100);
        assert_eq!(d.fractals[0].id, 200);
    }
}
