//! The library's error type.
//!
//! One `thiserror` enum for the whole crate; the binary wraps it in `anyhow::Error` and adds
//! context with `.context()`.

use std::io;
use std::process::ExitStatus;

/// Everything that can go wrong in `cargo_testcache_core`.
// `#[derive(thiserror::Error)]` writes the `Display` and `std::error::Error` impls; each
// `#[error("...")]` is the message, much like a Python exception's `__str__`. It is not `Clone`
// or `PartialEq` because `io::Error` is neither.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// One or more configuration values were invalid. Every problem found is listed, so a
    /// misconfiguration can be fixed in one pass.
    #[error("invalid configuration: {}", problems.join("; "))]
    InvalidConfig {
        /// One human-readable message per invalid setting.
        problems: Vec<String>,
    },

    /// A log format name other than `text` or `json`.
    #[error("unknown log format {value:?}, expected \"text\" or \"json\"")]
    UnknownLogFormat {
        /// The value that was given.
        value: String,
    },

    /// A filesystem or process operation failed.
    #[error("{action}: {source}")]
    Io {
        /// What was being done, such as "reading target/testcache/passes.jsonl".
        action: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// A command (`cargo metadata`, `rustc --print`) exited unsuccessfully.
    #[error("`{command}` failed with {status}")]
    CommandFailed {
        /// The command, for the message.
        command: String,
        /// How it exited.
        status: ExitStatus,
    },

    /// A command printed something this tool could not understand.
    #[error("unexpected output from `{command}`: {output:?}")]
    UnexpectedOutput {
        /// The command, for the message.
        command: String,
        /// What it printed.
        output: String,
    },

    /// JSON that was expected to parse did not.
    #[error("could not parse {what}: {source}")]
    Json {
        /// What was being parsed.
        what: String,
        /// The parser's error.
        #[source]
        source: serde_json::Error,
    },

    /// A `[workspace.metadata.testcache]` or `[package.metadata.testcache]` table is invalid.
    #[error("invalid testcache configuration in {location}: {message}")]
    InvalidManifest {
        /// Which table: "[workspace.metadata]" or "package `name`".
        location: String,
        /// The deserializer's message.
        message: String,
    },

    /// A glob in the configuration does not compile, or can never match.
    #[error("invalid glob {pattern:?}: {reason}")]
    InvalidGlob {
        /// The pattern as written.
        pattern: String,
        /// The glob compiler's error, or why the glob can never match.
        reason: String,
    },

    /// A `cargo test` argument this tool cannot honour.
    #[error("{argument} is not supported: {reason}")]
    UnsupportedArgument {
        /// The argument.
        argument: String,
        /// Why.
        reason: &'static str,
    },

    /// Cargo built a test binary for a package that `cargo metadata` did not list.
    #[error("cargo built a test binary for unknown package {id}")]
    UnknownPackage {
        /// The package id from the build message.
        id: String,
    },

    /// A library directory contains the platform's path-list separator, so the dynamic library
    /// path cannot be built.
    #[error("cannot build the library path: {0}")]
    LibraryPath(#[from] std::env::JoinPathsError),
}

impl Error {
    /// Returns a closure that wraps an `io::Error` with what was being done, for
    /// `.map_err(Error::io("reading x"))`.
    ///
    /// # Arguments
    ///
    /// * `action` - What was being done when the error happened.
    ///
    /// # Returns
    ///
    /// A closure from `io::Error` to [`Error::Io`].
    pub fn io(action: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let action = action.into();
        move |source| Self::Io { action, source }
    }
}

/// `Result` with this crate's [`Error`], like `std::io::Result`.
pub type Result<T> = std::result::Result<T, Error>;
