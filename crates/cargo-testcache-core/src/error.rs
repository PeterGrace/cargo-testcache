//! The library's error type.
//!
//! One `thiserror` enum for the whole crate; the binary wraps it in `anyhow::Error` and adds
//! context with `.context()`. Add a variant per failure a caller may want to tell apart, and
//! `#[from]` to convert an underlying error with `?`.

/// Everything that can go wrong in `cargo_testcache_core`.
// `#[derive(thiserror::Error)]` writes the `Display` and `std::error::Error` impls; each
// `#[error("...")]` is the message, much like a Python exception's `__str__`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// One or more configuration values were invalid. Every problem found is listed, so a
    /// misconfigured deployment can be fixed in one pass.
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
}

/// `Result` with this crate's [`Error`], like `std::io::Result`.
pub type Result<T> = std::result::Result<T, Error>;
