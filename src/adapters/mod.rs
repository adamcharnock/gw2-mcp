//! Concrete implementations of the traits in [`crate::ports`].
//!
//! Each adapter is the only place in the codebase that touches a specific
//! external concern (HTTP client, in-memory cache, system clock). Swapping
//! providers should be a one-file change.

pub mod bottle_discovery;
pub mod build_code_chatr;
pub mod catalog_discretize;
pub mod catalog_metabattle;
pub mod catalog_snowcrows;
pub mod error_body;
pub mod gw2_http;
pub mod gw2_map;
pub mod holder_format;
pub mod holder_supervisor;
pub mod mcp_stdio;
pub mod memory_cache;
pub mod mumble_link;
pub mod sqlite_index;
pub mod system_clock;
pub mod wiki_http;

pub use bottle_discovery::{Bottle, Runner, discover_bottles, pick_gw2_bottle};
pub use build_code_chatr::ChatrDecoder;
pub use catalog_discretize::DiscretizeCatalog;
pub use catalog_metabattle::MetaBattleCatalog;
pub use catalog_snowcrows::SnowCrowsCatalog;
pub use gw2_http::HttpGw2Api;
pub use gw2_map::HttpMapData;
pub use holder_supervisor::{HolderSupervisor, HolderSupervisorOpts};
pub use mcp_stdio::McpServer;
pub use memory_cache::MemoryCache;
pub use mumble_link::{MUMBLE_HEADER_LEN, StubMumbleLink, parse_header, probe_default};
pub use sqlite_index::{INDEX_FILE_NAME, SqliteSearchIndex};
pub use system_clock::SystemClock;
pub use wiki_http::HttpWiki;
