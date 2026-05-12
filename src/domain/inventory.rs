//! Inventory-shaped GW2 endpoints: account bank, character inventory,
//! account materials. These share the "items in slots" idiom even
//! though their precise response shapes differ; the per-slot wire
//! representations live here so the adapter stays a thin JSON →
//! typed-struct translator and the service does any projection.
//!
//! Fields that we never surface to the LLM (dyes, `upgrade_slot_indices`,
//! stats objects, infusions) are deliberately dropped during
//! deserialization — they'd more than double the response size for
//! zero LLM value. The adapter's wire shape is "what we keep" not
//! "what the API returned".

use serde::{Deserialize, Serialize};

/// One occupied slot in an account bank or character inventory bag.
/// `None` in the wire array means "empty slot" and is dropped by the
/// adapter before the slot reaches this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySlot {
    pub id: u32,
    pub count: u32,
    /// "Account" or "Character" when the item is soulbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Character name when `binding == "Character"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_to: Option<String>,
    /// Remaining charges on consumables that have them (salvage kits,
    /// gathering tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charges: Option<u32>,
}

/// Wire shape of `/v2/account/materials`. One per material-storage
/// row, including empty ones (count=0). The service filters out
/// zero-count rows in summary mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialSlot {
    pub id: u32,
    pub category: u32,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
}

/// One material category from `/v2/materials/<id>`. Static metadata;
/// changes only when a new expansion ships. The service caches the
/// full set under `STATIC_TTL`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialCategory {
    pub id: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub items: Vec<u32>,
}

/// Wire shape of `/v2/characters/<name>/inventory`. The character's
/// equipped bags, each with its own inventory of optional slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharacterInventory {
    #[serde(default)]
    pub bags: Vec<Option<CharacterBag>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharacterBag {
    /// Item id of the bag itself (e.g. "20 Slot Invisible Bag").
    pub id: u32,
    pub size: u32,
    #[serde(default)]
    pub inventory: Vec<Option<InventorySlot>>,
}
