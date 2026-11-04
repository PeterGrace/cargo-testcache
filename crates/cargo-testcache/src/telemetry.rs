//! Logging setup.
//!
//! Logs go to stderr, so stdout stays clean for output that is data (and can be piped).

use std::io::IsTerminal;

use tracing_subscriber::{EnvFilter, Layer, Registry, prelude::*};

use cargo_testcache_core::LogFormat;

/// The filter used when `RUST_LOG` is unset: this workspace's crates at `info`, everything
/// else at `warn`.
const DEFAULT_FILTER: &str = "warn,cargo_testcache=info,cargo_testcache_core=info";

/// Installs the global tracing subscriber. Call once, before anything logs.
///
/// The filter comes from `RUST_LOG` (for example `RUST_LOG=debug` or
/// `RUST_LOG=warn,cargo_testcache_core=trace`), or [`DEFAULT_FILTER`] when it is unset. Invalid
/// directives in it are skipped rather than failing startup.
///
/// # Arguments
///
/// * `format` - Human-readable text or JSON lines.
///
/// # Panics
///
/// If a global subscriber is already installed, which means `init` was called twice.
pub fn init(format: LogFormat) {
    let filter = EnvFilter::new(
        std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_else(|_| DEFAULT_FILTER.to_owned()),
    );
    let format_layer = tracing_subscriber::fmt::layer()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        // Colors only for a terminal; piped output (e.g. `kubectl logs`) stays plain text.
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    // The two formats are different types, so each is boxed (put behind a trait object, a
    // little like a duck-typed Python value) to give both arms of the `match` one type.
    let format_layer = match format {
        LogFormat::Text => format_layer.boxed(),
        LogFormat::Json => format_layer.json().boxed(),
    };

    let subscriber = Registry::default().with(format_layer.with_filter(filter));
    tracing::subscriber::set_global_default(subscriber)
        .expect("telemetry::init is called once, before anything else installs a subscriber");
}
