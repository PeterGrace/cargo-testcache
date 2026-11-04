//! `cargo_testcache_core`: the application logic of `cargo-testcache`.
//!
//! The `cargo-testcache` binary only parses arguments, sets up logging and handles signals;
//! everything else lives here, so unit tests, integration tests under `tests/`, and other
//! crates can call it without spawning a process.

pub mod app;
pub mod config;
pub mod error;

// Re-exports, so callers can write `cargo_testcache_core::Config` rather than the full path.
pub use config::{Config, LogFormat};
pub use error::{Error, Result};
