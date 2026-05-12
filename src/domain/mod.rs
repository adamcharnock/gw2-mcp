//! Pure domain types. No IO, no HTTP, no async runtime — only validation
//! and value semantics. These types are safe to share across thread
//! boundaries and can be constructed in tests without any setup.

pub mod account;
pub mod achievement;
pub mod api_key;
pub mod bearing;
pub mod build_code;
pub mod build_slug;
pub mod character;
pub mod continents;
pub mod currency;
pub mod currency_caps;
pub mod error;
pub mod instances;
pub mod inventory;
pub mod map_neighbors;
pub mod mastery;
pub mod reference;
pub mod reset;
pub mod snake_case;
pub mod wallet;
pub mod wiki;
pub mod wizards_vault;

pub use account::{Account, AccountAchievement, AccountMastery};
pub use achievement::{Achievement, AchievementId};
pub use api_key::ApiKey;
pub use bearing::{Bearing16, METERS_PER_GW2_UNIT, bearing, distance_meters, distance_units};
pub use build_code::BuildChatCode;
pub use build_slug::BuildSlug;
pub use character::CharacterName;
pub use continents::{Region, RegionMap};
pub use currency::{Currency, CurrencyId};
pub use error::DomainError;
pub use instances::{Dungeon, DungeonPath, Raid, RaidEvent, RaidWing};
pub use inventory::{
    CharacterBag, CharacterInventory, InventorySlot, MaterialCategory, MaterialSlot,
};
pub use map_neighbors::{
    ConnectionType, Expansion, MapNeighborEntry, MapNeighborLink, MapNeighbors, MapNeighborsError,
};
pub use mastery::{AccountMasteryPoints, Mastery, MasteryId, MasteryLevel, RegionMasteryPoints};
pub use reference::{
    Item, ItemId, Skill, SkillId, Specialization, SpecializationId, Trait, TraitId,
};
pub use reset::{next_daily_reset, next_raid_reset};
pub use snake_case::title_case;
pub use wallet::{WalletEntry, WalletInfo};
pub use wiki::{SearchLimit, SearchQuery, SearchResponse, SearchResult};
pub use wizards_vault::{WizardsVaultObjective, WizardsVaultTrack};
