//! Wizard's Vault objectives — the system that replaced GW2's old
//! `/v2/achievements/daily` endpoint when the Vault launched.
//!
//! The per-account `daily`/`weekly`/`special` endpoints all return the
//! same shape: a track with meta-progress + per-objective rows. Each
//! objective embeds its human-readable title, the track it belongs to
//! (`PvE`/`PvP`/`WvW`), and the Astral Acclaim reward — so no
//! catalog-side join is needed to answer "what are today's dailies?".

use serde::{Deserialize, Serialize};

/// One per-account Wizard's Vault track: daily, weekly, or special.
/// Mirrors the response shape of
/// `/v2/account/wizardsvault/{daily,weekly,special}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WizardsVaultTrack {
    #[serde(default)]
    pub meta_progress_current: u32,
    #[serde(default)]
    pub meta_progress_complete: u32,
    /// Item id for the track's meta-completion reward (the bonus chest).
    /// `None` for tracks where the API omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_reward_item_id: Option<u32>,
    #[serde(default)]
    pub meta_reward_astral: u32,
    #[serde(default)]
    pub meta_reward_claimed: bool,
    #[serde(default)]
    pub objectives: Vec<WizardsVaultObjective>,
}

/// A single Vault objective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WizardsVaultObjective {
    pub id: u32,
    #[serde(default)]
    pub title: String,
    /// `PvE`, `PvP`, or `WvW`.
    #[serde(default)]
    pub track: String,
    /// Astral Acclaim earned for completing this objective.
    #[serde(default)]
    pub acclaim: u32,
    #[serde(default)]
    pub progress_current: u32,
    #[serde(default)]
    pub progress_complete: u32,
    #[serde(default)]
    pub claimed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_track_round_trips() {
        let raw = r#"{
            "meta_progress_current": 2,
            "meta_progress_complete": 4,
            "meta_reward_item_id": 12345,
            "meta_reward_astral": 50,
            "meta_reward_claimed": false,
            "objectives": [
                {
                    "id": 1,
                    "title": "Complete an Event",
                    "track": "PvE",
                    "acclaim": 25,
                    "progress_current": 1,
                    "progress_complete": 1,
                    "claimed": true
                },
                {
                    "id": 2,
                    "title": "Defeat a World Boss",
                    "track": "PvE",
                    "acclaim": 25,
                    "progress_current": 0,
                    "progress_complete": 1,
                    "claimed": false
                }
            ]
        }"#;
        let t: WizardsVaultTrack = serde_json::from_str(raw).unwrap();
        assert_eq!(t.meta_progress_current, 2);
        assert_eq!(t.meta_reward_item_id, Some(12345));
        assert_eq!(t.objectives.len(), 2);
        assert_eq!(t.objectives[0].title, "Complete an Event");
        assert!(t.objectives[0].claimed);
        assert!(!t.objectives[1].claimed);
    }

    #[test]
    fn missing_meta_reward_item_round_trips() {
        let raw = r#"{
            "meta_progress_current": 0,
            "meta_progress_complete": 0,
            "meta_reward_astral": 0,
            "meta_reward_claimed": false,
            "objectives": []
        }"#;
        let t: WizardsVaultTrack = serde_json::from_str(raw).unwrap();
        assert_eq!(t.meta_reward_item_id, None);
        assert!(t.objectives.is_empty());
    }
}
