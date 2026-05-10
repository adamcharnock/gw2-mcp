//! Reference types from the GW2 API: skills, traits, specializations.
//!
//! These objects are huge and have many variants (skill facts in particular
//! have ~20 sub-shapes). We model only the few fields we actually use for
//! enrichment (`id`, `name`, plus a handful of cross-link fields) and keep
//! everything else under `#[serde(flatten)] extra` so payloads round-trip
//! verbatim to MCP clients without us having to track every API change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::error::DomainError;

macro_rules! id_newtype {
    ($name:ident, $err:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u32);

        impl $name {
            pub fn new(id: i64) -> Result<Self, DomainError> {
                if id <= 0 || id > i64::from(u32::MAX) {
                    return Err(DomainError::$err { got: id });
                }
                Ok(Self(u32::try_from(id).expect("bounds checked")))
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(SkillId, SkillIdInvalid);
id_newtype!(TraitId, TraitIdInvalid);
id_newtype!(SpecializationId, SpecializationIdInvalid);
id_newtype!(ItemId, ItemIdInvalid);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trait {
    pub id: TraitId,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Specialization {
    pub id: SpecializationId,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Equipment / inventory item from `/v2/items`. `extra` carries everything
/// the GW2 API returns that we don't model (rarity, level, type, details,
/// etc.) so payloads round-trip verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_newtypes_reject_zero_and_negative() {
        assert!(SkillId::new(0).is_err());
        assert!(TraitId::new(-1).is_err());
        assert!(SpecializationId::new(0).is_err());
    }

    #[test]
    fn id_newtypes_accept_positive() {
        assert_eq!(SkillId::new(9137).unwrap().get(), 9137);
    }

    #[test]
    fn skill_round_trips_with_unknown_fields() {
        let raw = r#"{
            "id": 9137,
            "name": "Wave of Wrath",
            "description": "Send out a wave",
            "icon": "https://x/icon.png",
            "facts": [{"type":"Damage","hit_count":1}]
        }"#;
        let skill: Skill = serde_json::from_str(raw).unwrap();
        assert_eq!(skill.id, SkillId::new(9137).unwrap());
        assert_eq!(skill.name, "Wave of Wrath");
        assert!(skill.extra.contains_key("description"));
        assert!(skill.extra.contains_key("facts"));

        // Round-trip: re-serialised JSON must keep all original fields.
        let back = serde_json::to_value(&skill).unwrap();
        assert_eq!(back["facts"][0]["type"], "Damage");
    }
}
