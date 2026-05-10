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
pub mod currency;
pub mod error;
pub mod reference;
pub mod wallet;
pub mod wiki;

pub use account::{Account, AccountAchievement, AccountMastery, Dailies, DailyEntry, DailyLevel};
pub use achievement::{Achievement, AchievementId};
pub use api_key::ApiKey;
pub use bearing::{Bearing16, METERS_PER_GW2_UNIT, bearing, distance_meters, distance_units};
pub use build_code::BuildChatCode;
pub use build_slug::BuildSlug;
pub use character::CharacterName;
pub use currency::{Currency, CurrencyId};
pub use error::DomainError;
pub use reference::{
    Item, ItemId, Skill, SkillId, Specialization, SpecializationId, Trait, TraitId,
};
pub use wallet::{WalletEntry, WalletInfo};
pub use wiki::{SearchLimit, SearchQuery, SearchResponse, SearchResult};
