//! `cargo_testcache_core`: the application logic of `cargo-testcache`.
//!
//! The `cargo-testcache` binary only splits the command line, sets up logging and maps the
//! outcome to an exit status; everything else lives here, so tests and other crates can call it
//! without spawning a process. [`app::run`] is the entry point.

pub mod app;
pub mod args;
pub mod cargo_config;
pub mod config;
pub mod discover;
pub mod error;
pub mod exec;
pub mod inputs;
pub mod key;
pub mod manifest;
pub mod plan;
pub mod report;
pub mod store;
pub mod workspace;

// Re-exports, so callers can write `cargo_testcache_core::Config` rather than the full path.
pub use config::{Config, LogFormat};
pub use error::{Error, Result};
