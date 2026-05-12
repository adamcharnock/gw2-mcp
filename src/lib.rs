//! Guild Wars 2 MCP server library.
//!
//! Hexagonal architecture:
//! - [`domain`] — pure types and validation, no IO.
//! - [`ports`] — trait definitions for repositories, external clients, clock.
//! - [`adapters`] — concrete implementations (HTTP, in-memory cache, system clock).
//! - [`service`] — orchestration that depends only on ports.
//!
//! `main.rs` is the only place that picks concrete adapters.

pub mod adapters;
pub mod cli;
pub mod dev_build;
pub mod domain;
pub mod indexing;
pub mod logging;
pub mod ports;
pub mod service;
