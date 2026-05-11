//! `decode_build_code` — turn a `[&Dw…]` chat build code into structured
//! JSON. The raw decoder gives us specialization ids and trait *positions*
//! (1..=3 per tier); we resolve those positions into concrete trait ids via
//! the cached specializations lookup and add a `profession_name` next to
//! the raw byte so the LLM doesn't need a follow-up lookup.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tracing::warn;

use super::{Service, ServiceError, profession_byte_to_name};
use crate::domain::{BuildChatCode, SpecializationId};

impl Service {
    /// Decode a `[&Dw…]` build chat code into structured JSON.
    ///
    /// On top of the raw decoder output, this:
    /// - resolves trait *positions* (1..=3 column index per tier) into the
    ///   concrete `trait_id` from the specialisation's `major_traits` array,
    ///   so the LLM can pass the id straight into `get_traits`;
    /// - adds a `profession_name` next to the raw `profession` byte.
    ///
    /// Specialisation lookups go through the cached `get_specializations`
    /// path so repeated decodes for the same profession are cheap.
    pub async fn decode_build_code(&self, code: &BuildChatCode) -> Result<Value, ServiceError> {
        let mut value = self.build_decoder.decode(code)?;

        // Profession name (1-based byte → name).
        if let Some(prof) = value.get("profession").and_then(Value::as_u64)
            && let Ok(byte) = u8::try_from(prof)
            && let Some(name) = profession_byte_to_name(byte)
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("profession_name".to_owned(), json!(name));
        }

        // Resolve trait column-positions to concrete trait_ids.
        // Collect the spec ids first so we batch the lookup.
        let spec_ids: Vec<SpecializationId> = value
            .get("specializations")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("id").and_then(Value::as_u64))
                    .filter_map(|id| SpecializationId::new(i64::try_from(id).ok()?).ok())
                    .collect()
            })
            .unwrap_or_default();

        let specs = if spec_ids.is_empty() {
            BTreeMap::new()
        } else {
            // Don't fail decode if upstream lookups fail — return the
            // structural-only output and let the LLM ask again.
            match self.get_specializations(&spec_ids).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = ?e, "decode_build_code: failed to resolve specializations; returning unresolved traits");
                    BTreeMap::new()
                }
            }
        };

        if let Some(arr) = value
            .get_mut("specializations")
            .and_then(Value::as_array_mut)
        {
            for spec_obj in arr.iter_mut() {
                let spec_id = spec_obj
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok());
                let major_traits: Vec<u32> = spec_id
                    .and_then(|id| specs.get(&id))
                    .and_then(|s| s.extra.get("major_traits"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0))
                            .collect()
                    })
                    .unwrap_or_default();

                if let Some(traits_obj) = spec_obj.get_mut("traits").and_then(Value::as_object_mut)
                {
                    let new_obj: Map<String, Value> =
                        [("adept", 0u8), ("master", 1u8), ("grandmaster", 2u8)]
                            .into_iter()
                            .map(|(slot, tier)| {
                                let raw = traits_obj.get(slot).and_then(Value::as_u64).unwrap_or(0);
                                let position = u8::try_from(raw).unwrap_or(0);
                                let trait_id = if (1..=3).contains(&position) {
                                    let idx = usize::from(tier) * 3 + usize::from(position) - 1;
                                    major_traits.get(idx).copied().filter(|n| *n > 0)
                                } else {
                                    None
                                };
                                (
                                    slot.to_owned(),
                                    json!({
                                        "position": position,
                                        "trait_id": trait_id,
                                    }),
                                )
                            })
                            .collect();
                    *traits_obj = new_obj;
                }
            }
        }

        Ok(value)
    }
}
