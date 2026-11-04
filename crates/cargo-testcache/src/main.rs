//! `cargo-testcache`: the binary.
//!
//! Kept thin on purpose: it splits the command line into its own options, `cargo test`'s
//! arguments and the test binaries' arguments, sets up logging, and hands the run to
//! [`cargo_testcache_core::app`]. When the user types `cargo testcache <args>`, Cargo runs
//! `cargo-testcache testcache <args>` with `CARGO` set to itself.

mod telemetry;

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use tracing::{error, info, warn};

use cargo_testcache_core::app::{self, Options, Outcome};
use cargo_testcache_core::args::{self, Arity, CargoArgs, SplitArgs};
use cargo_testcache_core::config::ENV_PREFIX;
use cargo_testcache_core::{Config, LogFormat};

/// The exit status of a run with a failing test, as `cargo test` uses.
const TEST_FAILURE_EXIT: u8 = 101;

/// Runs `cargo test`, skipping test binaries that already passed with the same inputs.
///
/// Every argument that is not one of the options below goes to `cargo test`; arguments after
/// `--` go to every test binary.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-testcache",
    bin_name = "cargo testcache",
    version,
    disable_help_subcommand = true,
    override_usage = "cargo testcache [OPTIONS] [CARGO TEST ARGS] [-- TEST BINARY ARGS]\n       \
                      cargo testcache status [OPTIONS] [CARGO TEST ARGS]\n       \
                      cargo testcache clear"
)]
struct Cli {
    /// The subcommand, if any; without one, tests run.
    #[command(subcommand)]
    mode: Option<Mode>,
    /// Run every test binary and record the passes (a full refresh).
    #[arg(long, global = true)]
    no_cache: bool,
    /// Print why each test binary runs.
    #[arg(long, global = true)]
    explain: bool,
    /// Run each test binary through `cargo test` instead of executing it directly.
    #[arg(long, global = true)]
    via_cargo: bool,
    /// Skip doc tests.
    #[arg(long, global = true)]
    no_doc: bool,
    /// Log format, `text` or `json` [env: CARGO_TESTCACHE_LOG_FORMAT].
    #[arg(long, global = true)]
    log_format: Option<LogFormat>,
}

/// The subcommands besides the default run.
#[derive(Debug, Clone, Copy, Subcommand)]
enum Mode {
    /// List what would run and what would be skipped, without running anything.
    #[command(override_usage = "cargo testcache status [OPTIONS] [CARGO TEST ARGS]")]
    Status,
    /// Forget every recorded pass.
    Clear,
}

// `main` returns an `ExitCode` instead of calling `exit`, so destructors run (the store's lock
// is released) and errors are logged through tracing first.
fn main() -> ExitCode {
    let split = split_command_line(std::env::args_os().skip(1));
    let misplaced = misplaced_subcommand(&split);
    let SplitArgs {
        own, cargo, test, ..
    } = split;
    // `--help`, `--version` and usage errors print and exit here.
    let cli = Cli::parse_from(std::iter::once(OsString::from("cargo-testcache")).chain(own));
    let (config, dotenv_warnings) = load_config();
    let config = config.map(|config| match cli.log_format {
        Some(log_format) => config.with_log_format(log_format),
        None => config,
    });
    telemetry::init(
        config
            .as_ref()
            .map_or(LogFormat::default(), Config::log_format),
    );
    for warning in dotenv_warnings {
        warn!("{warning}");
    }
    if let Some(name) = misplaced {
        warn!("`{name}` was taken as a test-name filter; put the subcommand first");
    }

    let outcome = config
        .context("loading configuration")
        .and_then(|_| dispatch(&cli, cargo, test));
    match outcome {
        Ok(Outcome::Passed) => ExitCode::SUCCESS,
        Ok(Outcome::TestsFailed) => ExitCode::from(TEST_FAILURE_EXIT),
        Ok(Outcome::Exited(code)) => {
            ExitCode::from(u8::try_from(code).unwrap_or(TEST_FAILURE_EXIT))
        }
        Err(run_error) => {
            // `{:#}` prints the whole context chain, e.g. "cargo testcache: <cause>" or
            // "clearing the recorded passes: <cause>".
            error!("{run_error:#}");
            ExitCode::FAILURE
        }
    }
}

/// The subcommand name that was taken as a positional test-name filter, if any.
fn misplaced_subcommand(split: &SplitArgs) -> Option<String> {
    let name = split.test.first()?.to_str()?;
    let is_subcommand = Cli::command()
        .get_subcommands()
        .any(|command| command.get_name() == name);
    (split.starts_with_filter && is_subcommand).then(|| name.to_owned())
}

/// Splits the command line, recognizing exactly the options and subcommands clap defines.
///
/// # Arguments
///
/// * `args` - The arguments after the program name.
///
/// # Returns
///
/// The tool's own arguments, `cargo test`'s, and the test binaries'.
fn split_command_line(args: impl IntoIterator<Item = OsString>) -> SplitArgs {
    let mut command = Cli::command();
    // `build` adds clap's own `--help`/`-h` and `--version`/`-V` (the `help` subcommand is
    // disabled, so `help` stays a test filter).
    command.build();
    let options: HashMap<String, Arity> = command
        .get_arguments()
        .flat_map(|argument| {
            let arity = if argument.get_action().takes_values() {
                Arity::Value
            } else {
                Arity::Switch
            };
            let long = argument.get_long().map(|long| format!("--{long}"));
            let short = argument.get_short().map(|short| format!("-{short}"));
            long.into_iter().chain(short).map(move |name| (name, arity))
        })
        .collect();
    let subcommands: Vec<&str> = command
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();
    args::split(args, &subcommands, |name| options.get(name).copied())
}

/// Reads the configuration from the environment and the `CARGO_TESTCACHE_*` entries of `.env`.
///
/// `.env` is read, not loaded: test binaries inherit this process's environment, and
/// `cargo test` would not have put a project's `.env` into it.
///
/// # Returns
///
/// The configuration, and warnings to log once logging is set up.
fn load_config() -> (cargo_testcache_core::Result<Config>, Vec<String>) {
    let mut warnings = Vec::new();
    let from_dotenv: HashMap<String, String> = match dotenvy::dotenv_iter() {
        Ok(entries) => entries
            .filter_map(|entry| {
                entry
                    .map_err(|error| warnings.push(format!("skipping a .env line: {error}")))
                    .ok()
            })
            .filter(|(name, _)| name.starts_with(ENV_PREFIX))
            .collect(),
        Err(error) => {
            if !error.not_found() {
                warnings.push(format!("could not read .env: {error}"));
            }
            HashMap::new()
        }
    };
    let config = Config::from_lookup(|name| {
        std::env::var_os(name).or_else(|| from_dotenv.get(name).map(OsString::from))
    });
    (config, warnings)
}

/// Runs what the command line asked for.
///
/// # Errors
///
/// Whatever the run or the clear fails with, with context.
fn dispatch(cli: &Cli, cargo_args: CargoArgs, test_args: Vec<OsString>) -> anyhow::Result<Outcome> {
    let status_only = match cli.mode {
        Some(Mode::Clear) => {
            let removed = app::clear(&cargo_args).context("clearing the recorded passes")?;
            info!(removed, "cleared the recorded passes");
            return Ok(Outcome::Passed);
        }
        Some(Mode::Status) => true,
        None => false,
    };
    let options = Options {
        status_only,
        no_cache: cli.no_cache,
        explain: cli.explain,
        via_cargo: cli.via_cargo,
        no_doc: cli.no_doc,
        cargo_args,
        test_args,
    };
    app::run(&options).context("cargo testcache")
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    // Property: every option clap defines is recognized wherever it sits before `--`, the
    // rest goes to cargo, and clap then parses what was recognized.
    #[hegel::test]
    fn own_options_are_recognized_among_cargo_arguments(tc: TestCase) {
        let flags = ["--no-cache", "--explain", "--via-cargo", "--no-doc"];
        let cargo = ["-p", "alpha", "--release"];
        let status = tc.draw(gs::booleans());
        let json = tc.draw(gs::booleans());
        let separate_value = tc.draw(gs::booleans());
        let mut own: Vec<Vec<&str>> = Vec::new();
        for flag in flags {
            if tc.draw(gs::booleans()) {
                own.push(vec![flag]);
            }
        }
        if json {
            own.push(if separate_value {
                vec!["--log-format", "json"]
            } else {
                vec!["--log-format=json"]
            });
        }
        // Insert each own option (with its value) at a drawn gap between cargo arguments,
        // never between `-p` and its value.
        let gaps = [0, 2, 3];
        let mut slots: Vec<Vec<&str>> = vec![Vec::new(); gaps.len()];
        for option in &own {
            slots[tc.draw(gs::integers::<usize>().max_value(gaps.len() - 1))].extend(option);
        }
        let mut argv: Vec<OsString> = vec!["testcache".into()];
        if status {
            argv.push("status".into());
        }
        let mut previous = 0;
        for (slot, gap) in slots.iter().zip(gaps) {
            argv.extend(cargo[previous..gap].iter().map(OsString::from));
            argv.extend(slot.iter().map(OsString::from));
            previous = gap;
        }
        argv.extend(cargo[previous..].iter().map(OsString::from));
        argv.extend(["--", "--explain"].map(OsString::from));

        let split = split_command_line(argv);
        let cli = Cli::try_parse_from(
            std::iter::once(OsString::from("cargo-testcache")).chain(split.own),
        )
        .expect("the recognized options parse");

        let has = |flag: &str| own.iter().any(|option| option[0] == flag);
        assert_eq!(split.cargo.as_slice(), cargo.map(OsString::from).as_slice());
        assert_eq!(split.test, [OsString::from("--explain")]);
        assert_eq!(cli.no_cache, has("--no-cache"));
        assert_eq!(cli.explain, has("--explain"));
        assert_eq!(cli.via_cargo, has("--via-cargo"));
        assert_eq!(cli.no_doc, has("--no-doc"));
        assert_eq!(cli.log_format.is_some(), json);
        assert_eq!(matches!(cli.mode, Some(Mode::Status)), status);
    }
}
