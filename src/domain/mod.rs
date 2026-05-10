//! Pure domain types. No IO, no HTTP, no async runtime — only validation
//! and value semantics. These types are safe to share across thread
//! boundaries and can be constructed in tests without any setup.

pub mod api_key;
pub mod currency;
pub mod error;
pub mod wallet;
pub mod wiki;

pub use api_key::ApiKey;
pub use currency::{Currency, CurrencyId};
pub use error::DomainError;
pub use wallet::{WalletEntry, WalletInfo};
pub use wiki::{SearchLimit, SearchQuery, SearchResponse, SearchResult};
