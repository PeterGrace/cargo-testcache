//! `cargo-testcache`: the binary.
//!
//! Kept thin on purpose: it parses arguments, loads `.env` and the configuration, sets up
//! logging, and runs [`cargo_testcache_core::app::run`] until SIGTERM or SIGINT. Logic belongs
//! in `cargo_testcache_core`, where it can be tested without a process.

mod shutdown;
mod telemetry;

use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;
use tokio::sync::watch;
use tracing::{error, info, warn};

use cargo_testcache_core::{Config, LogFormat};

use crate::shutdown::ShutdownListener;

/// Command-line arguments. Each one overrides the matching environment variable.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Log format, `text` or `json` [env: CARGO_TESTCACHE_LOG_FORMAT].
    #[arg(long)]
    log_format: Option<LogFormat>,
}

// `main` returns an `ExitCode` instead of panicking or calling `exit`, so destructors run and
// the error is logged through tracing first.
fn main() -> ExitCode {
    // Parse arguments first: `--help`, `--version` and usage errors print and exit here.
    let cli = Cli::parse();
    // Load `.env` before reading the environment. A missing file is normal; any other failure
    // is reported once logging is up.
    let dotenv = dotenvy::dotenv();
    let config = Config::from_env().map(|config| match cli.log_format {
        Some(log_format) => config.with_log_format(log_format),
        None => config,
    });

    // Logging starts before the configuration is checked, so a bad configuration is logged
    // like any other error (in the default format).
    telemetry::init(
        config
            .as_ref()
            .map_or(LogFormat::default(), Config::log_format),
    );
    if let Err(dotenv_error) = &dotenv
        && !dotenv_error.not_found()
    {
        warn!("could not load .env: {dotenv_error}");
    }

    let outcome = config.context("loading configuration").and_then(run);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(run_error) => {
            // `{:#}` prints the whole context chain: "loading configuration: invalid ...".
            error!("{run_error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the application until a shutdown signal, then waits for it to stop.
///
/// `#[tokio::main]` on a function other than `main` builds the runtime when it is called,
/// like `asyncio.run(...)`, so `main` stays synchronous and logging is set up before it.
///
/// # Arguments
///
/// * `config` - The validated configuration.
///
/// # Errors
///
/// When the signal handlers cannot be installed, the application task panics, or it does not
/// stop within the configured shutdown timeout.
#[tokio::main]
async fn run(config: Config) -> anyhow::Result<()> {
    let listener = ShutdownListener::install().context("installing signal handlers")?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let shutdown_timeout = config.shutdown_timeout();
    // `async move` hands `config` and `stop_rx` to the task, which may outlive this frame.
    let mut app = tokio::spawn(async move { cargo_testcache_core::app::run(&config, stop_rx).await });

    // Whichever finishes first: a signal, or the application stopping on its own.
    tokio::select! {
        signal = listener.wait() => {
            info!("received {}, shutting down", signal.context("waiting for a signal")?);
        }
        // `&mut app` polls the task without giving it up, so it can still be awaited below
        // if the other branch wins.
        joined = &mut app => return joined.context("application task panicked"),
    }

    // Fails only if the task already finished and dropped its receiver, which is fine.
    let _ = stop_tx.send(true);
    tokio::time::timeout(shutdown_timeout, app)
        .await
        .with_context(|| format!("application did not stop within {shutdown_timeout:?}"))?
        .context("application task panicked")
}
