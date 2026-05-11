//! Subcommand handlers for `gw2-mcp`.
//!
//! Each subcommand is its own module with a `run` function. `main.rs`
//! parses the CLI and dispatches; no business logic lives in main.

pub mod doctor;
pub mod print_config;
