//! `cargo_testcache_core`: the application logic of `cargo-testcache`.
//!
//! The `cargo-testcache` binary only splits the command line, sets up logging and maps the
//! outcome to an exit status; everything else lives here, so tests and other crates can call it
//! without spawning a process. `app::run` (added by a later task) is the entry point.

pub mod config;
pub mod error;

// Re-exports, so callers can write `cargo_testcache_core::Config` rather than the full path.
pub use config::{Config, LogFormat};
pub use error::{Error, Result};
