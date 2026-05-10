//! Concrete implementations of the traits in [`crate::ports`].
//!
//! Each adapter is the only place in the codebase that touches a specific
//! external concern (HTTP client, in-memory cache, system clock). Swapping
//! providers should be a one-file change.

pub mod gw2_http;
pub mod mcp_stdio;
pub mod memory_cache;
pub mod system_clock;
pub mod wiki_http;

pub use gw2_http::HttpGw2Api;
pub use mcp_stdio::McpServer;
pub use memory_cache::MemoryCache;
pub use system_clock::SystemClock;
pub use wiki_http::HttpWiki;
