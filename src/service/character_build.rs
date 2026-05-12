//! `get_character_build` — fetches both build- and equipment-tab arrays
//! for a character, filters by `TabSelector`, strips equipment cosmetics,
//! and pre-resolves skill/trait/specialization ids to `{id, name}` shapes
//! so the LLM doesn't need follow-up `get_skills` / `get_traits` calls.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::{Service, ServiceError, WALLET_TTL};
use crate::domain::{
    ApiKey, CharacterName, Skill, SkillId, Specialization, SpecializationId, Trait, TraitId,
};

/// Which build/equipment tab(s) to project from a character snapshot.
///
/// Defaults to `Active` because that's the in-game-equipped build — what
/// "what is this character running?" almost always means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabSelector {
    /// The single tab marked active in-game.
    #[default]
    Active,
    /// All tabs (verbatim).
    All,
    /// A specific tab number (1-indexed, matching the GW2 API `tab` field).
    Index(u8),
}

/// Snapshot of a character's build + equipment tabs at a moment in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CharacterBuildSnapshot {
    pub character_name: String,
    #[schemars(schema_with = "crate::ports::opaque_value_array_schema")]
    pub build_tabs: Vec<serde_json::Value>,
    #[schemars(schema_with = "crate::ports::opaque_value_array_schema")]
    pub equipment_tabs: Vec<serde_json::Value>,
    pub fetched_at: DateTime<Utc>,
    /// Minutes since `fetched_at`. Recomputed at response time
    /// (snapshot is cached for `WALLET_TTL` so this can range
    /// 0..5 min on cache-hit). See the time-delta convention in
    /// `service::minutes_between`.
    #[serde(default)]
    pub fetched_minutes_ago: i64,
}

impl Service {
    /// Fetch every build tab and equipment tab for a character.
    ///
    /// Output is cached per (api-key-fingerprint, character) at `WALLET_TTL` —
    /// the data changes whenever the player saves a tab in-game, so the
    /// short TTL avoids stale build advice without spamming the API.
    pub async fn get_character_build(
        &self,
        key: &ApiKey,
        name: &CharacterName,
        tab: TabSelector,
    ) -> Result<CharacterBuildSnapshot, ServiceError> {
        let cache_key = format!("character_build:{}:{}", key.fingerprint(), name.as_str());

        let raw_snap: CharacterBuildSnapshot = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(snap) = serde_json::from_str::<CharacterBuildSnapshot>(&json)
        {
            debug!(character = %name, "character build cache hit");
            snap
        } else {
            // Fetch in parallel — independent endpoints, no point sequential.
            let (build_tabs, equipment_tabs) = tokio::try_join!(
                self.gw2.fetch_buildtabs(key, name),
                self.gw2.fetch_equipmenttabs(key, name),
            )?;
            let snap = CharacterBuildSnapshot {
                character_name: name.as_str().to_owned(),
                build_tabs,
                equipment_tabs,
                fetched_at: self.clock.now(),
                fetched_minutes_ago: 0,
            };
            if let Ok(json) = serde_json::to_string(&snap) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            snap
        };

        // Filter by tab selector.
        let build_tabs = filter_tabs(&raw_snap.build_tabs, tab);
        let equipment_tabs = filter_tabs(&raw_snap.equipment_tabs, tab);

        // Strip cosmetic fields from equipment tabs.
        let equipment_tabs: Vec<Value> = equipment_tabs
            .into_iter()
            .map(strip_equipment_cosmetics)
            .collect();

        // Pre-resolve names on the selected build tabs.
        let resolved_build_tabs = self.resolve_build_tab_names(build_tabs).await;

        Ok(CharacterBuildSnapshot {
            character_name: raw_snap.character_name,
            build_tabs: resolved_build_tabs,
            equipment_tabs,
            fetched_at: raw_snap.fetched_at,
            fetched_minutes_ago: super::minutes_between(raw_snap.fetched_at, self.clock.now()),
        })
    }

    /// Walk each build tab, collect skill / trait / specialization ids,
    /// resolve them in batch via the cached lookups, and inline `{id, name}`
    /// shapes into the response.
    async fn resolve_build_tab_names(&self, tabs: Vec<Value>) -> Vec<Value> {
        // Collect all ids across all tabs first so a single batched lookup
        // covers everything.
        let mut skill_ids: Vec<SkillId> = Vec::new();
        let mut trait_ids: Vec<TraitId> = Vec::new();
        let mut spec_ids: Vec<SpecializationId> = Vec::new();

        for tab in &tabs {
            collect_build_ids(tab, &mut skill_ids, &mut trait_ids, &mut spec_ids);
        }
        skill_ids.sort_unstable();
        skill_ids.dedup();
        trait_ids.sort_unstable();
        trait_ids.dedup();
        spec_ids.sort_unstable();
        spec_ids.dedup();

        // Parallel fan-out — independent lookups.
        let (skills, traits, specs) = tokio::join!(
            self.get_skills_or_empty(&skill_ids),
            self.get_traits_or_empty(&trait_ids),
            self.get_specializations_or_empty(&spec_ids),
        );

        tabs.into_iter()
            .map(|t| inline_names(t, &skills, &traits, &specs))
            .collect()
    }

    async fn get_skills_or_empty(&self, ids: &[SkillId]) -> BTreeMap<SkillId, Skill> {
        match self.get_skills(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "skill name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
    }

    async fn get_traits_or_empty(&self, ids: &[TraitId]) -> BTreeMap<TraitId, Trait> {
        match self.get_traits(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "trait name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
    }

    async fn get_specializations_or_empty(
        &self,
        ids: &[SpecializationId],
    ) -> BTreeMap<SpecializationId, Specialization> {
        match self.get_specializations(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "specialization name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
    }
}

fn filter_tabs(tabs: &[Value], sel: TabSelector) -> Vec<Value> {
    match sel {
        TabSelector::All => tabs.to_vec(),
        TabSelector::Active => tabs
            .iter()
            .find(|t| {
                t.get("is_active").and_then(Value::as_bool).unwrap_or(false)
                    || t.get("is_active_equipment_template")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            })
            .cloned()
            .into_iter()
            .collect(),
        TabSelector::Index(n) => tabs
            .iter()
            .find(|t| t.get("tab").and_then(Value::as_u64) == Some(u64::from(n)))
            .cloned()
            .into_iter()
            .collect(),
    }
}

/// Drop cosmetic fields from each equipment piece on this tab (`dyes`,
/// `bound_to`, `binding`, `location`). LLMs reasoning about builds don't need
/// them; they're pure visual / inventory-state metadata.
fn strip_equipment_cosmetics(mut tab: Value) -> Value {
    let Some(eq_arr) = tab.get_mut("equipment").and_then(Value::as_array_mut) else {
        return tab;
    };
    for piece in eq_arr.iter_mut() {
        if let Some(obj) = piece.as_object_mut() {
            for k in ["dyes", "bound_to", "binding", "location"] {
                obj.remove(k);
            }
        }
    }
    tab
}

fn collect_build_ids(
    tab: &Value,
    skills: &mut Vec<SkillId>,
    traits: &mut Vec<TraitId>,
    specs: &mut Vec<SpecializationId>,
) {
    let Some(build) = tab.get("build") else {
        return;
    };

    // skills.heal, skills.utilities[], skills.elite + aquatic_skills.*
    for skill_block in ["skills", "aquatic_skills"] {
        let Some(block) = build.get(skill_block) else {
            continue;
        };
        for k in ["heal", "elite"] {
            push_skill(block.get(k), skills);
        }
        if let Some(arr) = block.get("utilities").and_then(Value::as_array) {
            for v in arr {
                push_skill(Some(v), skills);
            }
        }
    }

    // specializations[].id, specializations[].traits[]
    if let Some(arr) = build.get("specializations").and_then(Value::as_array) {
        for s in arr {
            if let Some(id) = s
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok())
            {
                specs.push(id);
            }
            if let Some(t_arr) = s.get("traits").and_then(Value::as_array) {
                for t in t_arr {
                    if let Some(id) = t
                        .as_u64()
                        .and_then(|n| TraitId::new(i64::try_from(n).ok()?).ok())
                    {
                        traits.push(id);
                    }
                }
            }
        }
    }
}

fn push_skill(v: Option<&Value>, out: &mut Vec<SkillId>) {
    if let Some(id) = v
        .and_then(Value::as_u64)
        .and_then(|n| SkillId::new(i64::try_from(n).ok()?).ok())
    {
        out.push(id);
    }
}

fn inline_names(
    mut tab: Value,
    skills: &BTreeMap<SkillId, Skill>,
    traits: &BTreeMap<TraitId, Trait>,
    specs: &BTreeMap<SpecializationId, Specialization>,
) -> Value {
    let Some(build) = tab.get_mut("build").and_then(Value::as_object_mut) else {
        return tab;
    };

    for skill_block in ["skills", "aquatic_skills"] {
        if let Some(block) = build.get_mut(skill_block).and_then(Value::as_object_mut) {
            for k in ["heal", "elite"] {
                if let Some(v) = block.get_mut(k) {
                    *v = inline_skill(v, skills);
                }
            }
            if let Some(arr) = block.get_mut("utilities").and_then(Value::as_array_mut) {
                for v in arr.iter_mut() {
                    *v = inline_skill(v, skills);
                }
            }
        }
    }

    if let Some(arr) = build
        .get_mut("specializations")
        .and_then(Value::as_array_mut)
    {
        for s in arr.iter_mut() {
            let spec_id = s
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok());
            if let (Some(obj), Some(id)) = (s.as_object_mut(), spec_id)
                && let Some(spec) = specs.get(&id)
            {
                obj.insert("name".to_owned(), json!(spec.name));
            }
            if let Some(t_arr) = s.get_mut("traits").and_then(Value::as_array_mut) {
                for t in t_arr.iter_mut() {
                    *t = inline_trait(t, traits);
                }
            }
        }
    }

    tab
}

fn inline_skill(v: &Value, skills: &BTreeMap<SkillId, Skill>) -> Value {
    let Some(id) = v
        .as_u64()
        .and_then(|n| SkillId::new(i64::try_from(n).ok()?).ok())
    else {
        return v.clone();
    };
    let name = skills.get(&id).map(|s| s.name.clone()).unwrap_or_default();
    json!({ "id": id.get(), "name": name })
}

fn inline_trait(v: &Value, traits: &BTreeMap<TraitId, Trait>) -> Value {
    let Some(id) = v
        .as_u64()
        .and_then(|n| TraitId::new(i64::try_from(n).ok()?).ok())
    else {
        return v.clone();
    };
    let name = traits.get(&id).map(|t| t.name.clone()).unwrap_or_default();
    json!({ "id": id.get(), "name": name })
}
