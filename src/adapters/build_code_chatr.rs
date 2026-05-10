//! `BuildCodeDecoder` adapter backed by the `chatr` crate.
//!
//! `chatr::BuildTemplate` does not derive `Serialize`, so we marshal it
//! into a stable JSON shape ourselves. Keeping the JSON layout under our
//! control means callers (LLMs and tests) aren't sensitive to upstream
//! field renames.

use chatr::{BuildTemplate, ChatCode};
use serde_json::json;

use crate::domain::BuildChatCode;
use crate::ports::{BuildCodeDecoder, BuildCodeError};

#[derive(Debug, Default, Clone, Copy)]
pub struct ChatrDecoder;

impl BuildCodeDecoder for ChatrDecoder {
    fn decode(&self, code: &BuildChatCode) -> Result<serde_json::Value, BuildCodeError> {
        let parsed =
            ChatCode::build(code.as_str()).map_err(|e| BuildCodeError::Malformed(e.to_string()))?;
        let t = BuildTemplate::try_from_chatcode(&parsed)
            .map_err(|e| BuildCodeError::Malformed(e.to_string()))?;
        Ok(template_to_json(&t))
    }
}

/// Convert a `chatr::BuildTemplate` into the public JSON shape MCP clients
/// see. Field names are `snake_case` and stable across `chatr` upgrades.
fn template_to_json(t: &BuildTemplate) -> serde_json::Value {
    json!({
        "profession": t.profession,
        "specializations": [
            spec_obj(t.specialization1, t.trait_adept_1, t.trait_master_1, t.trait_grandmaster_1),
            spec_obj(t.specialization2, t.trait_adept_2, t.trait_master_2, t.trait_grandmaster_2),
            spec_obj(t.specialization3, t.trait_adept_3, t.trait_master_3, t.trait_grandmaster_3),
        ],
        "skills": {
            "healing": pair(&t.healing),
            "utility": [pair(&t.utility[0]), pair(&t.utility[1]), pair(&t.utility[2])],
            "elite": pair(&t.elite),
        },
        // For ranger (4) these are pet ids; for revenant (9) these are legend ids.
        // For other professions chatr forces them to zero.
        "pets_or_legends": {
            "terrestrial_active": t.terrestrial_pet1_active_legend,
            "terrestrial_inactive": t.terrestrial_pet2_inactive_legend,
            "aquatic_active": t.aquatic_pet1_active_legend,
            "aquatic_inactive": t.aquatic_pet2_inactive_legend,
        },
        "inactive_legend_utilities": {
            "terrestrial": t.inactive_legend_utilities.terrestrial.utilities,
            "aquatic": t.inactive_legend_utilities.aquatic.utilities,
        },
        // Optional SotO weapon-mastery trailer; absent on pre-SotO builds.
        "weapon_mastery": json!({
            "palette_ids": t.weapons.weapon_palette_ids,
            "variant_skill_ids": t.weapons.weapon_variant_skill_ids,
        }),
    })
}

fn spec_obj(id: u8, adept: u8, master: u8, grandmaster: u8) -> serde_json::Value {
    json!({
        "id": id,
        // Trait positions within the spec — 0 = none selected, 1..=3 = column.
        "traits": { "adept": adept, "master": master, "grandmaster": grandmaster },
    })
}

fn pair(p: &chatr::PalettePair) -> serde_json::Value {
    json!({ "terrestrial": p.terrestrial, "aquatic": p.aquatic })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real chat code from chatr's own doctest — Soulbeast Ranger.
    const RANGER_CODE: &str = "[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]";

    #[test]
    fn decode_known_ranger_code() {
        let code = BuildChatCode::new(RANGER_CODE).unwrap();
        let json = ChatrDecoder.decode(&code).unwrap();
        // profession 6 = Ranger (0-indexed in build templates is 0-based,
        // chatr reports as the on-the-wire byte).
        assert_eq!(json["profession"], 6);
        assert_eq!(json["skills"]["healing"]["terrestrial"], 116);
    }

    #[test]
    fn rejects_garbage() {
        let code = BuildChatCode::new("[&NOTBASE64====]").unwrap();
        let err = ChatrDecoder.decode(&code).unwrap_err();
        assert!(matches!(err, BuildCodeError::Malformed(_)));
    }
}
