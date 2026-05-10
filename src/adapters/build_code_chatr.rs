//! `BuildCodeDecoder` adapter backed by the `chatr` crate.
//!
//! `chatr::BuildTemplate` does not derive `Serialize`, so we marshal it
//! into a stable JSON shape ourselves. Keeping the JSON layout under our
//! control means callers (LLMs and tests) aren't sensitive to upstream
//! field renames.
//!
//! We also resolve palette IDs (the 16-bit ids in chat codes) to real GW2
//! API skill IDs using the bundled palette table — see
//! `assets/professions_palette.json`. The output includes both the raw
//! palette ID and the resolved `api_skill_id`, so the LLM can pass that
//! straight into `get_skills` for names + descriptions.

use std::collections::HashMap;
use std::sync::OnceLock;

use chatr::{BuildTemplate, ChatCode};
use serde::Deserialize;
use serde_json::json;

use crate::domain::BuildChatCode;
use crate::ports::{BuildCodeDecoder, BuildCodeError};

#[derive(Debug, Default, Clone, Copy)]
pub struct ChatrDecoder;

// ---------------------------------------------------------------------------
// Palette table — vendored from chatr's `professions.json` (MIT/Apache).
// ---------------------------------------------------------------------------

const PALETTE_JSON: &str = include_str!("../../assets/professions_palette.json");

#[derive(Deserialize)]
struct WirePalette {
    id: String,
    /// `[[palette_id, skill_id], ...]`
    skills_by_palette: Vec<[u32; 2]>,
}

/// Profession byte (1-based, as carried in a build chat code) → palette → `skill_id`.
///
/// Built lazily on first decode and shared across all decoders. The
/// `OnceLock` guarantees we parse the JSON at most once per process.
struct PaletteTable {
    /// `[profession_byte_index][palette_id] = skill_id`.
    by_profession: HashMap<u8, HashMap<u32, u32>>,
}

static PALETTE_TABLE: OnceLock<PaletteTable> = OnceLock::new();

fn palette_table() -> &'static PaletteTable {
    PALETTE_TABLE.get_or_init(|| {
        // Profession byte order matches GW2 wiki Build_template format:
        //   1=Guardian 2=Warrior 3=Engineer 4=Ranger 5=Thief
        //   6=Elementalist 7=Mesmer 8=Necromancer 9=Revenant
        // chatr's professions.json lists them in that order, so we trust the
        // file order rather than re-mapping by name.
        let raw: Vec<WirePalette> =
            serde_json::from_str(PALETTE_JSON).expect("vendored palette JSON must parse");
        let mut by_profession = HashMap::new();
        for (idx, prof) in raw.iter().enumerate() {
            // Profession byte is 1-based.
            let byte = u8::try_from(idx + 1).expect("≤ 9 professions");
            let map: HashMap<u32, u32> =
                prof.skills_by_palette.iter().map(|[p, s]| (*p, *s)).collect();
            tracing::trace!(profession = %prof.id, byte, palette_count = map.len(), "loaded palette");
            by_profession.insert(byte, map);
        }
        PaletteTable { by_profession }
    })
}

fn resolve_palette(profession: u8, palette: u16) -> Option<u32> {
    if palette == 0 {
        // Empty slot.
        return None;
    }
    palette_table()
        .by_profession
        .get(&profession)?
        .get(&u32::from(palette))
        .copied()
}

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
    let prof = t.profession;
    json!({
        "profession": prof,
        "specializations": [
            spec_obj(t.specialization1, t.trait_adept_1, t.trait_master_1, t.trait_grandmaster_1),
            spec_obj(t.specialization2, t.trait_adept_2, t.trait_master_2, t.trait_grandmaster_2),
            spec_obj(t.specialization3, t.trait_adept_3, t.trait_master_3, t.trait_grandmaster_3),
        ],
        "skills": {
            "healing": pair(prof, &t.healing),
            "utility": [
                pair(prof, &t.utility[0]),
                pair(prof, &t.utility[1]),
                pair(prof, &t.utility[2]),
            ],
            "elite": pair(prof, &t.elite),
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

/// A palette pair. Both raw palette IDs (as stored in the chat code) and
/// resolved API skill IDs (so the LLM can call `get_skills` directly) are
/// included. `api_skill_id` is `null` if the palette ID is unknown — most
/// commonly because the slot is empty (palette 0).
fn pair(profession: u8, p: &chatr::PalettePair) -> serde_json::Value {
    json!({
        "terrestrial": {
            "palette_id": p.terrestrial,
            "api_skill_id": resolve_palette(profession, p.terrestrial),
        },
        "aquatic": {
            "palette_id": p.aquatic,
            "api_skill_id": resolve_palette(profession, p.aquatic),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real chat code from chatr's own doctest. Profession byte = 6 in chat-
    /// code format (chatr's own assertion).
    const CHATR_EXAMPLE: &str = "[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]";

    /// Discretize "Power Dragonhunter" build (Guardian, profession byte = 1).
    /// Captured from `tests/fixtures/discretize_power_dragonhunter.md`.
    const POWER_DH: &str =
        "[&DQEQPyo6GzkmDyYPihJIAUgBLQH+ALkBtRI3AQAAAAAAAAAAAAAAAAAAAAACMgAjAAA=]";

    #[test]
    fn decode_known_example_returns_palette_and_skill_id() {
        let code = BuildChatCode::new(CHATR_EXAMPLE).unwrap();
        let json = ChatrDecoder.decode(&code).unwrap();
        assert_eq!(json["profession"], 6);
        // Each pair carries both the raw palette id and the resolved API skill id.
        assert_eq!(json["skills"]["healing"]["terrestrial"]["palette_id"], 116);
        // The skill id resolved from palette 116 for profession 6 must be
        // a positive integer (we don't pin the exact value to avoid
        // tightening the test against chatr palette refreshes).
        let resolved = json["skills"]["healing"]["terrestrial"]["api_skill_id"].as_u64();
        assert!(
            matches!(resolved, Some(n) if n > 0),
            "expected resolved api_skill_id, got {resolved:?}"
        );
    }

    #[test]
    fn decode_power_dragonhunter_resolves_to_known_guardian_skills() {
        let code = BuildChatCode::new(POWER_DH).unwrap();
        let json = ChatrDecoder.decode(&code).unwrap();
        assert_eq!(json["profession"], 1, "Guardian = profession byte 1");

        // The Discretize Power Dragonhunter build uses these specific
        // Guardian skills (verified against the discretize index.md):
        //   heal     = Litany of Wrath (21664)
        //   utility1 = Procession of Blades (30364)
        //   utility2 = Bane Signet  (9168)
        //   utility3 = Sword of Justice (9093)
        //   elite    = Dragon's Maw (30273)
        assert_eq!(
            json["skills"]["healing"]["terrestrial"]["api_skill_id"], 21664,
            "heal palette must resolve to Litany of Wrath"
        );
        assert_eq!(
            json["skills"]["utility"][0]["terrestrial"]["api_skill_id"],
            30364
        );
        assert_eq!(
            json["skills"]["utility"][1]["terrestrial"]["api_skill_id"],
            9168
        );
        assert_eq!(
            json["skills"]["utility"][2]["terrestrial"]["api_skill_id"],
            9093
        );
        assert_eq!(
            json["skills"]["elite"]["terrestrial"]["api_skill_id"],
            30273
        );
    }

    #[test]
    fn empty_palette_resolves_to_null() {
        // Profession 1 (Guardian), palette 0 = empty slot.
        assert_eq!(resolve_palette(1, 0), None);
    }

    #[test]
    fn unknown_palette_id_resolves_to_null() {
        // Vastly out-of-range palette id should not panic, just return None.
        assert_eq!(resolve_palette(1, 65535), None);
    }

    #[test]
    fn unknown_profession_byte_resolves_to_null() {
        // Profession 99 doesn't exist; should not panic.
        assert_eq!(resolve_palette(99, 1), None);
    }

    #[test]
    fn palette_table_loads_all_nine_professions() {
        let table = palette_table();
        for byte in 1u8..=9 {
            assert!(
                table.by_profession.contains_key(&byte),
                "profession byte {byte} missing from palette table"
            );
            let map = &table.by_profession[&byte];
            assert!(
                !map.is_empty(),
                "profession byte {byte} has empty palette map"
            );
        }
    }

    #[test]
    fn rejects_garbage() {
        let code = BuildChatCode::new("[&NOTBASE64====]").unwrap();
        let err = ChatrDecoder.decode(&code).unwrap_err();
        assert!(matches!(err, BuildCodeError::Malformed(_)));
    }
}
