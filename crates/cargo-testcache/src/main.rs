//! `cargo-testcache`: the binary. Task 13 of the plan replaces this placeholder.

mod telemetry;

use std::process::ExitCode;

use clap::Parser;

use cargo_testcache_core::{Config, LogFormat};

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Log format, `text` or `json` [env: CARGO_TESTCACHE_LOG_FORMAT].
    #[arg(long)]
    log_format: Option<LogFormat>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    telemetry::init(cli.log_format.unwrap_or_default());
    match Config::from_lookup(|name| std::env::var_os(name)) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}
