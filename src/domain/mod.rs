//! Pure domain types. No IO, no HTTP, no async runtime — only validation
//! and value semantics. These types are safe to share across thread
//! boundaries and can be constructed in tests without any setup.

pub mod account;
pub mod api_key;
pub mod build_code;
pub mod build_slug;
pub mod character;
pub mod currency;
pub mod error;
pub mod reference;
pub mod wallet;
pub mod wiki;

pub use account::{Account, AccountAchievement, AccountMastery, Dailies, DailyEntry, DailyLevel};
pub use api_key::ApiKey;
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
