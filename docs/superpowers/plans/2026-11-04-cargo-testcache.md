# cargo-testcache v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `cargo testcache`, a Cargo subcommand that runs `cargo test` but skips every test
binary whose executable, spawned binaries, declared input files, declared environment and test
arguments match a recorded pass, as specified in
`docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`.

**Architecture:** The binary crate (`crates/cargo-testcache`) only splits the command line, sets up
logging and maps the outcome to an exit status. The library crate (`crates/cargo-testcache-core`)
does the work in one synchronous pass: read `cargo metadata`, build with
`cargo test --no-run --message-format=json-render-diagnostics`, hash executables and inputs in
parallel with rayon, decide run or skip for each binary against a JSON-lines store under the target
directory, run the binaries one at a time (directly, with the environment `cargo test` would give
them, or through `cargo test` itself), record the passes, then run doc tests uncached.

**Tech Stack:** Rust 1.99 (MSRV 1.89), serde/serde_json, sha2, hex, globset, walkdir, rayon,
indicatif, clap, thiserror/anyhow, tracing; tests with hegeltest (`#[hegel::test]`) and tempfile.

---

## Facts established before planning

Checked on 2026-11-04 with cargo 1.99.1 against a scratch workspace; the code below relies on them.

- `cargo test` runs every test binary with the **package root as its working directory**.
- It adds `CARGO`, `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH` and `CARGO_PKG_{NAME, VERSION,
  VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH, VERSION_PRE, AUTHORS, DESCRIPTION, HOMEPAGE,
  REPOSITORY, LICENSE, LICENSE_FILE, README, RUST_VERSION}` (empty string when unset; authors joined
  with `:`). It does **not** set `CARGO_CRATE_NAME` at run time.
- `CARGO_BIN_EXE_<name>` is set only for integration tests (and benchmarks), never for unit tests.
- `LD_LIBRARY_PATH` is `<target-dir>/<profile>`, then `<build-dir>/<profile>/deps`, then
  `<sysroot>/lib/rustlib/<host>/lib`, then whatever the variable held before (rustup's proxy has
  already put `<sysroot>/lib` there).
- When `build.build-dir` is configured, test executables live under the **build directory**
  (`<build-dir>/<profile>/deps/...`) while `bin` executables, the ones `CARGO_BIN_EXE_*` point at,
  live under the target directory. `cargo metadata` reports both as `target_directory` and
  `build_directory`.
- `cargo metadata` reports `rust_version` as written (`"1.89"`), and that is what tests see. The
  `cargo_metadata` crate parses it into `1.89.0`, which would make the environment differ, so this
  plan parses metadata and build messages with its own small serde structs.
- In the `--no-run` messages, a test binary is a `compiler-artifact` with `profile.test == true`
  and an `executable`; a `bin` built for `CARGO_BIN_EXE_*` has `profile.test == false`.
- `cargo test --doc` fails with "no library targets found" on a package without a library, so doc
  tests run only when a library test binary was built.
- `sha2` 0.11 does not implement `std::io::Write`; files are hashed with a read loop.
- `std::fs::File::lock`/`try_lock` are stable since Rust 1.89, and std opens files with
  `O_CLOEXEC` on Unix, so the store needs no `fs4` and a spawned daemon never inherits its lock.

## Decisions taken in this plan

Each is a deviation from, or a resolution of, something the design left open. The changelog entry
of the final task records them.

1. **No `cargo_metadata` crate.** Own serde structs for `cargo metadata` and build messages (see
   above: `rust_version`). Fewer dependencies, and exactly the fields used.
2. **Direct execution is the default; `--via-cargo` is the fallback** (design section 10, question
   1). The conformance test runs in CI on Linux and macOS, which is the condition the design set.
3. **Test output is inherited, not piped.** Piping stdout to parse libtest's summary would turn off
   libtest's colours and break `harness = false` binaries. Records therefore hold no
   passed/ignored counts (design 4.5 said "when present").
4. **No tokio, no signal handlers.** The tool is a synchronous CLI. Ctrl-C reaches the running test
   binary through the terminal's process group, and only an exit status of 0 is recorded, so an
   interrupted run records nothing for the interrupted binary. The template's `app.rs`,
   `shutdown.rs`, `tests/startup.rs`, `tokio-console` feature and recipes go.
5. **`.env` is read, never loaded.** Only its `CARGO_TESTCACHE_*` entries are used. Loading the
   whole file into the process would leak a project's `.env` into its test binaries, which
   `cargo test` does not do.
6. **Package `inputs` globs are relative to the package root**; workspace globs to the workspace
   root. Both add up.
7. **Unreadable directories are skipped while matching inputs** (a test running as the same user
   cannot read them either); an unreadable *matched file* makes the key unavailable, so the binary
   runs.
8. **Environment values are hashed in the record, never stored**, so a secret in a declared
   variable never lands in `target/`.
9. **Module names:** `config` stays the template's environment settings; the
   `[*.metadata.testcache]` tables live in `manifest`.
10. **`--no-run` and `--doc` pass straight through** to `cargo test`; `--message-format` is refused,
    because the tool sets it itself.
11. **Binaries run in (package name, target kind, target name) order**, lib before bins before
    integration tests, as Cargo does.
12. **Plain records have public fields** (`TargetId`, `TestBinary`, `Build`, `KeyParts`,
    `PassRecord`, `RunOutcome`): they are values with no invariants. Types with behaviour
    (`Workspace`, `Package`, `Store`, `Toolchain`, `Tally`) keep private fields and accessors.

## File structure

```
Cargo.toml                                   workspace deps, rust-version 1.89
justfile, .github/workflows/ci.yml           drop tokio-console; add the macOS fixture job
.env.example, README.md, .claude/CLAUDE.md   drop tokio-console and the shutdown timeout; usage
crates/cargo-testcache-core/
  Cargo.toml
  src/lib.rs          module list and re-exports
  src/error.rs        Error enum (thiserror)
  src/config.rs       CARGO_TESTCACHE_LOG_FORMAT (template, shutdown timeout removed)
  src/key.rs          Digest, KeyParts, KeyParts::key, env_part
  src/args.rs         split(): own options / cargo args / test args; CargoArgs queries
  src/manifest.rs     TestcacheConfig ([*.metadata.testcache]), AlwaysRun
  src/workspace.rs    Workspace and Package from `cargo metadata`; CARGO_PKG_* values
  src/discover.rs     cargo test --no-run; TargetKind, TargetId, TestBinary, Build
  src/inputs.rs       InputPatterns (globs per owner), InputIndex (walk + hash)
  src/store.rs        Store: passes.jsonl, lock, compaction, lookup, record, clear
  src/plan.rs         decide(), changes(), RunReason, pass_record()
  src/exec.rs         Toolchain, the test environment, direct/via-cargo commands, run()
  src/report.rs       Tally (summary line), labels, Progress (indicatif)
  src/app.rs          run() and clear(): the whole flow
crates/cargo-testcache/
  Cargo.toml
  src/main.rs         clap Cli, dispatch, exit status
  src/telemetry.rs    (template, tokio-console block removed)
  tests/fixture.rs    fixture-workspace tests, design section 9, items 1 to 9
  tests/fixtures/ws/  the fixture workspace (probe, alpha, beta, gamma)
changelog/YYYY-MM-DD-HHMM-cargo-testcache-v1.md
```

Every task ends with `just fmt` and then `just gate` passing (the code below is written close to
rustfmt's style, but `just fmt` is the authority). Tasks 2 to 11 add modules that nothing calls
yet; every item in them is `pub` or used by a `pub` item, so no `dead_code` warning appears and no
`#[expect(dead_code)]` is needed.

Run a single unit test with `cargo test -p cargo-testcache-core <module>::tests`.

---

### Task 1: Strip the template's async scaffolding and add the dependencies

**Files:**
- Delete: `crates/cargo-testcache-core/src/app.rs`, `crates/cargo-testcache/src/shutdown.rs`,
  `crates/cargo-testcache/tests/startup.rs`
- Modify: `Cargo.toml`, `crates/cargo-testcache-core/Cargo.toml`,
  `crates/cargo-testcache/Cargo.toml`, `crates/cargo-testcache-core/src/lib.rs`,
  `crates/cargo-testcache-core/src/config.rs`, `crates/cargo-testcache-core/src/error.rs`,
  `crates/cargo-testcache/src/main.rs`, `crates/cargo-testcache/src/telemetry.rs`, `justfile`,
  `.github/workflows/ci.yml`, `.env.example`, `README.md`, `.claude/CLAUDE.md`

- [ ] **Step 1: Create the branch and delete the async files**

```bash
git switch -c feat/cargo-testcache-v1
git rm crates/cargo-testcache-core/src/app.rs crates/cargo-testcache/src/shutdown.rs \
    crates/cargo-testcache/tests/startup.rs
```

- [ ] **Step 2: Root `Cargo.toml`**

Set `rust-version = "1.89"` and replace its comment with:

```toml
# Minimum supported Rust version, checked by `just msrv`. `std::fs::File::lock` (the store's
# lock) needs 1.89.
rust-version = "1.89"
```

Replace the whole `[workspace.dependencies]` table with:

```toml
[workspace.dependencies]
# Internal crates, referenced by path so members can depend on each other.
cargo-testcache-core = { path = "crates/cargo-testcache-core" }

# Application-level errors with `.context()`, used by the binary.
anyhow = "1.0.104"
# Command-line parsing. `derive` gives `#[derive(Parser)]`; `env` lets an `#[arg]` fall back to
# an environment variable.
clap = { version = "4.6.7", features = ["derive", "env"] }
# Reads `.env` without loading it into the process (see `load_config` in main.rs).
dotenvy = "0.15.7"
# Glob matching for `inputs` and `always_run`.
globset = "0.4.21"
# Hex encoding of digests in the store; `serde` adds `hex::serde` for `#[serde(with = ...)]`.
hex = { version = "0.4.3", features = ["serde"] }
# Progress bar on stderr while hashing and running.
indicatif = "0.18.6"
# Data parallelism for hashing executables and input files.
rayon = "1.12.0"
# Wrapper types that keep secrets out of `Debug` output and logs. Not used yet; here so every
# crate that needs it shares one version.
secrecy = "0.10.3"
# The store's JSON lines and Cargo's JSON output.
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
# SHA-256 for every digest in a cache key.
sha2 = "0.11.1"
# Library error enums (`#[derive(Error)]`).
thiserror = "2.0.21"
tracing = "0.1.44"
# `json` adds the JSON log format; `env-filter` reads `RUST_LOG`.
tracing-subscriber = { version = "0.3.23", features = ["fmt", "env-filter", "json"] }
# Walking the workspace for input files.
walkdir = "2.5.0"

# Hegel property-based testing. Published as `hegeltest`, imported as `hegel`. `static-engine`
# links the engine into the test binary instead of downloading it at test time.
hegeltest = { version = "0.52.0", features = ["static-engine"] }
# Temporary directories for store and fixture tests.
tempfile = "3.27.0"
```

- [ ] **Step 3: Member manifests**

`crates/cargo-testcache-core/Cargo.toml`, replace `[dependencies]` and `[dev-dependencies]`:

```toml
[dependencies]
globset.workspace = true
hex.workspace = true
indicatif.workspace = true
rayon.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tracing.workspace = true
walkdir.workspace = true

[dev-dependencies]
hegeltest.workspace = true
tempfile.workspace = true

[lints]
workspace = true
```

`crates/cargo-testcache/Cargo.toml`: set `description = "Runs cargo test, skipping test binaries
that already passed with the same inputs."`, and replace everything from `[dependencies]` down with:

```toml
[dependencies]
cargo-testcache-core.workspace = true
anyhow.workspace = true
clap.workspace = true
dotenvy.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true

[dev-dependencies]
hegeltest.workspace = true
tempfile.workspace = true
walkdir.workspace = true

[lints]
workspace = true
```

- [ ] **Step 4: `config.rs` without the shutdown timeout**

Replace `crates/cargo-testcache-core/src/config.rs` with:

```rust
//! Runtime configuration, read from `CARGO_TESTCACHE_*` environment variables.
//!
//! Settings layer from lowest to highest precedence: built-in defaults, the `CARGO_TESTCACHE_*`
//! entries of a `.env` file, environment variables, then command-line overrides applied by the
//! binary with the `with_*` methods. `RUST_LOG` is not read here: the binary hands it straight
//! to the log filter.
//!
//! [`Config::from_lookup`] takes the environment as a function, so tests pass a map instead of
//! setting process environment variables, which is `unsafe` in edition 2024 because other
//! threads may be reading the environment at the same time.

use std::ffi::OsString;
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// The prefix of every variable this tool reads; `.env` entries without it are ignored.
pub const ENV_PREFIX: &str = "CARGO_TESTCACHE_";

/// Environment variable naming the log format, `text` or `json`.
pub const LOG_FORMAT_VAR: &str = "CARGO_TESTCACHE_LOG_FORMAT";

/// How log lines are written to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines, the default.
    #[default]
    Text,
    /// One JSON object per line, for log collectors.
    Json,
}

// `FromStr` is what `"json".parse::<LogFormat>()` calls, and what clap uses to parse
// `--log-format json`.
impl FromStr for LogFormat {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("text") {
            Ok(Self::Text)
        } else if value.eq_ignore_ascii_case("json") {
            Ok(Self::Json)
        } else {
            Err(Error::UnknownLogFormat {
                value: value.to_owned(),
            })
        }
    }
}

// `Display` is Rust's `__str__`; it is also how clap shows a default value.
impl fmt::Display for LogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text => "text",
            Self::Json => "json",
        })
    }
}

/// The application's settings.
///
/// Fields are private; read them through the accessors, and build a value with
/// [`Config::from_lookup`] or [`Config::default`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    log_format: LogFormat,
}

impl Config {
    /// Reads the configuration through `lookup`, which returns a variable's value, or `None`
    /// when it is unset.
    ///
    /// # Arguments
    ///
    /// * `lookup` - Called once per variable name, like `os.environ.get` in Python.
    ///
    /// # Returns
    ///
    /// The configuration, with defaults for every variable that is unset.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`], listing every invalid variable rather than only the first.
    ///
    /// # Examples
    ///
    /// ```
    /// use cargo_testcache_core::{Config, LogFormat};
    ///
    /// let config = Config::from_lookup(|name| match name {
    ///     "CARGO_TESTCACHE_LOG_FORMAT" => Some("json".into()),
    ///     _ => None,
    /// })
    /// .expect("valid configuration");
    /// assert_eq!(config.log_format(), LogFormat::Json);
    /// ```
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let mut problems = Vec::new();
        let log_format = parse_var(&lookup, LOG_FORMAT_VAR, &mut problems);
        if !problems.is_empty() {
            return Err(Error::InvalidConfig { problems });
        }
        Ok(Self {
            log_format: log_format.unwrap_or_default(),
        })
    }

    /// Replaces the log format, for a command-line override.
    ///
    /// # Arguments
    ///
    /// * `log_format` - The new format.
    ///
    /// # Returns
    ///
    /// The updated configuration.
    #[must_use]
    pub fn with_log_format(mut self, log_format: LogFormat) -> Self {
        self.log_format = log_format;
        self
    }

    /// How log lines are written.
    pub fn log_format(&self) -> LogFormat {
        self.log_format
    }
}

/// Reads and parses one variable through `lookup`.
///
/// # Arguments
///
/// * `lookup` - The environment, as in [`Config::from_lookup`].
/// * `name` - The variable to read.
/// * `problems` - Where a message is appended if the variable is set but invalid.
///
/// # Returns
///
/// The parsed value, or `None` if the variable is unset or invalid.
fn parse_var<T>(
    lookup: &impl Fn(&str) -> Option<OsString>,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let raw = lookup(name)?;
    // Environment values are OS strings, which need not be valid UTF-8.
    let Some(text) = raw.to_str() else {
        problems.push(format!("{name} is not valid UTF-8"));
        return None;
    };
    text.parse()
        .map_err(|error| problems.push(format!("{name}={text:?}: {error}")))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    /// Builds a `lookup` over `vars`, standing in for the process environment.
    fn lookup_in<'a>(vars: &'a HashMap<&str, String>) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| vars.get(name).map(OsString::from)
    }

    // Property: a format's name parses back to the same format, in any letter case.
    #[hegel::test]
    fn log_format_names_round_trip(tc: TestCase) {
        let format =
            tc.draw(gs::sampled_from(&[LogFormat::Text, LogFormat::Json]).print_as_debug());
        let uppercase = tc.draw(gs::booleans());
        let name = format.to_string();
        let name = if uppercase { name.to_uppercase() } else { name };

        let parsed: LogFormat = name.parse().expect("a format's own name parses");

        assert_eq!(parsed, format);
    }

    // Property: a set variable is used, and an unset one gets the default.
    #[hegel::test]
    fn valid_values_are_read_and_unset_ones_default(tc: TestCase) {
        let format = tc.draw(gs::optional(
            gs::sampled_from(&[LogFormat::Text, LogFormat::Json]).print_as_debug(),
        ));
        let mut vars = HashMap::new();
        if let Some(format) = format {
            vars.insert(LOG_FORMAT_VAR, format.to_string());
        }

        let config = Config::from_lookup(lookup_in(&vars)).expect("valid values are accepted");

        assert_eq!(config.log_format(), format.unwrap_or_default());
    }

    // Property: an invalid value is reported under its variable's name.
    #[hegel::test]
    fn an_invalid_value_is_reported(tc: TestCase) {
        let format = tc.draw(gs::text().filter(|text: &String| text.parse::<LogFormat>().is_err()));
        let vars = HashMap::from([(LOG_FORMAT_VAR, format)]);

        let error = Config::from_lookup(lookup_in(&vars)).expect_err("invalid values are refused");

        let Error::InvalidConfig { problems } = error else {
            panic!("expected InvalidConfig, got {error:?}");
        };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with(LOG_FORMAT_VAR), "{problems:?}");
    }

    // Property: a command-line override wins over whatever the environment set.
    #[hegel::test]
    fn with_log_format_overrides_the_environment(tc: TestCase) {
        let formats = [LogFormat::Text, LogFormat::Json];
        let from_env = tc.draw(gs::sampled_from(&formats).print_as_debug());
        let from_cli = tc.draw(gs::sampled_from(&formats).print_as_debug());
        let vars = HashMap::from([(LOG_FORMAT_VAR, from_env.to_string())]);

        let config = Config::from_lookup(lookup_in(&vars))
            .expect("valid values are accepted")
            .with_log_format(from_cli);

        assert_eq!(config.log_format(), from_cli);
    }
}
```

- [ ] **Step 5: `error.rs` with every variant the later tasks use**

Replace `crates/cargo-testcache-core/src/error.rs` with:

```rust
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

    /// A glob in the configuration does not compile.
    #[error("invalid glob {pattern:?}: {source}")]
    InvalidGlob {
        /// The pattern as written.
        pattern: String,
        /// The compiler's error.
        #[source]
        source: globset::Error,
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
```

- [ ] **Step 6: `lib.rs`**

Replace `crates/cargo-testcache-core/src/lib.rs` with (later tasks add one `pub mod` line each, in
this order: `key`, `args`, `manifest`, `workspace`, `discover`, `inputs`, `store`, `plan`, `exec`,
`report`, `app`):

```rust
//! `cargo_testcache_core`: the application logic of `cargo-testcache`.
//!
//! The `cargo-testcache` binary only splits the command line, sets up logging and maps the
//! outcome to an exit status; everything else lives here, so tests and other crates can call it
//! without spawning a process. [`app::run`] is the entry point.

pub mod config;
pub mod error;

// Re-exports, so callers can write `cargo_testcache_core::Config` rather than the full path.
pub use config::{Config, LogFormat};
pub use error::{Error, Result};
```

- [ ] **Step 7: Interim `main.rs` and `telemetry.rs`**

Task 13 replaces `main.rs`; until then it only parses `--log-format` and exits. Replace
`crates/cargo-testcache/src/main.rs` with:

```rust
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
```

In `crates/cargo-testcache/src/telemetry.rs`, delete these lines:

```rust
    let registry = Registry::default();
    // `#[cfg(...)]` compiles the next statement only with the `tokio-console` feature, so
    // ordinary builds never include console-subscriber or bind its port.
    #[cfg(feature = "tokio-console")]
    let registry = registry.with(console_subscriber::spawn());

    // The filter is attached to the format layer only, so tokio-console still sees every span.
    let subscriber = registry.with(format_layer.with_filter(filter));
```

and put in their place:

```rust
    let subscriber = Registry::default().with(format_layer.with_filter(filter));
```

- [ ] **Step 8: justfile, CI, `.env.example`, README, CLAUDE.md**

- `justfile`: delete the `clippy-tokio-console` and `run-tokio-console` recipes (each with its
  comment line).
- `.github/workflows/ci.yml`: delete the line `      - run: just clippy-tokio-console`.
- `.env.example`: delete the three lines about `CARGO_TESTCACHE_SHUTDOWN_TIMEOUT_SECS` (comment,
  blank line before it, and the variable).
- `README.md`: delete the `just run-tokio-console` table row.
- `.claude/CLAUDE.md`: in "Project Layout", delete the sentence "Use `just run-tokio-console` for
  tokio-console: it sets `--cfg tokio_unstable` for that build only."

- [ ] **Step 9: Verify**

Run: `cargo update -w && just gate`
Expected: PASS. `cargo test` shows the four `config::tests` properties and the doc test passing.
Then `just deny`: expected PASS (all new crates are MIT, Apache-2.0 or Unlicense, which
`deny.toml` allows).

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "chore: drop the template's async scaffolding and add v1 dependencies"
```

### Task 2: `key`: digests and the cache key (design 4.2)

**Files:**
- Create: `crates/cargo-testcache-core/src/key.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod key;`)

- [ ] **Step 1: Write the module with its tests**

Write the whole file now, but with the bodies of `KeyParts::key` and `env_part` as `todo!()`, so
the tests compile and fail.
`testing` is a `#[cfg(test)]` helper module that `plan`'s and `store`'s tests reuse.

Create `crates/cargo-testcache-core/src/key.rs`:

```rust
//! Cache keys: a digest of a test binary and of every runtime input it declares.
//!
//! [`KeyParts`] holds one digest per input named in section 4.2 of the design, and
//! [`KeyParts::key`] folds them into the single [`Digest`] the store compares. Keeping the parts,
//! not only the key, is what lets `--explain` name the input that changed.
//!
//! Every value is encoded with a length prefix before it is hashed, so the parts `("ab", "c")`
//! and `("a", "bc")` can never produce the same key.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
// `as _` imports the trait only for its methods (`Sha256::new`, `update`, `finalize`), without
// taking the name `Digest`, which this module uses for its own type.
use sha2::{Digest as _, Sha256};

/// Bumped whenever [`KeyParts::key`] changes how it encodes the parts, so a key computed one
/// way is never compared with one computed another way.
pub const KEY_FORMAT_VERSION: u32 = 1;

/// Environment variables that are part of every key, besides those the configuration names.
pub const ALWAYS_KEYED_ENV: [&str; 2] = ["RUST_TEST_THREADS", "RUST_MIN_STACK"];

/// Every variable whose name starts with this is part of every key (`RUST_TEST_TIME_UNIT` and
/// its siblings, which turn libtest's time limits on).
pub const ALWAYS_KEYED_ENV_PREFIX: &str = "RUST_TEST_TIME_";

/// How much of a file is read per hash update. Test binaries run to hundreds of megabytes.
const READ_CHUNK: usize = 256 * 1024;

/// A SHA-256 digest, serialized as 64 lowercase hex digits.
// `#[serde(transparent)]` serializes the struct as its one field; `hex::serde` turns the bytes
// into a hex string and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(#[serde(with = "hex::serde")] [u8; 32]);

impl Digest {
    /// Hashes a byte string.
    ///
    /// # Arguments
    ///
    /// * `bytes` - What to hash.
    ///
    /// # Returns
    ///
    /// The SHA-256 of `bytes`.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hashes an OS string, such as an environment variable's value, by its platform bytes.
    ///
    /// # Arguments
    ///
    /// * `value` - What to hash.
    ///
    /// # Returns
    ///
    /// The SHA-256 of the value's bytes.
    pub fn of_os_str(value: &OsStr) -> Self {
        Self::of_bytes(value.as_encoded_bytes())
    }

    /// Hashes a sequence of byte strings, each length-prefixed, after their count.
    ///
    /// # Arguments
    ///
    /// * `fields` - The byte strings, in order. `ExactSizeIterator` means the iterator knows its
    ///   length up front, like a Python sequence rather than a generator.
    ///
    /// # Returns
    ///
    /// A digest that differs whenever the sequence differs, including in where one field ends
    /// and the next begins.
    pub fn of_fields<'a>(fields: impl ExactSizeIterator<Item = &'a [u8]>) -> Self {
        let mut encoder = Encoder::default();
        encoder.fields(fields);
        encoder.finish()
    }

    /// Hashes a file's contents, reading it in chunks.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// The SHA-256 of the file's contents.
    ///
    /// # Errors
    ///
    /// Any error opening or reading the file.
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        // On the heap: rayon's worker threads have small stacks.
        let mut chunk = vec![0_u8; READ_CHUNK];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => hasher.update(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// The digest's raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

/// Everything a test binary's key is computed from, one field per part of design section 4.2.
///
/// The maps are `BTreeMap`s (sorted by key, like a `dict` that always iterates in sorted order),
/// so the order in which files or variables were found never changes the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyParts {
    /// This tool's version and [`KEY_FORMAT_VERSION`] (part 6).
    pub tool: String,
    /// The test executable (part 1).
    pub binary: Digest,
    /// Binaries the test may spawn: `bin <name>` for `CARGO_BIN_EXE_<name>`, `spawns <path>` for
    /// the configured `spawns` (part 2).
    pub spawned: BTreeMap<String, Digest>,
    /// Declared input files, by workspace-relative path (part 3).
    pub inputs: BTreeMap<String, Digest>,
    /// Declared environment variables: a digest of the value, or `None` when unset (part 4).
    /// Values are hashed so that a secret never reaches the store.
    pub env: BTreeMap<String, Option<Digest>>,
    /// The arguments after `--` (part 5).
    pub args: Digest,
}

impl KeyParts {
    /// The value of [`KeyParts::tool`] for this build of the tool.
    pub fn tool_id() -> String {
        // `env!` reads a variable at compile time; Cargo sets `CARGO_PKG_VERSION` while building.
        format!("{}+key{KEY_FORMAT_VERSION}", env!("CARGO_PKG_VERSION"))
    }

    /// The digest of the arguments after `--`, for [`KeyParts::args`].
    ///
    /// # Arguments
    ///
    /// * `args` - The test binary arguments, in order.
    ///
    /// # Returns
    ///
    /// Their digest; order matters.
    pub fn args_digest(args: &[OsString]) -> Digest {
        Digest::of_fields(args.iter().map(|arg| arg.as_encoded_bytes()))
    }

    /// Folds the parts into the key the store compares.
    ///
    /// # Returns
    ///
    /// A digest of every part, each length-prefixed, in a fixed order.
    pub fn key(&self) -> Digest {
        todo!()
    }
}

/// Builds [`KeyParts::env`] from the configured variable names and the environment.
///
/// # Arguments
///
/// * `names` - The variables the configuration declares. [`ALWAYS_KEYED_ENV`] and every
///   variable starting with [`ALWAYS_KEYED_ENV_PREFIX`] are added.
/// * `environment` - A snapshot of the process environment.
///
/// # Returns
///
/// One entry per variable: the digest of its value, or `None` when it is unset.
pub fn env_part<'a>(
    names: impl IntoIterator<Item = &'a str>,
    environment: &BTreeMap<OsString, OsString>,
) -> BTreeMap<String, Option<Digest>> {
    todo!()
}

/// Feeds length-prefixed values into SHA-256.
#[derive(Default)]
struct Encoder(Sha256);

impl Encoder {
    /// Adds a count (or a length) as 8 little-endian bytes.
    fn count(&mut self, count: usize) {
        self.0.update((count as u64).to_le_bytes());
    }

    /// Adds one byte string, after its length.
    fn field(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.0.update(bytes);
    }

    /// Adds a sequence of byte strings, after their count.
    fn fields<'a>(&mut self, fields: impl ExactSizeIterator<Item = &'a [u8]>) {
        self.count(fields.len());
        for field in fields {
            self.field(field);
        }
    }

    /// Adds a name-to-digest map, after its length.
    fn digest_map(&mut self, map: &BTreeMap<String, Digest>) {
        self.count(map.len());
        for (name, digest) in map {
            self.field(name.as_bytes());
            self.field(&digest.0);
        }
    }

    /// Finishes the hash.
    fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}

/// Generators shared with other modules' tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    /// Draws an arbitrary digest.
    pub(crate) fn draw_digest(tc: &TestCase) -> Digest {
        Digest::of_bytes(&tc.draw(gs::binary().max_size(16)))
    }

    /// Draws a map of short names to digests.
    pub(crate) fn draw_digest_map(tc: &TestCase) -> BTreeMap<String, Digest> {
        let names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(4));
        names.into_iter().map(|name| (name, draw_digest(tc))).collect()
    }

    /// Draws arbitrary key parts.
    pub(crate) fn draw_parts(tc: &TestCase) -> KeyParts {
        let env_names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(4));
        KeyParts {
            tool: tc.draw(gs::text().max_size(8)),
            binary: draw_digest(tc),
            spawned: draw_digest_map(tc),
            inputs: draw_digest_map(tc),
            env: env_names
                .into_iter()
                .map(|name| (name, tc.draw(gs::booleans()).then(|| draw_digest(tc))))
                .collect(),
            args: draw_digest(tc),
        }
    }

    /// Changes exactly one part of `parts`, chosen by `which` (0 to 5).
    pub(crate) fn mutate_one_part(parts: &mut KeyParts, which: u8, name: String) {
        let rehash = |digest: &mut Digest| *digest = Digest::of_bytes(&digest.0);
        match which {
            0 => parts.tool.push('+'),
            1 => rehash(&mut parts.binary),
            2 => rehash(parts.spawned.entry(name).or_insert(parts.binary)),
            3 => rehash(parts.inputs.entry(name).or_insert(parts.binary)),
            4 => {
                let flipped = match parts.env.get(&name) {
                    Some(Some(_)) => None,
                    Some(None) => Some(parts.binary),
                    None => None,
                };
                parts.env.insert(name, flipped);
            }
            _ => rehash(&mut parts.args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{draw_digest, draw_parts, mutate_one_part};
    use super::*;

    use std::io::Write;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    // Property (design 9): the key is a function of its parts, and survives the store's JSON.
    #[hegel::test]
    fn the_key_is_a_function_of_its_parts(tc: TestCase) {
        let parts = draw_parts(&tc);

        let reloaded: KeyParts = serde_json::from_str(
            &serde_json::to_string(&parts).expect("parts serialize"),
        )
        .expect("parts deserialize");

        assert_eq!(parts.key(), parts.clone().key());
        assert_eq!(reloaded, parts);
        assert_eq!(reloaded.key(), parts.key());
    }

    // Property (design 9): changing any one part changes the key.
    #[hegel::test]
    fn changing_any_one_part_changes_the_key(tc: TestCase) {
        let parts = draw_parts(&tc);
        let which = tc.draw(gs::integers::<u8>().max_value(5));
        let name = tc.draw(gs::text().max_size(8));
        let mut changed = parts.clone();

        mutate_one_part(&mut changed, which, name);

        assert_ne!(parts.key(), changed.key());
    }

    // Property (design 9): the order inputs were found in does not change the key.
    #[hegel::test]
    fn input_order_does_not_change_the_key(tc: TestCase) {
        let mut parts = draw_parts(&tc);
        let names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(6));
        let entries: Vec<(String, Digest)> =
            names.into_iter().map(|name| (name, draw_digest(&tc))).collect();
        let rotation = tc.draw(gs::integers::<usize>().max_value(entries.len()));
        let mut reordered = entries.clone();
        reordered.rotate_left(rotation);
        reordered.reverse();

        parts.inputs = entries.into_iter().collect();
        let mut other = parts.clone();
        other.inputs = reordered.into_iter().collect();

        assert_eq!(parts.key(), other.key());
    }

    // Property (design 9): length prefixes keep ("ab", "c") and ("a", "bc") apart.
    #[hegel::test]
    fn length_prefixes_separate_fields(tc: TestCase) {
        let bytes = tc.draw(gs::binary().min_size(1).max_size(32));
        let first = tc.draw(gs::integers::<usize>().max_value(bytes.len()));
        let offset = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        // Any split point other than `first`.
        let second = (first + 1 + offset) % (bytes.len() + 1);

        let split_at = |at: usize| Digest::of_fields([&bytes[..at], &bytes[at..]].into_iter());

        assert_ne!(split_at(first), split_at(second));
    }

    // Property: hashing a file gives the digest of its bytes, across chunk boundaries.
    #[hegel::test(test_cases = 30)]
    fn a_file_hashes_like_its_bytes(tc: TestCase) {
        let unit = tc.draw(gs::binary().min_size(1).max_size(64));
        let repeats = tc.draw(gs::integers::<usize>().max_value(READ_CHUNK / 16));
        let contents = unit.repeat(repeats);
        let mut file = tempfile::NamedTempFile::new().expect("create a temporary file");
        file.write_all(&contents).expect("write it");

        let digest = Digest::of_file(file.path()).expect("hash it");

        assert_eq!(digest, Digest::of_bytes(&contents));
    }

    // Property: env_part has one entry per declared or always-keyed variable, plus every
    // RUST_TEST_TIME_* variable, and its value is present exactly when the variable is set.
    #[hegel::test]
    fn env_part_covers_declared_and_always_keyed_variables(tc: TestCase) {
        let pool = ["A", "B", "RUST_TEST_THREADS", "RUST_TEST_TIME_UNIT", "RUST_MIN_STACK"];
        let declared = tc.draw(gs::vecs(gs::sampled_from(&pool).print_as_debug()).max_size(3));
        let set = tc.draw(gs::vecs(gs::sampled_from(&pool).print_as_debug()).max_size(5));
        let environment: BTreeMap<OsString, OsString> =
            set.iter().map(|name| (name.into(), "value".into())).collect();

        let part = env_part(declared.iter().copied(), &environment);

        for name in declared.iter().chain(&ALWAYS_KEYED_ENV) {
            let is_set = set.contains(name);
            assert_eq!(part.get(*name).map(Option::is_some), Some(is_set), "{name}");
        }
        if set.contains(&"RUST_TEST_TIME_UNIT") {
            assert!(part.contains_key("RUST_TEST_TIME_UNIT"));
        }
        let expected = declared.len() + ALWAYS_KEYED_ENV.len() + 1;
        assert!(part.len() <= expected, "{part:?}");
    }

    // Property: a digest is 64 hex digits in JSON and reads back unchanged.
    #[hegel::test]
    fn digests_round_trip_through_json(tc: TestCase) {
        let digest = draw_digest(&tc);

        let json = serde_json::to_string(&digest).expect("serialize");

        assert_eq!(json, format!("\"{digest}\""));
        assert_eq!(digest.to_string().len(), 64);
        assert_eq!(serde_json::from_str::<Digest>(&json).expect("deserialize"), digest);
    }
}
```

Add `pub mod key;` to `lib.rs` after `pub mod error;`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p cargo-testcache-core key::tests`
Expected: FAIL. The key and env tests panic with "not yet implemented"; the file and JSON tests
pass.

- [ ] **Step 3: Implement `KeyParts::key` and `env_part`**

Replace the body of `KeyParts::key`:

```rust
    pub fn key(&self) -> Digest {
        let mut encoder = Encoder::default();
        encoder.field(self.tool.as_bytes());
        encoder.field(&self.binary.0);
        encoder.digest_map(&self.spawned);
        encoder.digest_map(&self.inputs);
        encoder.count(self.env.len());
        for (name, value) in &self.env {
            encoder.field(name.as_bytes());
            // An unset variable is an empty field; a set one is 32 bytes, so they never collide.
            encoder.field(value.as_ref().map_or(&[][..], |digest| &digest.0[..]));
        }
        encoder.field(&self.args.0);
        encoder.finish()
    }
```

Replace the body of `env_part`:

```rust
pub fn env_part<'a>(
    names: impl IntoIterator<Item = &'a str>,
    environment: &BTreeMap<OsString, OsString>,
) -> BTreeMap<String, Option<Digest>> {
    let declared = names.into_iter().chain(ALWAYS_KEYED_ENV).map(|name| {
        let value = environment.get(OsStr::new(name)).map(|value| Digest::of_os_str(value));
        (name.to_owned(), value)
    });
    let prefixed = environment.iter().filter_map(|(name, value)| {
        let name = name.to_str()?;
        name.starts_with(ALWAYS_KEYED_ENV_PREFIX)
            .then(|| (name.to_owned(), Some(Digest::of_os_str(value))))
    });
    declared.chain(prefixed).collect()
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core key::tests`
Expected: PASS, 7 tests. If `hasher.finalize().into()` does not convert to `[u8; 32]`, use
`<[u8; 32]>::from(hasher.finalize())`.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/key.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(key): digests and the length-prefixed cache key"
```

### Task 3: `args`: splitting the command line (design 6)

**Files:**
- Create: `crates/cargo-testcache-core/src/args.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod args;`)

`cargo testcache` accepts its own options anywhere before `--`, Cargo's arguments around them, and
the test binaries' arguments after `--`. Its own options are recognized by name through a
`classify` callback, which the binary builds from clap's definition (task 13), so the list of
options lives in one place.

- [ ] **Step 1: Write the tests**

Create `crates/cargo-testcache-core/src/args.rs` with the public items' signatures and
`todo!()` bodies (from Step 3), followed by:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    /// Classifies the options this test pretends the tool has.
    fn classify(name: &str) -> Option<Arity> {
        match name {
            "--explain" | "--no-cache" => Some(Arity::Switch),
            "--log-format" => Some(Arity::Value),
            _ => None,
        }
    }

    /// A run of arguments that belong together, and the group they belong to.
    #[derive(Debug, Clone, Copy)]
    enum Piece {
        Own(&'static [&'static str]),
        Cargo(&'static [&'static str]),
    }

    const PIECES: [Piece; 7] = [
        Piece::Own(&["--explain"]),
        Piece::Own(&["--log-format", "json"]),
        Piece::Own(&["--log-format=text"]),
        Piece::Cargo(&["--release"]),
        Piece::Cargo(&["-p", "alpha"]),
        Piece::Cargo(&["--features=x"]),
        Piece::Cargo(&["--test", "it"]),
    ];

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    // Property: own options (with their values) go to `own`, everything else before `--` to
    // `cargo`, everything after the first `--` to `test`, each in its original order.
    #[hegel::test]
    fn split_sorts_every_argument_into_its_group(tc: TestCase) {
        let leading_subcommand_name = tc.draw(gs::booleans());
        let pieces = tc.draw(gs::vecs(gs::sampled_from(&PIECES).print_as_debug()).max_size(6));
        let test = tc.draw(gs::vecs(
            gs::sampled_from(&["--explain", "--", "filter", "-p"]).print_as_debug(),
        ).max_size(4));
        let separator = tc.draw(gs::booleans()) || !test.is_empty();

        let mut argv = Vec::new();
        let (mut own, mut cargo) = (Vec::new(), Vec::new());
        if leading_subcommand_name {
            argv.push(SUBCOMMAND_NAME);
        }
        for piece in &pieces {
            match piece {
                Piece::Own(args) => {
                    argv.extend_from_slice(args);
                    own.extend_from_slice(args);
                }
                Piece::Cargo(args) => {
                    argv.extend_from_slice(args);
                    cargo.extend_from_slice(args);
                }
            }
        }
        if separator {
            argv.push("--");
            argv.extend_from_slice(&test);
        }

        let split = split(os(&argv), &["status"], classify);

        assert_eq!(split.own, os(&own));
        assert_eq!(split.cargo.as_slice(), os(&cargo).as_slice());
        assert_eq!(split.test, os(&test));
    }

    // Property: a subcommand name counts only as the first argument.
    #[hegel::test]
    fn subcommands_are_recognized_only_first(tc: TestCase) {
        let first = tc.draw(gs::booleans());

        let argv = if first { vec!["status", "-p", "x"] } else { vec!["-p", "status"] };
        let split = split(os(&argv), &["status"], classify);

        assert_eq!(split.own.first().map(OsString::as_os_str) == Some(OsStr::new("status")), first);
    }

    // Property: `value` finds the last occurrence, in either `--flag value` or `--flag=value`.
    #[hegel::test]
    fn value_returns_the_last_occurrence(tc: TestCase) {
        let occurrences = tc.draw(gs::vecs(gs::tuples!(
            gs::booleans(),
            gs::text().alphabet("abc/").min_size(1).max_size(6)
        )).max_size(4));
        let mut args = vec![OsString::from("--release")];
        for (attached, value) in &occurrences {
            if *attached {
                args.push(format!("--target-dir={value}").into());
            } else {
                args.push("--target-dir".into());
                args.push(value.into());
            }
        }

        let cargo = CargoArgs::new(args);

        let expected = occurrences.last().map(|(_, value)| OsString::from(value));
        assert_eq!(cargo.value("--target-dir").map(OsStr::to_os_string), expected);
    }

    const SELECTION: [&[&str]; 7] = [
        &["-p", "x"],
        &["-px"],
        &["--package=x"],
        &["--workspace"],
        &["--test", "t"],
        &["--lib"],
        &["--exclude", "y"],
    ];
    const OTHER: [&[&str]; 4] = [&["--release"], &["--features", "f"], &["-j", "4"], &["--locked"]];

    // Property: `without_selection` drops exactly the package and target selection arguments,
    // and `selects_targets` sees a target selection exactly when there is one.
    #[hegel::test]
    fn without_selection_keeps_everything_else(tc: TestCase) {
        let pieces = tc.draw(gs::vecs(gs::tuples!(
            gs::booleans(),
            gs::integers::<usize>().max_value(6)
        )).max_size(6));
        let mut args = Vec::new();
        let mut kept = Vec::new();
        let mut targets = false;
        for (selection, index) in pieces {
            if selection {
                let piece = SELECTION[index];
                targets |= piece[0] == "--test" || piece[0] == "--lib";
                args.extend(os(piece));
            } else {
                let piece = OTHER[index % OTHER.len()];
                args.extend(os(piece));
                kept.extend(os(piece));
            }
        }
        let cargo = CargoArgs::new(args);

        assert_eq!(cargo.without_selection().as_slice(), kept.as_slice());
        assert_eq!(cargo.selects_targets(), targets);
    }
}
```

If `gs::tuples!` is not available in this hegeltest version, draw the two values with two
separate `tc.draw` calls inside a loop of a drawn length instead.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core args::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

Put this above the tests in `args.rs`:

```rust
//! Splitting the command line into this tool's own options, Cargo's arguments, and the test
//! binaries' arguments, and answering questions about Cargo's.

use std::ffi::{OsStr, OsString};

/// The first argument Cargo passes when it runs `cargo testcache` as `cargo-testcache testcache`.
pub const SUBCOMMAND_NAME: &str = "testcache";

/// Package selection options that take a value.
const PACKAGE_SELECTION_WITH_VALUE: [&str; 3] = ["--package", "-p", "--exclude"];
/// Package selection switches.
const PACKAGE_SELECTION: [&str; 2] = ["--workspace", "--all"];
/// Target selection options that take a value.
const TARGET_SELECTION_WITH_VALUE: [&str; 4] = ["--bin", "--example", "--test", "--bench"];
/// Target selection switches. Any target selection turns `cargo test`'s doc tests off.
const TARGET_SELECTION: [&str; 6] =
    ["--lib", "--bins", "--examples", "--tests", "--benches", "--all-targets"];
/// Arguments that make the cache pointless, so `cargo test` runs unchanged.
const PASSTHROUGH: [&str; 2] = ["--no-run", "--doc"];
/// Arguments this tool sets itself.
const UNSUPPORTED: [&str; 1] = ["--message-format"];

/// Whether one of the tool's own options takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// A flag such as `--explain`.
    Switch,
    /// An option such as `--log-format json` (or `--log-format=json`).
    Value,
}

/// The command line, split into its three groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitArgs {
    /// The tool's own subcommand and options, for clap.
    pub own: Vec<OsString>,
    /// Arguments for `cargo test`.
    pub cargo: CargoArgs,
    /// Arguments for every test binary (after `--`).
    pub test: Vec<OsString>,
}

/// Splits the command line.
///
/// # Arguments
///
/// * `args` - The arguments after the program name. A leading `testcache` (what Cargo passes
///   when run as `cargo testcache`) is dropped.
/// * `subcommands` - The tool's subcommand names; one counts only as the first argument.
/// * `classify` - Given an option name such as `--explain` or `-h`, returns its arity if it is
///   one of the tool's own options.
///
/// # Returns
///
/// The three groups, each in its original order.
pub fn split(
    args: impl IntoIterator<Item = OsString>,
    subcommands: &[&str],
    classify: impl Fn(&str) -> Option<Arity>,
) -> SplitArgs {
    // `peekable` lets the loop look at the next item without taking it.
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == SUBCOMMAND_NAME) {
        args.next();
    }
    let mut split = SplitArgs::default();
    if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| subcommands.contains(&arg))
    {
        split.own.extend(args.next());
    }
    while let Some(arg) = args.next() {
        if arg == "--" {
            // `by_ref` lends the iterator to `extend` instead of giving it away.
            split.test.extend(args.by_ref());
            break;
        }
        let text = arg.to_str();
        match text.and_then(option_name).and_then(&classify) {
            Some(arity) => {
                let needs_value = arity == Arity::Value && !text.is_some_and(|t| t.contains('='));
                split.own.push(arg);
                if needs_value {
                    split.own.extend(args.next());
                }
            }
            None => split.cargo.0.push(arg),
        }
    }
    split
}

/// The option name of an argument: `--name` from `--name=value`, or `None` if it is not an
/// option at all.
fn option_name(arg: &str) -> Option<&str> {
    if !arg.starts_with('-') || arg == "-" {
        return None;
    }
    Some(arg.split_once('=').map_or(arg, |(name, _)| name))
}

/// The value attached to `arg` for `flag`: `v` from `--flag=v`, or from `-pv` for a short flag.
fn attached_value<'a>(arg: &'a str, flag: &str) -> Option<&'a str> {
    let rest = arg.strip_prefix(flag)?;
    if let Some(value) = rest.strip_prefix('=') {
        return Some(value);
    }
    let is_short = flag.len() == 2;
    (is_short && !rest.is_empty()).then_some(rest)
}

/// Whether `arg` is `flag`, alone or with an attached value.
fn is_flag(arg: &OsStr, flag: &str) -> bool {
    arg.to_str()
        .is_some_and(|text| text == flag || attached_value(text, flag).is_some())
}

/// The arguments for `cargo test`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoArgs(Vec<OsString>);

impl CargoArgs {
    /// Wraps arguments.
    ///
    /// # Arguments
    ///
    /// * `args` - Arguments for `cargo test`, in order.
    ///
    /// # Returns
    ///
    /// The wrapper.
    pub fn new(args: Vec<OsString>) -> Self {
        Self(args)
    }

    /// The arguments, in order.
    pub fn as_slice(&self) -> &[OsString] {
        &self.0
    }

    /// Whether `flag` is given, alone or with a value.
    ///
    /// # Arguments
    ///
    /// * `flag` - Such as `--no-fail-fast`.
    ///
    /// # Returns
    ///
    /// `true` if any argument is that flag.
    pub fn has(&self, flag: &str) -> bool {
        self.0.iter().any(|arg| is_flag(arg, flag))
    }

    /// The value of the last occurrence of `flag`, as `--flag value` or `--flag=value`.
    ///
    /// # Arguments
    ///
    /// * `flag` - Such as `--target-dir`.
    ///
    /// # Returns
    ///
    /// The value, or `None` when the flag is absent or has no value.
    pub fn value(&self, flag: &str) -> Option<&OsStr> {
        let mut found = None;
        let mut iter = self.0.iter();
        while let Some(arg) = iter.next() {
            let Some(text) = arg.to_str() else { continue };
            if text == flag {
                found = iter.next().map(OsString::as_os_str);
            } else if let Some(value) = attached_value(text, flag) {
                found = Some(OsStr::new(value));
            }
        }
        found
    }

    /// The first argument that makes `cargo test` run unchanged (`--no-run`, `--doc`).
    pub fn passthrough_flag(&self) -> Option<&'static str> {
        PASSTHROUGH.into_iter().find(|flag| self.has(flag))
    }

    /// The first argument this tool refuses (`--message-format`).
    pub fn unsupported_flag(&self) -> Option<&'static str> {
        UNSUPPORTED.into_iter().find(|flag| self.has(flag))
    }

    /// Whether the arguments select targets, which turns doc tests off in `cargo test`.
    pub fn selects_targets(&self) -> bool {
        TARGET_SELECTION
            .into_iter()
            .chain(TARGET_SELECTION_WITH_VALUE)
            .any(|flag| self.has(flag))
    }

    /// The arguments without package or target selection, for running one target through
    /// `cargo test -p <package> --test <name>`.
    ///
    /// # Returns
    ///
    /// Every other argument, in order.
    #[must_use]
    pub fn without_selection(&self) -> Self {
        let with_value = || PACKAGE_SELECTION_WITH_VALUE.into_iter().chain(TARGET_SELECTION_WITH_VALUE);
        let switches = || PACKAGE_SELECTION.into_iter().chain(TARGET_SELECTION);
        let mut kept = Vec::with_capacity(self.0.len());
        let mut iter = self.0.iter();
        while let Some(arg) = iter.next() {
            if let Some(text) = arg.to_str() {
                if with_value().any(|flag| text == flag) {
                    iter.next();
                    continue;
                }
                if with_value().any(|flag| attached_value(text, flag).is_some())
                    || switches().any(|flag| text == flag)
                {
                    continue;
                }
            }
            kept.push(arg.clone());
        }
        Self(kept)
    }
}
```

Add `pub mod args;` to `lib.rs` after `pub mod error;` (modules in alphabetical order:
`args`, `config`, `error`, `key`).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core args::tests`
Expected: PASS, 5 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/args.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(args): split own options, cargo args and test args"
```

### Task 4: `manifest`: the `[*.metadata.testcache]` tables (design 5)

**Files:**
- Create: `crates/cargo-testcache-core/src/manifest.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod manifest;`)

- [ ] **Step 1: Write the tests**

Create `manifest.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};
    use serde_json::json;

    fn draw_list(tc: &TestCase) -> Vec<String> {
        tc.draw(gs::vecs(gs::text().max_size(10)).max_size(3))
    }

    fn draw_name(tc: &TestCase) -> String {
        tc.draw(gs::text().alphabet("abc_-").min_size(1).max_size(8))
    }

    // Property: every key of the table is read as written.
    #[hegel::test]
    fn tables_are_read_as_written(tc: TestCase) {
        let config = TestcacheConfig {
            inputs: draw_list(&tc),
            env: draw_list(&tc),
            always_run: draw_list(&tc),
            spawns: draw_list(&tc),
        };
        let metadata = json!({ "other": 1, "testcache": {
            "inputs": config.inputs, "env": config.env,
            "always_run": config.always_run, "spawns": config.spawns,
        }});

        let read = TestcacheConfig::from_metadata(Some(&metadata), "[workspace.metadata]")
            .expect("a valid table");

        assert_eq!(read, config);
    }

    // Property: no table (or no metadata at all) means the empty configuration.
    #[hegel::test]
    fn a_missing_table_is_the_default(tc: TestCase) {
        let other_key = tc.draw(gs::text().filter(|key: &String| *key != TABLE));
        // A map built by hand: `json!` would take `other_key` as the literal key "other_key".
        let metadata = tc.draw(gs::booleans()).then(|| {
            Value::Object(serde_json::Map::from_iter([(other_key, json!({ "inputs": ["x"] }))]))
        });

        let read = TestcacheConfig::from_metadata(metadata.as_ref(), "package `p`")
            .expect("no table is valid");

        assert_eq!(read, TestcacheConfig::default());
    }

    // Property: a misspelt key is an error that names the table it is in.
    #[hegel::test]
    fn unknown_keys_are_refused_with_their_location(tc: TestCase) {
        let location = draw_name(&tc);
        let metadata = json!({ "testcache": { "inptus": ["x"] } });

        let error = TestcacheConfig::from_metadata(Some(&metadata), &location)
            .expect_err("unknown keys are refused");

        assert!(
            matches!(&error, Error::InvalidManifest { location: at, .. } if *at == location),
            "{error:?}"
        );
    }

    // Property: a literal pattern matches its own (package, target) and nothing else, and `*`
    // stays within one side of the `/`.
    #[hegel::test]
    fn always_run_matches_package_and_target(tc: TestCase) {
        let package = draw_name(&tc);
        let target = draw_name(&tc);
        let literal = format!("{}/{}", globset::escape(&package), globset::escape(&target));
        let matcher = AlwaysRun::new([literal.as_str(), "*/live_*"]).expect("valid globs");

        assert!(matcher.matches(&package, &target));
        assert!(!matcher.matches(&package, &format!("{target}x")) || target.starts_with("live_"));
        assert!(matcher.matches(&package, &format!("live_{target}")));
        assert!(!matcher.matches(&format!("{package}/live_x"), "y"));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core manifest::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! The `[workspace.metadata.testcache]` and `[package.metadata.testcache]` tables.
//!
//! ```toml
//! [workspace.metadata.testcache]
//! inputs = ["migrations/**"]     # files tests read at runtime
//! env = ["DATABASE_IMAGE"]       # variables whose values are part of every key
//! always_run = ["*/live_*"]      # "package/target" patterns that are never skipped
//! spawns = ["myapp-server"]      # binaries spawned by path, relative to target/<profile>
//! ```
//!
//! A package's table uses the same keys and adds to the workspace's. Its `inputs` are relative
//! to the package's directory; the workspace's to the workspace root.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// The key under `metadata` that holds this tool's table.
const TABLE: &str = "testcache";

/// One `[*.metadata.testcache]` table.
// `deny_unknown_fields` turns a misspelt key into an error instead of a silently ignored one;
// `default` makes every key optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestcacheConfig {
    /// Globs of files that tests read at runtime.
    pub inputs: Vec<String>,
    /// Environment variables whose values are part of the key.
    pub env: Vec<String>,
    /// `package/target` patterns that always run.
    pub always_run: Vec<String>,
    /// Paths, relative to the target profile directory, of binaries tests spawn.
    pub spawns: Vec<String>,
}

impl TestcacheConfig {
    /// Reads the table from a `metadata` value of `cargo metadata`'s output.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The workspace's or a package's `metadata` value, if any.
    /// * `location` - Where it came from, for error messages.
    ///
    /// # Returns
    ///
    /// The table, or the default when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidManifest`] when the table has an unknown key or a value of the wrong type.
    pub fn from_metadata(metadata: Option<&Value>, location: &str) -> Result<Self> {
        let Some(table) = metadata.and_then(|metadata| metadata.get(TABLE)) else {
            return Ok(Self::default());
        };
        // `&Value` is itself a serde deserializer, so no clone is needed.
        Self::deserialize(table).map_err(|error| Error::InvalidManifest {
            location: location.to_owned(),
            message: error.to_string(),
        })
    }
}

/// Compiles one glob, with `*` and `?` stopping at `/` (only `**` crosses directories).
///
/// # Arguments
///
/// * `pattern` - The glob.
///
/// # Returns
///
/// The compiled glob.
///
/// # Errors
///
/// [`Error::InvalidGlob`] when it does not compile.
pub fn glob(pattern: &str) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|source| Error::InvalidGlob {
            pattern: pattern.to_owned(),
            source,
        })
}

/// The `always_run` patterns, matched against `package/target`.
#[derive(Debug, Clone)]
pub struct AlwaysRun(GlobSet);

impl AlwaysRun {
    /// Compiles the patterns.
    ///
    /// # Arguments
    ///
    /// * `patterns` - `package/target` globs.
    ///
    /// # Returns
    ///
    /// The matcher.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a pattern does not compile.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(glob(pattern)?);
        }
        builder.build().map(Self).map_err(|source| Error::InvalidGlob {
            pattern: "always_run".to_owned(),
            source,
        })
    }

    /// Whether a target must always run.
    ///
    /// # Arguments
    ///
    /// * `package` - The package name.
    /// * `target` - The target name.
    ///
    /// # Returns
    ///
    /// `true` if any pattern matches `package/target`.
    pub fn matches(&self, package: &str, target: &str) -> bool {
        self.0.is_match(format!("{package}/{target}"))
    }
}
```

Add `pub mod manifest;` to `lib.rs` (alphabetical, after `key`).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core manifest::tests`
Expected: PASS, 4 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/manifest.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(manifest): read [*.metadata.testcache] tables"
```

### Task 5: `workspace`: what `cargo metadata` says (design 4.4)

**Files:**
- Create: `crates/cargo-testcache-core/src/workspace.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod workspace;`)

- [ ] **Step 1: Write the tests**

Create `workspace.rs` with Step 3's items as `todo!()` stubs, then the shared fixture and the
tests:

```rust
/// A `cargo metadata` fixture shared with `exec`'s tests.
#[cfg(test)]
pub(crate) mod testing {
    use serde_json::json;

    /// The id of the fixture's one package, at /ws/crates/a.
    pub(crate) const ID: &str = "path+file:///ws/crates/a#0.1.0";

    /// `cargo metadata` output for one package at /ws/crates/a, with the target directory at
    /// /ws/target and, when given, a separate build directory.
    pub(crate) fn metadata(version: &str, build_directory: Option<&str>) -> Vec<u8> {
        json!({
            "workspace_root": "/ws",
            "target_directory": "/ws/target",
            "build_directory": build_directory,
            "metadata": { "testcache": { "inputs": ["data/**"], "env": ["W"] } },
            "packages": [{
                "id": ID, "name": "a", "version": version,
                "authors": ["A <a@example.com>", "B"],
                "description": null, "homepage": null, "repository": null,
                "license": "MIT", "license_file": null, "readme": "README.md",
                "rust_version": "1.89",
                "manifest_path": "/ws/crates/a/Cargo.toml",
                "metadata": { "testcache": { "env": ["P"] } },
            }],
        })
        .to_string()
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{ID, metadata};
    use super::*;

    use std::ffi::OsStr;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    fn env_value<'a>(package: &'a Package, name: &str) -> &'a OsStr {
        package
            .env()
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_os_str())
            .unwrap_or_else(|| panic!("{name} is set"))
    }

    // Property: CARGO_PKG_VERSION_* split the version as Cargo does: the pre-release is what
    // follows `-`, and build metadata after `+` is dropped from every part but the whole.
    #[hegel::test]
    fn version_parts_are_split_as_cargo_does(tc: TestCase) {
        let [major, minor, patch] = [0, 1, 2].map(|_| tc.draw(gs::integers::<u32>()));
        let pre = tc.draw(gs::optional(gs::text().alphabet("abc.1").min_size(1).max_size(6)));
        let build = tc.draw(gs::optional(gs::text().alphabet("xyz.9").min_size(1).max_size(6)));
        let mut version = format!("{major}.{minor}.{patch}");
        if let Some(pre) = &pre {
            version = format!("{version}-{pre}");
        }
        if let Some(build) = &build {
            version = format!("{version}+{build}");
        }

        let workspace = Workspace::from_json(&metadata(&version, None)).expect("valid metadata");
        let package = workspace.package(ID).expect("the package is listed");

        assert_eq!(env_value(package, "CARGO_PKG_VERSION"), version.as_str());
        assert_eq!(env_value(package, "CARGO_PKG_VERSION_MAJOR"), major.to_string().as_str());
        assert_eq!(env_value(package, "CARGO_PKG_VERSION_MINOR"), minor.to_string().as_str());
        assert_eq!(env_value(package, "CARGO_PKG_VERSION_PATCH"), patch.to_string().as_str());
        assert_eq!(env_value(package, "CARGO_PKG_VERSION_PRE"), pre.unwrap_or_default().as_str());
        assert_eq!(env_value(package, "CARGO_PKG_AUTHORS"), "A <a@example.com>:B");
        assert_eq!(env_value(package, "CARGO_PKG_RUST_VERSION"), "1.89");
        assert_eq!(env_value(package, "CARGO_PKG_DESCRIPTION"), "");
        assert_eq!(env_value(package, "CARGO_MANIFEST_DIR"), "/ws/crates/a");
        assert_eq!(package.root(), Path::new("/ws/crates/a"));
    }

    // Property: executables under the build directory map to the same relative place under the
    // target directory; without a separate build directory the two are the same.
    #[hegel::test]
    fn artifact_dir_mirrors_the_build_directory(tc: TestCase) {
        let separate = tc.draw(gs::booleans());
        let profile = tc.draw(gs::vecs(gs::text().alphabet("abc-").min_size(1).max_size(5))
            .min_size(1).max_size(2));
        let build_dir = if separate { "/cache/build" } else { "/ws/target" };
        let workspace = Workspace::from_json(&metadata("0.1.0", separate.then_some(build_dir)))
            .expect("valid metadata");
        let relative: PathBuf = profile.iter().collect();
        let executable = Path::new(build_dir).join(&relative).join("deps").join("t-0123");

        let artifact_dir = workspace.artifact_dir(&executable);

        assert_eq!(artifact_dir, Some(Path::new("/ws/target").join(relative)));
    }

    // Property: the workspace's and the package's `env` names both apply to the package.
    #[hegel::test]
    fn env_names_add_up(tc: TestCase) {
        let _ = tc.draw(gs::booleans());
        let workspace = Workspace::from_json(&metadata("0.1.0", None)).expect("valid metadata");
        let package = workspace.package(ID).expect("listed");

        let names: Vec<&str> = workspace.env_names(package).collect();

        assert_eq!(names, ["W", "P"]);
        assert_eq!(workspace.config().inputs, ["data/**"]);
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core workspace::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! The workspace as `cargo metadata` describes it: its directories, its packages, the
//! environment `cargo test` gives each package's test binaries, and the configuration tables.
//!
//! The JSON is read with this module's own structs rather than the `cargo_metadata` crate,
//! because that crate normalizes `rust-version = "1.89"` to `1.89.0`, while Cargo hands tests
//! the string as written.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

use crate::args::CargoArgs;
use crate::error::{Error, Result};
use crate::manifest::{AlwaysRun, TestcacheConfig};

/// The directory under the target directory that holds the store.
pub const STORE_DIR: &str = "testcache";

/// Cargo flags that `cargo metadata` must see too, because they change what it reads.
const METADATA_SWITCHES: [&str; 3] = ["--locked", "--offline", "--frozen"];

/// The fields of `cargo metadata --format-version 1` this tool uses.
#[derive(Debug, Deserialize)]
struct RawMetadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    /// Present since Cargo 1.91; the target directory before that.
    build_directory: Option<PathBuf>,
    /// `[workspace.metadata]`.
    metadata: Option<Value>,
    packages: Vec<RawPackage>,
}

/// One package of `cargo metadata`'s output. A missing `Option` field reads as `None`.
#[derive(Debug, Deserialize)]
struct RawPackage {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    authors: Vec<String>,
    description: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    license: Option<String>,
    license_file: Option<String>,
    readme: Option<String>,
    rust_version: Option<String>,
    manifest_path: PathBuf,
    /// `[package.metadata]`.
    metadata: Option<Value>,
}

/// A package: where it lives, its test environment and its configuration table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    name: String,
    root: PathBuf,
    env: Vec<(&'static str, OsString)>,
    config: TestcacheConfig,
}

impl Package {
    /// The package name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory holding its `Cargo.toml`: test binaries' working directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `CARGO_MANIFEST_*` and `CARGO_PKG_*` variables `cargo test` sets for its binaries.
    pub fn env(&self) -> &[(&'static str, OsString)] {
        &self.env
    }

    /// Its `[package.metadata.testcache]` table.
    pub fn config(&self) -> &TestcacheConfig {
        &self.config
    }
}

/// The workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
    target_dir: PathBuf,
    build_dir: PathBuf,
    config: TestcacheConfig,
    packages: HashMap<String, Package>,
}

impl Workspace {
    /// Runs `cargo metadata` and reads its output.
    ///
    /// # Arguments
    ///
    /// * `cargo` - The cargo executable.
    /// * `args` - The `cargo test` arguments; `--manifest-path`, `--target-dir`, `--locked`,
    ///   `--offline` and `--frozen` are passed on.
    ///
    /// # Returns
    ///
    /// The workspace.
    ///
    /// # Errors
    ///
    /// When `cargo metadata` cannot run or fails, or its output or a configuration table is
    /// invalid.
    pub fn load(cargo: &Path, args: &CargoArgs) -> Result<Self> {
        let mut command = Command::new(cargo);
        command
            .args(["metadata", "--format-version", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(manifest) = args.value("--manifest-path") {
            command.arg("--manifest-path").arg(manifest);
        }
        // `cargo metadata` has no `--target-dir`; the variable is equivalent.
        if let Some(target_dir) = args.value("--target-dir") {
            command.env("CARGO_TARGET_DIR", target_dir);
        }
        command.args(METADATA_SWITCHES.into_iter().filter(|flag| args.has(flag)));
        let output = command.output().map_err(Error::io("running cargo metadata"))?;
        if !output.status.success() {
            return Err(Error::CommandFailed {
                command: "cargo metadata".to_owned(),
                status: output.status,
            });
        }
        Self::from_json(&output.stdout)
    }

    /// Reads `cargo metadata --format-version 1` output.
    ///
    /// # Arguments
    ///
    /// * `json` - The output.
    ///
    /// # Returns
    ///
    /// The workspace.
    ///
    /// # Errors
    ///
    /// [`Error::Json`] for malformed output, [`Error::InvalidManifest`] for a bad table.
    pub fn from_json(json: &[u8]) -> Result<Self> {
        let raw: RawMetadata = serde_json::from_slice(json).map_err(|source| Error::Json {
            what: "cargo metadata output".to_owned(),
            source,
        })?;
        let config = TestcacheConfig::from_metadata(raw.metadata.as_ref(), "[workspace.metadata]")?;
        let packages = raw
            .packages
            .into_iter()
            .map(|package| {
                let location = format!("package `{}`", package.name);
                let config = TestcacheConfig::from_metadata(package.metadata.as_ref(), &location)?;
                Ok((package.id.clone(), Package::new(package, config)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(Self {
            build_dir: raw.build_directory.unwrap_or_else(|| raw.target_directory.clone()),
            root: raw.workspace_root,
            target_dir: raw.target_directory,
            config,
            packages,
        })
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The target directory (`bin` executables, and the cache).
    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// The build directory (test executables); the target directory unless configured apart.
    pub fn build_dir(&self) -> &Path {
        &self.build_dir
    }

    /// Where the store lives: `<target-dir>/testcache`.
    pub fn cache_dir(&self) -> PathBuf {
        self.target_dir.join(STORE_DIR)
    }

    /// The `[workspace.metadata.testcache]` table.
    pub fn config(&self) -> &TestcacheConfig {
        &self.config
    }

    /// A package by its id.
    pub fn package(&self, id: &str) -> Option<&Package> {
        self.packages.get(id)
    }

    /// Every package, with its id.
    pub fn packages(&self) -> impl Iterator<Item = (&str, &Package)> {
        self.packages.iter().map(|(id, package)| (id.as_str(), package))
    }

    /// The environment variable names whose values are part of a package's keys.
    pub fn env_names<'a>(&'a self, package: &'a Package) -> impl Iterator<Item = &'a str> {
        self.config.env.iter().chain(&package.config.env).map(String::as_str)
    }

    /// The `spawns` paths that apply to a package.
    pub fn spawns<'a>(&'a self, package: &'a Package) -> impl Iterator<Item = &'a str> {
        self.config.spawns.iter().chain(&package.config.spawns).map(String::as_str)
    }

    /// Every `always_run` pattern, the workspace's and every package's.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a pattern does not compile.
    pub fn always_run(&self) -> Result<AlwaysRun> {
        let packages = self.packages.values().flat_map(|package| &package.config.always_run);
        AlwaysRun::new(self.config.always_run.iter().chain(packages).map(String::as_str))
    }

    /// The target-directory profile directory (`target/debug`) matching a test executable in
    /// the build directory (`<build-dir>/debug/deps/name-hash`). `CARGO_BIN_EXE_*` and `spawns`
    /// paths live there.
    ///
    /// # Arguments
    ///
    /// * `executable` - A test executable.
    ///
    /// # Returns
    ///
    /// The directory, or `None` if the executable is not under the build directory.
    pub fn artifact_dir(&self, executable: &Path) -> Option<PathBuf> {
        let profile_dir = executable.parent()?.parent()?;
        let relative = profile_dir.strip_prefix(&self.build_dir).ok()?;
        Some(self.target_dir.join(relative))
    }
}

impl Package {
    /// Builds a package from its metadata.
    fn new(raw: RawPackage, config: TestcacheConfig) -> Self {
        let root = raw.manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            env: package_env(&raw, &root),
            name: raw.name,
            root,
            config,
        }
    }
}

/// The variables `cargo test` sets for a package's test binaries (checked against Cargo 1.99).
fn package_env(raw: &RawPackage, root: &Path) -> Vec<(&'static str, OsString)> {
    let without_build = raw.version.split_once('+').map_or(raw.version.as_str(), |(v, _)| v);
    let (numbers, pre) = without_build.split_once('-').unwrap_or((without_build, ""));
    let mut numbers = numbers.splitn(3, '.');
    // A closure that captures `numbers` mutably; each call takes the next component.
    let mut next_number = || OsString::from(numbers.next().unwrap_or_default());
    let text = |value: &Option<String>| OsString::from(value.as_deref().unwrap_or_default());
    vec![
        ("CARGO_MANIFEST_DIR", root.as_os_str().to_owned()),
        ("CARGO_MANIFEST_PATH", raw.manifest_path.as_os_str().to_owned()),
        ("CARGO_PKG_AUTHORS", raw.authors.join(":").into()),
        ("CARGO_PKG_DESCRIPTION", text(&raw.description)),
        ("CARGO_PKG_HOMEPAGE", text(&raw.homepage)),
        ("CARGO_PKG_LICENSE", text(&raw.license)),
        ("CARGO_PKG_LICENSE_FILE", text(&raw.license_file)),
        ("CARGO_PKG_NAME", OsString::from(&raw.name)),
        ("CARGO_PKG_README", text(&raw.readme)),
        ("CARGO_PKG_REPOSITORY", text(&raw.repository)),
        ("CARGO_PKG_RUST_VERSION", text(&raw.rust_version)),
        ("CARGO_PKG_VERSION", OsString::from(&raw.version)),
        ("CARGO_PKG_VERSION_MAJOR", next_number()),
        ("CARGO_PKG_VERSION_MINOR", next_number()),
        ("CARGO_PKG_VERSION_PATCH", next_number()),
        ("CARGO_PKG_VERSION_PRE", OsString::from(pre)),
    ]
}
```

Add `pub mod workspace;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core workspace::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/workspace.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(workspace): read cargo metadata and each package's test environment"
```

### Task 6: `discover`: build and find the test binaries (design 4.1)

**Files:**
- Create: `crates/cargo-testcache-core/src/discover.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod discover;`)

- [ ] **Step 1: Write the tests**

Create `discover.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};
    use serde_json::json;

    /// One artifact the test feeds in.
    #[derive(Debug, Clone)]
    struct Fed {
        package: &'static str,
        kind: &'static str,
        name: String,
        test: bool,
        executable: bool,
    }

    fn draw_artifact(tc: &TestCase) -> Fed {
        Fed {
            package: tc.draw(gs::sampled_from(&["p1", "p2"]).print_as_debug()),
            kind: tc.draw(
                gs::sampled_from(&["lib", "proc-macro", "bin", "test", "bench", "example",
                    "custom-build"]).print_as_debug(),
            ),
            name: tc.draw(gs::text().alphabet("ab").min_size(1).max_size(3)),
            test: tc.draw(gs::booleans()),
            executable: tc.draw(gs::booleans()),
        }
    }

    fn line(fed: &Fed) -> String {
        let executable = fed.executable.then(|| format!("/exe/{}-{}", fed.package, fed.name));
        json!({
            "reason": "compiler-artifact", "package_id": fed.package,
            "manifest_path": "/ws/Cargo.toml",
            "target": { "name": fed.name, "kind": [fed.kind], "src_path": "/ws/src/x.rs",
                "edition": "2024", "doctest": false, "test": true },
            "profile": { "opt_level": "0", "debuginfo": 2, "debug_assertions": true,
                "overflow_checks": true, "test": fed.test },
            "features": [], "filenames": [], "executable": executable, "fresh": false,
        })
        .to_string()
    }

    // Property: test binaries are exactly the artifacts built for test with an executable, in
    // order; package bins are the non-test `bin` executables; other lines are ignored.
    #[hegel::test]
    fn messages_yield_binaries_and_package_bins(tc: TestCase) {
        let fed: Vec<Fed> = (0..tc.draw(gs::integers::<usize>().max_value(8)))
            .map(|_| draw_artifact(&tc))
            .collect();
        let mut output: Vec<String> = fed.iter().map(line).collect();
        output.push(json!({ "reason": "build-finished", "success": true }).to_string());
        output.push("not json".to_owned());
        output.push(
            json!({ "reason": "build-script-executed", "package_id": "p1",
                "linked_libs": [], "linked_paths": ["native=/t/native", "/t/plain"],
                "cfgs": [], "env": [], "out_dir": "/t/out" })
            .to_string(),
        );

        let build = parse(Cursor::new(output.join("\n"))).expect("parse");

        let expected: Vec<(String, String)> = fed
            .iter()
            .filter(|f| f.test && f.executable && f.kind != "custom-build")
            .map(|f| (f.package.to_owned(), f.name.clone()))
            .collect();
        let found: Vec<(String, String)> = build
            .binaries
            .iter()
            .map(|b| (b.id.package_id.clone(), b.id.name.clone()))
            .collect();
        assert_eq!(found, expected);
        for f in fed.iter().filter(|f| !f.test && f.executable && f.kind == "bin") {
            assert!(build.package_bins[f.package].contains_key(&f.name), "{f:?}");
        }
        let bin_count: usize = build.package_bins.values().map(BTreeMap::len).sum();
        assert!(bin_count <= fed.iter().filter(|f| !f.test && f.kind == "bin").count());
        assert_eq!(build.linked_paths, [PathBuf::from("/t/native"), PathBuf::from("/t/plain")]);
    }

    // Property: a `kind=` prefix is stripped from a linked path; anything else is kept whole.
    #[hegel::test]
    fn linked_paths_lose_their_kind_prefix(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&LINK_KINDS).print_as_debug());
        let path = tc.draw(gs::text().alphabet("/a=b").max_size(8));

        assert_eq!(linked_path(&format!("{kind}={path}")), PathBuf::from(&path));
        if !LINK_KINDS.iter().any(|k| path.starts_with(&format!("{k}="))) {
            assert_eq!(linked_path(&path), PathBuf::from(&path));
        }
    }

    // Property: the kind order is Cargo's run order: lib, bin, test, bench, example.
    #[hegel::test]
    fn kinds_sort_in_cargos_order(tc: TestCase) {
        let mut kinds = tc.draw(gs::vecs(
            gs::sampled_from(&[TargetKind::Example, TargetKind::Test, TargetKind::Lib,
                TargetKind::Bench, TargetKind::Bin]).print_as_debug(),
        ).max_size(6));
        kinds.sort();
        let rank = |kind: &TargetKind| ["lib", "bin", "test", "bench", "example"]
            .iter()
            .position(|name| *name == kind.as_str());
        assert!(kinds.windows(2).all(|pair| rank(&pair[0]) <= rank(&pair[1])));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core discover::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! Building the test binaries with `cargo test --no-run` and reading what Cargo built.
//!
//! Cargo prints one JSON message per line on stdout (`--message-format=json-render-diagnostics`
//! still renders compiler errors on stderr). A test binary is a `compiler-artifact` message whose
//! `profile.test` is true and that has an `executable`; a package's `bin` executables, which
//! integration tests reach through `CARGO_BIN_EXE_<name>`, are the `bin` artifacts built
//! without `test`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use serde::{Deserialize, Serialize};

use crate::args::CargoArgs;
use crate::error::{Error, Result};

/// Prefixes Cargo puts on a build script's `rustc-link-search` paths.
const LINK_KINDS: [&str; 5] = ["dependency", "crate", "native", "framework", "all"];

/// What kind of target a test binary was built from. The order is the order `cargo test` runs
/// them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A library (including a proc-macro): `--lib`.
    Lib,
    /// A binary's unit tests: `--bin <name>`.
    Bin,
    /// An integration test: `--test <name>`.
    Test,
    /// A benchmark built as a test: `--bench <name>`.
    Bench,
    /// An example built as a test: `--example <name>`.
    Example,
}

impl TargetKind {
    /// Reads Cargo's kind list (`["lib"]`, `["proc-macro"]`, `["test"]`).
    fn from_cargo(kinds: &[String]) -> Option<Self> {
        kinds.iter().find_map(|kind| match kind.as_str() {
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => Some(Self::Lib),
            "bin" => Some(Self::Bin),
            "test" => Some(Self::Test),
            "bench" => Some(Self::Bench),
            "example" => Some(Self::Example),
            _ => None,
        })
    }

    /// The kind's name, as Cargo's messages spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::Test => "test",
            Self::Bench => "bench",
            Self::Example => "example",
        }
    }

    /// Whether `cargo test` gives this kind `CARGO_BIN_EXE_*`: integration tests and benchmarks.
    pub fn spawns_package_bins(self) -> bool {
        matches!(self, Self::Test | Self::Bench)
    }
}

/// Which target a test binary is: what the store keys its records by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetId {
    /// Cargo's package id, such as `path+file:///ws/crates/a#0.1.0`.
    pub package_id: String,
    /// The target kind.
    pub kind: TargetKind,
    /// The target name.
    pub name: String,
}

/// A test binary Cargo built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBinary {
    /// The target it was built from.
    pub id: TargetId,
    /// The target's root source file.
    pub src_path: PathBuf,
    /// The executable.
    pub executable: PathBuf,
}

/// What `cargo test --no-run` built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Build {
    /// The test binaries, in message order.
    pub binaries: Vec<TestBinary>,
    /// Each package's `bin` executables by name: the `CARGO_BIN_EXE_<name>` values.
    pub package_bins: HashMap<String, BTreeMap<String, PathBuf>>,
    /// Build scripts' `rustc-link-search` directories, without their `kind=` prefix.
    pub linked_paths: Vec<PathBuf>,
}

/// The messages this module reads. `#[serde(tag = "reason")]` picks the variant by the
/// `reason` field, like a discriminated union; every other reason becomes `Other`.
#[derive(Debug, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum Message {
    CompilerArtifact(ArtifactMessage),
    BuildScriptExecuted(BuildScriptMessage),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ArtifactMessage {
    package_id: String,
    target: TargetMessage,
    profile: ProfileMessage,
    executable: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct TargetMessage {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ProfileMessage {
    test: bool,
}

#[derive(Debug, Deserialize)]
struct BuildScriptMessage {
    linked_paths: Vec<String>,
}

impl Build {
    /// Files one artifact as a test binary, a package bin, or neither.
    fn add_artifact(&mut self, artifact: ArtifactMessage) {
        let (Some(kind), Some(executable)) =
            (TargetKind::from_cargo(&artifact.target.kind), artifact.executable)
        else {
            return;
        };
        if artifact.profile.test {
            self.binaries.push(TestBinary {
                id: TargetId {
                    package_id: artifact.package_id,
                    kind,
                    name: artifact.target.name,
                },
                src_path: artifact.target.src_path,
                executable,
            });
        } else if kind == TargetKind::Bin {
            self.package_bins
                .entry(artifact.package_id)
                .or_default()
                .insert(artifact.target.name, executable);
        }
    }
}

/// Builds every test binary `cargo test` would, with the user's arguments.
///
/// # Arguments
///
/// * `cargo` - The cargo executable.
/// * `args` - The user's `cargo test` arguments.
///
/// # Returns
///
/// Cargo's exit status and what it built. Diagnostics and progress go straight to stderr.
///
/// # Errors
///
/// When Cargo cannot be started or its output cannot be read.
pub fn build(cargo: &Path, args: &CargoArgs) -> Result<(ExitStatus, Build)> {
    let mut child = Command::new(cargo)
        .args(["test", "--no-run", "--message-format=json-render-diagnostics"])
        .args(args.as_slice())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(Error::io("running cargo test --no-run"))?;
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let build = parse(BufReader::new(stdout));
    let status = child.wait().map_err(Error::io("waiting for cargo test --no-run"))?;
    Ok((status, build?))
}

/// Reads Cargo's JSON messages.
///
/// # Arguments
///
/// * `output` - Cargo's stdout, one message per line.
///
/// # Returns
///
/// What was built. Lines that are not messages are logged at debug level and skipped.
///
/// # Errors
///
/// When reading fails.
pub fn parse(output: impl BufRead) -> Result<Build> {
    let mut build = Build::default();
    for line in output.lines() {
        let line = line.map_err(Error::io("reading cargo's build output"))?;
        match serde_json::from_str::<Message>(&line) {
            Ok(Message::CompilerArtifact(artifact)) => build.add_artifact(artifact),
            Ok(Message::BuildScriptExecuted(script)) => build
                .linked_paths
                .extend(script.linked_paths.iter().map(|path| linked_path(path))),
            Ok(Message::Other) => {}
            Err(error) => tracing::debug!(%error, line, "skipping a line that is not a message"),
        }
    }
    Ok(build)
}

/// Strips Cargo's `kind=` prefix (`native=/path`) from a linked path.
fn linked_path(path: &str) -> PathBuf {
    match path.split_once('=') {
        Some((kind, rest)) if LINK_KINDS.contains(&kind) => PathBuf::from(rest),
        _ => PathBuf::from(path),
    }
}
```

Add `pub mod discover;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core discover::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/discover.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(discover): build with --no-run and read the test binaries"
```

### Task 7: `inputs`: declared input files (design 4.2 part 3, section 5)

**Files:**
- Create: `crates/cargo-testcache-core/src/inputs.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod inputs;`)

The glob matching is pure and property-tested here. The walk and the hashing touch the
filesystem and are covered by the fixture tests of task 14 (items 3 of design section 9).

- [ ] **Step 1: Write the tests**

Create `inputs.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::key::testing::draw_digest;

    /// The workspace has `data/**`; packages a and b each have `fixtures/**` under their own
    /// directory (which may need escaping); package c is outside the workspace root.
    fn patterns(a_dir: &str) -> InputPatterns {
        let fixtures = ["fixtures/**".to_owned()];
        InputPatterns::from_parts(
            &["data/**".to_owned(), "*.txt".to_owned()],
            [
                PackageGlobs { id: "a", dir: Some(a_dir.to_owned()), globs: &fixtures },
                PackageGlobs { id: "b", dir: Some("crates/b".to_owned()), globs: &fixtures },
                PackageGlobs { id: "c", dir: None, globs: &fixtures },
                PackageGlobs { id: "d", dir: Some("crates/d".to_owned()), globs: &[] },
            ],
        )
        .expect("valid globs")
    }

    fn draw_name(tc: &TestCase) -> String {
        tc.draw(gs::text().alphabet("xyz.").min_size(1).max_size(6))
    }

    // Property: a file is owned by every glob that matches it and by no other, package globs
    // are rooted at their package's directory even when it holds glob metacharacters, and `*`
    // does not cross `/`.
    #[hegel::test]
    fn files_are_owned_by_the_globs_that_match_them(tc: TestCase) {
        let a_dir = tc.draw(gs::sampled_from(&["crates/a", "we[i]rd/*a", ""]).print_as_debug());
        let name = draw_name(&tc);
        let patterns = patterns(a_dir);
        let in_a = if a_dir.is_empty() {
            format!("fixtures/{name}")
        } else {
            format!("{a_dir}/fixtures/{name}")
        };

        assert_eq!(patterns.owners_of(&in_a), [Owner::Package(0)]);
        assert_eq!(patterns.owners_of(&format!("crates/b/fixtures/{name}")), [Owner::Package(1)]);
        assert_eq!(patterns.owners_of(&format!("data/{name}")), [Owner::Workspace]);
        assert!(patterns.owners_of(&format!("crates/d/fixtures/{name}")).is_empty());
        assert!(patterns.owners_of(&format!("crates/b/{name}.txt")).is_empty());
        assert_eq!(patterns.owners_of(&format!("{name}.txt")), [Owner::Workspace]);
    }

    // Property: a package's inputs are the workspace's plus its own; an unreadable one, or a
    // package outside the workspace root, makes them unavailable.
    #[hegel::test]
    fn package_inputs_are_the_workspace_and_its_own(tc: TestCase) {
        let patterns = patterns("crates/a");
        let (shared, own_a, own_b) = (draw_digest(&tc), draw_digest(&tc), draw_digest(&tc));
        let a_readable = tc.draw(gs::booleans());
        let file = |relative: &str, digest: std::result::Result<Digest, String>| InputFile {
            owners: patterns.owners_of(relative),
            relative: relative.to_owned(),
            digest,
        };
        let files = vec![
            file("data/x", Ok(shared)),
            file("crates/a/fixtures/y", if a_readable { Ok(own_a) } else { Err("denied".into()) }),
            file("crates/b/fixtures/z", Ok(own_b)),
        ];
        let index = InputIndex { patterns, files };

        let for_a = index.for_package("a");
        let for_b = index.for_package("b").expect("b's inputs are readable");

        if a_readable {
            let expected = BTreeMap::from([
                ("crates/a/fixtures/y".to_owned(), own_a),
                ("data/x".to_owned(), shared),
            ]);
            assert_eq!(for_a, Ok(expected));
        } else {
            assert!(for_a.expect_err("unreadable").contains("crates/a/fixtures/y"));
        }
        assert_eq!(for_b.keys().collect::<Vec<_>>(), ["crates/b/fixtures/z", "data/x"]);
        assert!(index.for_package("c").is_err());
        assert_eq!(index.for_package("d").map(|inputs| inputs.len()), Ok(1));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core inputs::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! Files tests read at runtime: matched by the configured `inputs` globs, and hashed.
//!
//! The workspace is walked once. Each file is matched against one compiled set holding the
//! workspace's globs (relative to its root) and every package's (relative to that package's
//! directory, rewritten to be relative to the root), and the matching files are hashed in
//! parallel. The target and build directories and `.git` are not walked.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{GlobSet, GlobSetBuilder};
// The prelude adds `into_par_iter()`, rayon's parallel counterpart of `into_iter()`.
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::key::Digest;
use crate::manifest::glob;
use crate::workspace::Workspace;

/// Who declared a glob: the workspace, or the package at this index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Owner {
    Workspace,
    Package(usize),
}

/// One package's `inputs`, for [`InputPatterns::from_parts`].
#[derive(Debug, Clone)]
pub struct PackageGlobs<'a> {
    /// The package id.
    pub id: &'a str,
    /// The package directory relative to the workspace root, with `/` separators; `None` when
    /// it is outside the root.
    pub dir: Option<String>,
    /// Its globs, relative to its directory.
    pub globs: &'a [String],
}

/// Every `inputs` glob, compiled into one set.
#[derive(Debug, Clone)]
pub struct InputPatterns {
    set: GlobSet,
    /// The owner of each glob in `set`, by index.
    owners: Vec<Owner>,
    /// Package id to its `Owner::Package` index.
    package_index: HashMap<String, usize>,
    /// Packages that declare inputs but lie outside the workspace root.
    outside: HashSet<String>,
}

impl InputPatterns {
    /// Compiles the workspace's and every package's globs.
    ///
    /// # Arguments
    ///
    /// * `workspace` - The workspace.
    ///
    /// # Returns
    ///
    /// The compiled patterns.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile.
    pub fn new(workspace: &Workspace) -> Result<Self> {
        let packages = workspace.packages().map(|(id, package)| PackageGlobs {
            id,
            dir: package.root().strip_prefix(workspace.root()).ok().map(slash_path),
            globs: &package.config().inputs,
        });
        Self::from_parts(&workspace.config().inputs, packages)
    }

    /// Compiles globs given directly.
    ///
    /// # Arguments
    ///
    /// * `workspace_globs` - Globs relative to the workspace root.
    /// * `packages` - Each package's globs and directory.
    ///
    /// # Returns
    ///
    /// The compiled patterns.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile.
    pub fn from_parts<'a>(
        workspace_globs: &[String],
        packages: impl IntoIterator<Item = PackageGlobs<'a>>,
    ) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut owners = Vec::new();
        let mut package_index = HashMap::new();
        let mut outside = HashSet::new();
        for pattern in workspace_globs {
            builder.add(glob(pattern)?);
            owners.push(Owner::Workspace);
        }
        for (index, package) in packages.into_iter().enumerate() {
            if package.globs.is_empty() {
                continue;
            }
            package_index.insert(package.id.to_owned(), index);
            let Some(dir) = package.dir else {
                outside.insert(package.id.to_owned());
                continue;
            };
            for pattern in package.globs {
                // `escape` makes a directory such as `we[i]rd` match itself literally.
                let rooted = if dir.is_empty() {
                    pattern.clone()
                } else {
                    format!("{}/{pattern}", globset::escape(&dir))
                };
                builder.add(glob(&rooted)?);
                owners.push(Owner::Package(index));
            }
        }
        let set = builder.build().map_err(|source| Error::InvalidGlob {
            pattern: "inputs".to_owned(),
            source,
        })?;
        Ok(Self { set, owners, package_index, outside })
    }

    /// The owners of the globs a workspace-relative path matches, sorted, without repeats.
    fn owners_of(&self, relative: &str) -> Vec<Owner> {
        let mut owners: Vec<Owner> =
            self.set.matches(relative).into_iter().map(|index| self.owners[index]).collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }
}

/// One matched file.
#[derive(Debug, Clone)]
struct InputFile {
    /// Workspace-relative, with `/` separators: the name it has in a key.
    relative: String,
    owners: Vec<Owner>,
    /// Its digest, or why it could not be read.
    digest: std::result::Result<Digest, String>,
}

/// Every matched input file, hashed.
#[derive(Debug, Clone)]
pub struct InputIndex {
    patterns: InputPatterns,
    files: Vec<InputFile>,
}

impl InputIndex {
    /// Walks the workspace and hashes every file an `inputs` glob matches.
    ///
    /// # Arguments
    ///
    /// * `workspace` - The workspace.
    ///
    /// # Returns
    ///
    /// The index. With no globs configured, nothing is walked.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a glob does not compile. Unreadable directories are skipped
    /// with a warning: a test running as the same user could not read them either.
    pub fn collect(workspace: &Workspace) -> Result<Self> {
        let patterns = InputPatterns::new(workspace)?;
        let matched = if patterns.owners.is_empty() {
            Vec::new()
        } else {
            matching_files(workspace, &patterns)
        };
        let files = matched
            .into_par_iter()
            .map(|(relative, owners, path)| InputFile {
                digest: Digest::of_file(&path).map_err(|error| error.to_string()),
                relative,
                owners,
            })
            .collect();
        Ok(Self { patterns, files })
    }

    /// A package's inputs: the files the workspace's globs or its own match.
    ///
    /// # Arguments
    ///
    /// * `package_id` - The package.
    ///
    /// # Returns
    ///
    /// Workspace-relative path to digest, or why the inputs cannot be known (an unreadable
    /// file, or a package outside the workspace root that declares inputs).
    pub fn for_package(
        &self,
        package_id: &str,
    ) -> std::result::Result<BTreeMap<String, Digest>, String> {
        if self.patterns.outside.contains(package_id) {
            return Err("the package declares inputs but is outside the workspace root".to_owned());
        }
        let own = self.patterns.package_index.get(package_id).copied().map(Owner::Package);
        self.files
            .iter()
            .filter(|file| {
                file.owners.iter().any(|owner| *owner == Owner::Workspace || Some(*owner) == own)
            })
            .map(|file| match &file.digest {
                Ok(digest) => Ok((file.relative.clone(), *digest)),
                Err(error) => Err(format!("cannot read input {}: {error}", file.relative)),
            })
            .collect()
    }
}

/// Walks the workspace for files the patterns match.
///
/// # Returns
///
/// Each match's workspace-relative name, owners and path.
fn matching_files(
    workspace: &Workspace,
    patterns: &InputPatterns,
) -> Vec<(String, Vec<Owner>, PathBuf)> {
    let skipped = [workspace.target_dir(), workspace.build_dir()];
    WalkDir::new(workspace.root())
        .into_iter()
        // `filter_entry` stops the walk from descending into the directories it rejects.
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && (entry.file_name() == ".git" || skipped.contains(&entry.path())))
        })
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(%error, "skipping an unreadable path while looking for inputs");
                None
            }
        })
        .filter(|entry| {
            entry.file_type().is_file() || (entry.path_is_symlink() && entry.path().is_file())
        })
        .filter_map(|entry| {
            let relative = slash_path(entry.path().strip_prefix(workspace.root()).ok()?);
            let owners = patterns.owners_of(&relative);
            (!owners.is_empty()).then(|| (relative, owners, entry.into_path()))
        })
        .collect()
}

/// A relative path with `/` separators on every platform, as globs and keys spell it.
fn slash_path(path: &Path) -> String {
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
```

Add `pub mod inputs;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core inputs::tests`
Expected: PASS, 2 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/inputs.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(inputs): match inputs globs per owner and hash the files"
```

### Task 8: `store`: the pass records (design 4.5)

**Files:**
- Create: `crates/cargo-testcache-core/src/store.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod store;`)

- [ ] **Step 1: Write the tests**

Create `store.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetKind;
    use crate::key::testing::draw_parts;

    const NAMES: [&str; 3] = ["a", "b", "c"];

    fn target(name: &str) -> TargetId {
        TargetId { package_id: "p".to_owned(), kind: TargetKind::Test, name: name.to_owned() }
    }

    fn draw_record(tc: &TestCase, name: &str) -> PassRecord {
        let parts = draw_parts(tc);
        PassRecord {
            target: target(name),
            key: parts.key(),
            parts,
            passed_at: tc.draw(gs::integers::<u64>()),
            duration_ms: tc.draw(gs::integers::<u64>()),
        }
    }

    fn draw_records(tc: &TestCase) -> Vec<PassRecord> {
        let names = tc.draw(gs::vecs(gs::sampled_from(&NAMES).print_as_debug()).max_size(6));
        names.into_iter().map(|name| draw_record(tc, name)).collect()
    }

    /// The newest record per target, as `lookup` should see them.
    fn newest(records: &[PassRecord]) -> HashMap<TargetId, PassRecord> {
        records.iter().map(|record| (record.target.clone(), record.clone())).collect()
    }

    fn line_count(dir: &Path) -> usize {
        fs::read(dir.join(PASSES_FILE)).map_or(0, |bytes| bytes.split(|b| *b == b'\n')
            .filter(|line| !line.is_empty()).count())
    }

    // Property (design 9): opening compacts the file to the newest line per target, and drops
    // lines it cannot read.
    #[hegel::test(test_cases = 50)]
    fn opening_keeps_the_newest_line_per_target(tc: TestCase) {
        let records = draw_records(&tc);
        let garbage = tc.draw(gs::booleans());
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut contents = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut contents, record).expect("serialize");
            contents.push(b'\n');
            if garbage {
                contents.extend_from_slice(b"{not json\xff\n");
            }
        }
        fs::write(dir.path().join(PASSES_FILE), contents).expect("write the file");

        let store = Store::open(dir.path()).expect("open");

        let expected = newest(&records);
        for name in NAMES {
            assert_eq!(store.lookup(&target(name)), expected.get(&target(name)), "{name}");
        }
        drop(store);
        assert_eq!(line_count(dir.path()), expected.len());
    }

    // Property: recorded passes survive reopening, and `clear` forgets them all.
    #[hegel::test(test_cases = 50)]
    fn records_persist_until_cleared(tc: TestCase) {
        let records = draw_records(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");

        let mut store = Store::open(dir.path()).expect("open");
        for record in &records {
            store.record(record.clone()).expect("record");
        }
        drop(store);
        let reopened = Store::open(dir.path()).expect("reopen");
        let expected = newest(&records);
        for name in NAMES {
            assert_eq!(reopened.lookup(&target(name)), expected.get(&target(name)), "{name}");
        }
        drop(reopened);

        assert!(Store::clear(dir.path()).expect("clear"));
        let cleared = Store::open(dir.path()).expect("open after clear");
        assert!(NAMES.iter().all(|name| cleared.lookup(&target(name)).is_none()));
    }

    // Property (design 9): concurrent writers under the lock never lose or interleave lines.
    #[hegel::test(test_cases = 10)]
    fn concurrent_writers_never_interleave(tc: TestCase) {
        let per_thread = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let records: Vec<Vec<PassRecord>> = (0..2)
            .map(|thread| {
                (0..per_thread).map(|i| draw_record(&tc, &format!("t{thread}-{i}"))).collect()
            })
            .collect();
        let dir = tempfile::tempdir().expect("temporary directory");

        // `thread::scope` lets the threads borrow `records` and `dir`, and joins them all at
        // the end of the block.
        std::thread::scope(|scope| {
            for batch in &records {
                scope.spawn(|| {
                    let mut store = Store::open(dir.path()).expect("open");
                    for record in batch {
                        store.record(record.clone()).expect("record");
                    }
                });
            }
        });

        let store = Store::open(dir.path()).expect("reopen");
        for record in records.iter().flatten() {
            assert_eq!(store.lookup(&record.target), Some(record));
        }
        drop(store);
        assert_eq!(line_count(dir.path()), 2 * per_thread);
    }

    // Property (design 9): a process spawned while the store is open does not inherit the lock.
    #[cfg(target_os = "linux")]
    #[hegel::test(test_cases = 5)]
    fn spawned_processes_do_not_inherit_the_lock(tc: TestCase) {
        let records = draw_records(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut store = Store::open(dir.path()).expect("open");
        for record in records {
            store.record(record).expect("record");
        }

        let output = std::process::Command::new("sh")
            .args(["-c", r#"for fd in /proc/self/fd/*; do readlink "$fd"; done"#])
            .output()
            .expect("run sh");

        let lock_path = dir.path().join(LOCK_FILE);
        let open_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            !open_files.lines().any(|line| Path::new(line) == lock_path),
            "the child holds {}: {open_files}",
            lock_path.display()
        );
        drop(store);
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core store::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! The record of passing test binaries: `<target-dir>/testcache/passes.jsonl`.
//!
//! One JSON object per line, appended only after a binary exits 0. An advisory lock on
//! `<target-dir>/testcache/lock` serializes runs that share a target directory, as Cargo's own
//! build lock does. Opening the store compacts the file to the newest line per target, so it
//! stays one line per test binary.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::discover::TargetId;
use crate::error::{Error, Result};
use crate::key::{Digest, KeyParts};

/// The records file, inside the store directory.
const PASSES_FILE: &str = "passes.jsonl";
/// The lock file, inside the store directory.
const LOCK_FILE: &str = "lock";

/// One recorded pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassRecord {
    /// The test binary's target.
    pub target: TargetId,
    /// [`KeyParts::key`] of `parts`.
    pub key: Digest,
    /// What the key was computed from, for `--explain`.
    pub parts: KeyParts,
    /// When it passed, in seconds since the Unix epoch.
    pub passed_at: u64,
    /// How long it ran, in milliseconds.
    pub duration_ms: u64,
}

/// The open store. Holds the lock until dropped.
#[derive(Debug)]
pub struct Store {
    records: HashMap<TargetId, PassRecord>,
    passes: File,
    /// Never read: holding the open file is what holds the lock. The leading `_` tells the
    /// compiler that is intended.
    _lock: File,
}

impl Store {
    /// Opens the store in `dir`, creating it, waiting for the lock, and compacting the records.
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory, `<target-dir>/testcache`.
    ///
    /// # Returns
    ///
    /// The store, holding the lock.
    ///
    /// # Errors
    ///
    /// When the directory or files cannot be created, read, locked or rewritten. Lines that do
    /// not parse are dropped with a warning, not an error.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).map_err(Error::io(format!("creating {}", dir.display())))?;
        let lock = lock(dir)?;
        let path = dir.join(PASSES_FILE);
        let (records, lines) = read(&path)?;
        if lines != records.len() {
            rewrite(&path, &records)?;
        }
        let passes = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(Error::io(format!("opening {}", path.display())))?;
        Ok(Self { records, passes, _lock: lock })
    }

    /// The newest recorded pass of a target.
    ///
    /// # Arguments
    ///
    /// * `target` - The target.
    ///
    /// # Returns
    ///
    /// Its record, if it ever passed.
    pub fn lookup(&self, target: &TargetId) -> Option<&PassRecord> {
        self.records.get(target)
    }

    /// Appends a pass.
    ///
    /// # Arguments
    ///
    /// * `record` - The pass.
    ///
    /// # Errors
    ///
    /// When the line cannot be written.
    pub fn record(&mut self, record: PassRecord) -> Result<()> {
        let mut line = serde_json::to_vec(&record).map_err(|source| Error::Json {
            what: "a pass record".to_owned(),
            source,
        })?;
        line.push(b'\n');
        // One `write_all` on a file opened for appending; the lock keeps other runs out.
        self.passes.write_all(&line).map_err(Error::io("appending to the store"))?;
        self.records.insert(record.target.clone(), record);
        Ok(())
    }

    /// Forgets every recorded pass.
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory.
    ///
    /// # Returns
    ///
    /// Whether there was anything to forget.
    ///
    /// # Errors
    ///
    /// When the lock cannot be taken or the file cannot be removed.
    pub fn clear(dir: &Path) -> Result<bool> {
        if !dir.is_dir() {
            return Ok(false);
        }
        let _lock = lock(dir)?;
        let path = dir.join(PASSES_FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { action: format!("removing {}", path.display()), source }),
        }
    }
}

/// Takes the store's lock, waiting (with a message) if another run holds it.
fn lock(dir: &Path) -> Result<File> {
    let path = dir.join(LOCK_FILE);
    // Rust's standard library opens every file with close-on-exec (O_CLOEXEC) on Unix, so a
    // test binary, or a daemon such as sccache that it starts, never inherits the lock.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(Error::io(format!("opening {}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            tracing::info!(lock = %path.display(), "waiting for another cargo-testcache run");
            file.lock().map_err(Error::io(format!("locking {}", path.display())))?;
        }
        Err(TryLockError::Error(source)) => {
            return Err(Error::Io { action: format!("locking {}", path.display()), source });
        }
    }
    Ok(file)
}

/// Reads every record, keeping the last per target.
///
/// # Returns
///
/// The records, and how many lines the file had (more lines than records means it needs
/// compacting).
fn read(path: &Path) -> Result<(HashMap<TargetId, PassRecord>, usize)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((HashMap::new(), 0)),
        Err(source) => {
            return Err(Error::Io { action: format!("reading {}", path.display()), source });
        }
    };
    let mut records = HashMap::new();
    let mut lines = 0;
    // `split` yields raw bytes, so a line that is not UTF-8 is dropped like any bad line
    // instead of failing the whole read.
    for line in BufReader::new(file).split(b'\n') {
        let line = line.map_err(Error::io(format!("reading {}", path.display())))?;
        if line.is_empty() {
            continue;
        }
        lines += 1;
        match serde_json::from_slice::<PassRecord>(&line) {
            Ok(record) => {
                records.insert(record.target.clone(), record);
            }
            Err(error) => tracing::warn!(%error, "dropping an unreadable line from the store"),
        }
    }
    Ok((records, lines))
}

/// Replaces the file with one line per record, through a temporary file and a rename so a crash
/// never leaves it half written.
fn rewrite(path: &Path, records: &HashMap<TargetId, PassRecord>) -> Result<()> {
    let mut sorted: Vec<&PassRecord> = records.values().collect();
    sorted.sort_by(|a, b| a.target.cmp(&b.target));
    let mut contents = Vec::new();
    for record in sorted {
        serde_json::to_writer(&mut contents, record).map_err(|source| Error::Json {
            what: "a pass record".to_owned(),
            source,
        })?;
        contents.push(b'\n');
    }
    let temporary = path.with_extension("jsonl.tmp");
    fs::write(&temporary, contents).map_err(Error::io(format!("writing {}", temporary.display())))?;
    fs::rename(&temporary, path).map_err(Error::io(format!("replacing {}", path.display())))
}
```

Add `pub mod store;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core store::tests`
Expected: PASS, 4 tests (3 off Linux).

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/store.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(store): locked, compacting JSON-lines store of passes"
```

### Task 9: `plan`: run or skip, and why (design 4.3, `--explain`)

**Files:**
- Create: `crates/cargo-testcache-core/src/plan.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod plan;`)

- [ ] **Step 1: Write the tests**

Create `plan.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetKind;
    use crate::key::testing::{draw_parts, mutate_one_part};

    fn target() -> TargetId {
        TargetId { package_id: "p".to_owned(), kind: TargetKind::Lib, name: "p".to_owned() }
    }

    fn record_of(parts: KeyParts) -> PassRecord {
        pass_record(&target(), parts, true, Duration::from_millis(5), 7)
            .expect("a pass is recorded")
    }

    // Property (design 9): with no policy in the way, a binary is skipped exactly when its
    // current parts are the ones recorded, and a run says why.
    #[hegel::test]
    fn skipped_exactly_when_the_key_is_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let history = tc.draw(gs::integers::<u8>().max_value(2));
        let which = tc.draw(gs::integers::<u8>().max_value(5));
        let name = tc.draw(gs::text().max_size(6));
        let previous = match history {
            0 => None,
            1 => Some(record_of(parts.clone())),
            _ => {
                let mut changed = parts.clone();
                mutate_one_part(&mut changed, which, name);
                Some(record_of(changed))
            }
        };

        let decision = decide(&Ok(parts), previous.as_ref(), Policy::default());

        match (history, decision) {
            (0, Decision::Run(RunReason::NeverPassed)) => {}
            (1, Decision::Skip { passed_at }) => assert_eq!(passed_at, 7),
            (2, Decision::Run(RunReason::Changed(changes))) => assert_eq!(changes.len(), 1),
            (history, decision) => panic!("history {history} gave {decision:?}"),
        }
    }

    // Property: --no-cache, always_run and an uncomputable key each force a run.
    #[hegel::test]
    fn policies_and_unknown_keys_force_a_run(tc: TestCase) {
        let parts = draw_parts(&tc);
        let previous = record_of(parts.clone());
        let no_cache = tc.draw(gs::booleans());
        let always_run = tc.draw(gs::booleans());
        let known = tc.draw(gs::booleans());
        let parts = if known { Ok(parts) } else { Err("unreadable".to_owned()) };

        let decision = decide(&parts, Some(&previous), Policy { no_cache, always_run });

        let expected = if no_cache {
            Some(RunReason::NoCache)
        } else if always_run {
            Some(RunReason::AlwaysRun)
        } else if !known {
            Some(RunReason::Unavailable("unreadable".to_owned()))
        } else {
            None
        };
        match expected {
            Some(reason) => assert_eq!(decision, Decision::Run(reason)),
            None => assert!(matches!(decision, Decision::Skip { .. })),
        }
    }

    // Property: changes() names the one input that changed, and nothing else.
    #[hegel::test]
    fn changes_name_the_changed_input(tc: TestCase) {
        let old = draw_parts(&tc);
        let name = tc.draw(gs::text().max_size(6));
        let mut new = old.clone();
        mutate_one_part(&mut new, 3, name.clone());

        assert_eq!(changes(&old, &new), [Change::Input(name.clone())]);
        assert_eq!(Change::Input(name.clone()).to_string(), format!("input {name} changed"));
    }

    // Property (design 9): only a passing run produces a record, and its key is its parts' key.
    #[hegel::test]
    fn only_a_pass_is_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let passed = tc.draw(gs::booleans());
        let millis = tc.draw(gs::integers::<u64>());

        let record = pass_record(&target(), parts.clone(), passed, Duration::from_millis(millis), 1);

        assert_eq!(record.is_some(), passed);
        if let Some(record) = record {
            assert_eq!(record.key, parts.key());
            assert_eq!(record.duration_ms, millis);
        }
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core plan::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! Deciding, for each test binary, whether it runs or is skipped, and why.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::discover::TargetId;
use crate::key::KeyParts;
use crate::store::PassRecord;

/// What forces a binary to run whatever its key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `--no-cache` was given.
    pub no_cache: bool,
    /// The binary matches an `always_run` pattern.
    pub always_run: bool,
}

/// Run or skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The key matches a recorded pass.
    Skip {
        /// When that pass was recorded, in Unix seconds.
        passed_at: u64,
    },
    /// The binary runs, for this reason.
    Run(RunReason),
}

/// Why a binary runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReason {
    /// `--no-cache`.
    NoCache,
    /// An `always_run` pattern matches it.
    AlwaysRun,
    /// A part of its key could not be computed; the message says which.
    Unavailable(String),
    /// It has no recorded pass.
    NeverPassed,
    /// Its key differs from the recorded pass in these parts.
    Changed(Vec<Change>),
}

/// A part of a key that differs from the recorded pass.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// The tool or its key format.
    Tool,
    /// The test executable.
    Binary,
    /// A spawned binary, by its name in the key.
    Spawned(String),
    /// An input file, by its workspace-relative path.
    Input(String),
    /// An environment variable.
    Env(String),
    /// The arguments after `--`.
    Args,
}

/// The summary line's groups of run reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReasonCategory {
    /// [`RunReason::Changed`].
    Changed,
    /// [`RunReason::NeverPassed`].
    NeverPassed,
    /// [`RunReason::AlwaysRun`].
    AlwaysRun,
    /// [`RunReason::NoCache`].
    NoCache,
    /// [`RunReason::Unavailable`].
    Unavailable,
}

impl ReasonCategory {
    /// The group's name in the summary line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::NeverPassed => "never passed",
            Self::AlwaysRun => "always run",
            Self::NoCache => "no cache",
            Self::Unavailable => "key unavailable",
        }
    }
}

impl RunReason {
    /// The reason's group in the summary line.
    pub fn category(&self) -> ReasonCategory {
        match self {
            Self::NoCache => ReasonCategory::NoCache,
            Self::AlwaysRun => ReasonCategory::AlwaysRun,
            Self::Unavailable(_) => ReasonCategory::Unavailable,
            Self::NeverPassed => ReasonCategory::NeverPassed,
            Self::Changed(_) => ReasonCategory::Changed,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tool => formatter.write_str("cargo-testcache changed"),
            Self::Binary => formatter.write_str("binary changed"),
            Self::Spawned(name) => write!(formatter, "spawned {name} changed"),
            Self::Input(path) => write!(formatter, "input {path} changed"),
            Self::Env(name) => write!(formatter, "env {name} changed"),
            Self::Args => formatter.write_str("arguments changed"),
        }
    }
}

impl fmt::Display for RunReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCache => formatter.write_str("--no-cache"),
            Self::AlwaysRun => formatter.write_str("matches always_run"),
            Self::Unavailable(why) => write!(formatter, "key unavailable: {why}"),
            Self::NeverPassed => formatter.write_str("never passed"),
            Self::Changed(changes) => {
                let changes: Vec<String> = changes.iter().map(ToString::to_string).collect();
                formatter.write_str(&changes.join("; "))
            }
        }
    }
}

/// Decides whether a binary runs.
///
/// # Arguments
///
/// * `parts` - The binary's key parts, or why they could not be computed.
/// * `previous` - Its recorded pass, if any.
/// * `policy` - `--no-cache` and `always_run`.
///
/// # Returns
///
/// Skip when the key matches the recorded pass and nothing forces a run; otherwise the first
/// reason that applies, in the order `--no-cache`, `always_run`, unavailable, never passed,
/// changed.
pub fn decide(
    parts: &std::result::Result<KeyParts, String>,
    previous: Option<&PassRecord>,
    policy: Policy,
) -> Decision {
    if policy.no_cache {
        return Decision::Run(RunReason::NoCache);
    }
    if policy.always_run {
        return Decision::Run(RunReason::AlwaysRun);
    }
    let parts = match parts {
        Ok(parts) => parts,
        Err(why) => return Decision::Run(RunReason::Unavailable(why.clone())),
    };
    let Some(previous) = previous else {
        return Decision::Run(RunReason::NeverPassed);
    };
    if previous.key == parts.key() {
        return Decision::Skip { passed_at: previous.passed_at };
    }
    let changes = changes(&previous.parts, parts);
    // Equal parts under a different key can only mean the key's encoding changed.
    let changes = if changes.is_empty() { vec![Change::Tool] } else { changes };
    Decision::Run(RunReason::Changed(changes))
}

/// The parts that differ between a recorded pass and now.
///
/// # Arguments
///
/// * `old` - The recorded parts.
/// * `new` - The current parts.
///
/// # Returns
///
/// One [`Change`] per differing part, or per differing entry of a map part.
pub fn changes(old: &KeyParts, new: &KeyParts) -> Vec<Change> {
    let mut changes = Vec::new();
    if old.tool != new.tool {
        changes.push(Change::Tool);
    }
    if old.binary != new.binary {
        changes.push(Change::Binary);
    }
    changes.extend(changed_names(&old.spawned, &new.spawned).map(Change::Spawned));
    changes.extend(changed_names(&old.inputs, &new.inputs).map(Change::Input));
    changes.extend(changed_names(&old.env, &new.env).map(Change::Env));
    if old.args != new.args {
        changes.push(Change::Args);
    }
    changes
}

/// The names that were removed, added or changed between two maps.
fn changed_names<'a, V: PartialEq>(
    old: &'a BTreeMap<String, V>,
    new: &'a BTreeMap<String, V>,
) -> impl Iterator<Item = String> + 'a {
    let removed_or_changed =
        old.iter().filter(|(name, value)| new.get(*name) != Some(*value)).map(|(name, _)| name);
    let added = new.keys().filter(|name| !old.contains_key(*name));
    removed_or_changed.chain(added).cloned()
}

/// The record of a run, if it passed. Only a zero exit status is ever recorded.
///
/// # Arguments
///
/// * `target` - The binary's target.
/// * `parts` - Its key parts.
/// * `passed` - Whether it exited 0.
/// * `duration` - How long it ran.
/// * `passed_at` - Now, in Unix seconds.
///
/// # Returns
///
/// The record to store, or `None` for a failure.
pub fn pass_record(
    target: &TargetId,
    parts: KeyParts,
    passed: bool,
    duration: Duration,
    passed_at: u64,
) -> Option<PassRecord> {
    passed.then(|| PassRecord {
        target: target.clone(),
        key: parts.key(),
        parts,
        passed_at,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    })
}
```

Add `pub mod plan;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core plan::tests`
Expected: PASS, 4 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/plan.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(plan): decide run or skip, with the reason"
```

### Task 10: `exec`: running a binary as `cargo test` would (design 4.4)

**Files:**
- Create: `crates/cargo-testcache-core/src/exec.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod exec;`)

- [ ] **Step 1: Write the tests**

Create `exec.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, HashMap};
    use std::ffi::OsStr;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;
    use crate::workspace::testing::{ID, metadata};

    const KINDS: [TargetKind; 5] =
        [TargetKind::Lib, TargetKind::Bin, TargetKind::Test, TargetKind::Bench, TargetKind::Example];

    /// The workspace fixture with a separate build directory at /cache/build.
    fn workspace() -> Workspace {
        Workspace::from_json(&metadata("0.1.0", Some("/cache/build"))).expect("valid metadata")
    }

    fn build() -> Build {
        Build {
            binaries: Vec::new(),
            package_bins: HashMap::from([(
                ID.to_owned(),
                BTreeMap::from([("tool".to_owned(), PathBuf::from("/ws/target/debug/tool"))]),
            )]),
            linked_paths: vec![
                PathBuf::from("/ws/target/debug/build/x/out"),
                PathBuf::from("/elsewhere"),
            ],
        }
    }

    fn binary(kind: TargetKind) -> TestBinary {
        TestBinary {
            id: TargetId { package_id: ID.to_owned(), kind, name: "t".to_owned() },
            src_path: PathBuf::from("/ws/crates/a/tests/t.rs"),
            executable: PathBuf::from("/cache/build/debug/deps/t-0123"),
        }
    }

    fn toolchain() -> Toolchain {
        Toolchain::new(PathBuf::from("/sys"), "h".to_owned(), PathBuf::from("/c/cargo"))
    }

    fn get<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_os_str())
    }

    // Property: CARGO_BIN_EXE_* is set exactly for integration tests and benchmarks; the
    // package's variables and CARGO are always set; the library path is the build scripts'
    // directories inside the target, the profile directory, the deps directory, the toolchain's
    // libraries, then what the variable held before.
    #[hegel::test]
    fn the_environment_is_cargos(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        let inherited = tc.draw(gs::booleans());
        let (workspace, build, toolchain) = (workspace(), build(), toolchain());
        let context = RunContext {
            workspace: &workspace,
            build: &build,
            toolchain: &toolchain,
            inherited_library_path: inherited
                .then(|| env::join_paths(["/x", "/y"]).expect("joinable")),
        };
        let package = workspace.package(ID).expect("listed");

        let vars = binary_env(&binary(kind), package, &context).expect("an environment");

        assert_eq!(get(&vars, "CARGO_BIN_EXE_tool").is_some(), kind.spawns_package_bins());
        assert_eq!(get(&vars, "CARGO"), Some(OsStr::new("/c/cargo")));
        assert_eq!(get(&vars, "CARGO_PKG_NAME"), Some(OsStr::new("a")));
        let library_path: Vec<PathBuf> =
            env::split_paths(get(&vars, LIBRARY_PATH_VAR).expect("set")).collect();
        let mut expected: Vec<PathBuf> = [
            "/ws/target/debug/build/x/out",
            "/ws/target/debug",
            "/cache/build/debug/deps",
            "/sys/lib/rustlib/h/lib",
        ]
        .map(PathBuf::from)
        .into();
        if inherited {
            expected.extend(["/x", "/y"].map(PathBuf::from));
        } else {
            expected.extend(default_library_path());
        }
        assert_eq!(library_path, expected);
    }

    // Property: the direct command runs the executable from the package root with the test
    // arguments.
    #[hegel::test]
    fn direct_commands_run_from_the_package_root(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        let test_args: Vec<OsString> =
            tc.draw(gs::vecs(gs::text().max_size(6)).max_size(3)).into_iter().map(Into::into).collect();
        let (workspace, build, toolchain) = (workspace(), build(), toolchain());
        let context = RunContext {
            workspace: &workspace,
            build: &build,
            toolchain: &toolchain,
            inherited_library_path: None,
        };

        let command = direct_command(&binary(kind), &context, &test_args).expect("a command");

        assert_eq!(command.get_program(), binary(kind).executable.as_os_str());
        assert_eq!(command.get_current_dir(), Some(Path::new("/ws/crates/a")));
        assert_eq!(command.get_args().collect::<Vec<_>>(), test_args.iter().map(OsString::as_os_str).collect::<Vec<_>>());
    }

    // Property: the via-cargo command selects exactly the binary's target, keeps the other
    // cargo arguments, and passes the test arguments after `--`.
    #[hegel::test]
    fn via_cargo_commands_select_one_target(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        let test_args: Vec<OsString> =
            tc.draw(gs::vecs(gs::text().max_size(6)).max_size(3)).into_iter().map(Into::into).collect();
        let cargo_args = CargoArgs::new(
            ["--release", "-p", "other", "--test", "x", "--features", "f"].map(OsString::from).into(),
        );

        let command = via_cargo_command(Path::new("cargo"), &binary(kind), &cargo_args, &test_args);

        let args: Vec<&OsStr> = command.get_args().collect();
        let selector: &[&str] = match kind {
            TargetKind::Lib => &["--lib"],
            TargetKind::Bin => &["--bin", "t"],
            TargetKind::Test => &["--test", "t"],
            TargetKind::Bench => &["--bench", "t"],
            TargetKind::Example => &["--example", "t"],
        };
        let mut expected: Vec<&OsStr> = ["test", "--release", "--features", "f", "--package", ID]
            .iter()
            .chain(selector)
            .map(OsStr::new)
            .collect();
        if !test_args.is_empty() {
            expected.push(OsStr::new("--"));
            expected.extend(test_args.iter().map(OsString::as_os_str));
        }
        assert_eq!(args, expected);
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core exec::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! Running a test binary as `cargo test` would, or through Cargo itself (design 4.4).
//!
//! `cargo test` runs a test binary from its package's directory, with the package's
//! `CARGO_PKG_*` variables, `CARGO_BIN_EXE_<name>` for integration tests and benchmarks, and a
//! dynamic library path that finds the build's and the toolchain's shared libraries. The direct
//! runner rebuilds exactly that; the conformance test in
//! `crates/cargo-testcache/tests/fixture.rs` compares it with Cargo's. The via-cargo runner lets
//! Cargo do it, at the cost of Cargo's freshness check for every binary it runs.
//!
//! Test output is inherited, not captured: it reaches the terminal exactly as under
//! `cargo test`, colours included.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::args::CargoArgs;
use crate::discover::{Build, TargetKind, TestBinary};
use crate::error::{Error, Result};
use crate::workspace::{Package, Workspace};

/// The variable the dynamic loader searches, as Cargo sets it on each platform.
#[cfg(target_os = "macos")]
pub const LIBRARY_PATH_VAR: &str = "DYLD_FALLBACK_LIBRARY_PATH";
/// The variable the dynamic loader searches, as Cargo sets it on each platform.
#[cfg(windows)]
pub const LIBRARY_PATH_VAR: &str = "PATH";
/// The variable the dynamic loader searches, as Cargo sets it on each platform.
#[cfg(not(any(target_os = "macos", windows)))]
pub const LIBRARY_PATH_VAR: &str = "LD_LIBRARY_PATH";

/// What the direct runner needs to know about the Rust toolchain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    sysroot: PathBuf,
    host: String,
    cargo: PathBuf,
}

impl Toolchain {
    /// A toolchain given directly.
    ///
    /// # Arguments
    ///
    /// * `sysroot` - `rustc --print sysroot`.
    /// * `host` - `rustc --print host-tuple`.
    /// * `cargo` - The value test binaries get as `CARGO`.
    ///
    /// # Returns
    ///
    /// The toolchain.
    pub fn new(sysroot: PathBuf, host: String, cargo: PathBuf) -> Self {
        Self { sysroot, host, cargo }
    }

    /// Asks `rustc` (or `$RUSTC`) for the sysroot and host, from the workspace root so that a
    /// `rust-toolchain.toml` there applies.
    ///
    /// # Arguments
    ///
    /// * `cargo` - The cargo this tool runs. A bare `cargo` (no `$CARGO` was set) is replaced by
    ///   the toolchain's own, the absolute path `cargo test` would give tests.
    /// * `workspace_root` - Where to run `rustc`.
    ///
    /// # Returns
    ///
    /// The toolchain.
    ///
    /// # Errors
    ///
    /// When `rustc` cannot run, fails, or prints something unexpected.
    pub fn detect(cargo: &Path, workspace_root: &Path) -> Result<Self> {
        const COMMAND: &str = "rustc --print sysroot --print host-tuple";
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let output = Command::new(rustc)
            .args(["--print", "sysroot", "--print", "host-tuple"])
            .current_dir(workspace_root)
            .stderr(Stdio::inherit())
            .output()
            .map_err(Error::io(format!("running {COMMAND}")))?;
        if !output.status.success() {
            return Err(Error::CommandFailed { command: COMMAND.to_owned(), status: output.status });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let (Some(sysroot), Some(host)) = (lines.next(), lines.next()) else {
            return Err(Error::UnexpectedOutput {
                command: COMMAND.to_owned(),
                output: text.into_owned(),
            });
        };
        let sysroot = PathBuf::from(sysroot);
        let bundled = sysroot.join("bin").join(format!("cargo{}", env::consts::EXE_SUFFIX));
        let cargo = if cargo.is_absolute() || !bundled.is_file() {
            cargo.to_path_buf()
        } else {
            bundled
        };
        Ok(Self::new(sysroot, host.to_owned(), cargo))
    }

    /// `<sysroot>/lib/rustlib/<host>/lib`, where the standard library's dynamic libraries live.
    fn host_libdir(&self) -> PathBuf {
        self.sysroot.join("lib").join("rustlib").join(&self.host).join("lib")
    }
}

/// Everything the direct runner needs besides the binary.
#[derive(Debug, Clone)]
pub struct RunContext<'a> {
    /// The workspace.
    pub workspace: &'a Workspace,
    /// What was built (package bins, build scripts' library directories).
    pub build: &'a Build,
    /// The toolchain.
    pub toolchain: &'a Toolchain,
    /// [`LIBRARY_PATH_VAR`] as this process got it; appended after the build's directories.
    pub inherited_library_path: Option<OsString>,
}

/// How a binary's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOutcome {
    /// Its exit status.
    pub status: ExitStatus,
    /// How long it ran.
    pub duration: Duration,
}

impl RunOutcome {
    /// Whether it exited 0.
    pub fn passed(&self) -> bool {
        self.status.success()
    }
}

/// The variables `cargo test` adds to a test binary's environment.
///
/// # Arguments
///
/// * `binary` - The test binary.
/// * `package` - Its package.
/// * `context` - The build and toolchain.
///
/// # Returns
///
/// Name and value pairs, to set on top of this process's environment.
///
/// # Errors
///
/// [`Error::LibraryPath`] when a directory contains the path-list separator.
pub fn binary_env(
    binary: &TestBinary,
    package: &Package,
    context: &RunContext<'_>,
) -> Result<Vec<(OsString, OsString)>> {
    let mut vars: Vec<(OsString, OsString)> =
        package.env().iter().map(|(name, value)| (OsString::from(name), value.clone())).collect();
    vars.push(("CARGO".into(), context.toolchain.cargo.clone().into()));
    if binary.id.kind.spawns_package_bins()
        && let Some(bins) = context.build.package_bins.get(&binary.id.package_id)
    {
        vars.extend(
            bins.iter().map(|(name, path)| (format!("CARGO_BIN_EXE_{name}").into(), path.into())),
        );
    }
    vars.push((LIBRARY_PATH_VAR.into(), library_path(binary, context)?));
    Ok(vars)
}

/// The dynamic library path `cargo test` gives a binary.
fn library_path(binary: &TestBinary, context: &RunContext<'_>) -> Result<OsString> {
    let workspace = context.workspace;
    let in_target = |path: &&PathBuf| {
        path.starts_with(workspace.target_dir()) || path.starts_with(workspace.build_dir())
    };
    let mut dirs: Vec<PathBuf> =
        context.build.linked_paths.iter().filter(in_target).cloned().collect();
    dirs.extend(workspace.artifact_dir(&binary.executable));
    dirs.extend(binary.executable.parent().map(Path::to_path_buf));
    dirs.push(context.toolchain.host_libdir());
    match &context.inherited_library_path {
        Some(inherited) => dirs.extend(env::split_paths(inherited)),
        None => dirs.extend(default_library_path()),
    }
    Ok(env::join_paths(dirs)?)
}

/// What Cargo appends when the library path variable was unset: on macOS, the loader's own
/// fallback directories; nothing elsewhere.
fn default_library_path() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let home_lib = env::var_os("HOME").map(|home| PathBuf::from(home).join("lib"));
    home_lib.into_iter().chain(["/usr/local/lib", "/usr/lib"].map(PathBuf::from)).collect()
}

/// The command that runs a test binary directly, as `cargo test` would.
///
/// # Arguments
///
/// * `binary` - The test binary.
/// * `context` - The workspace, build and toolchain.
/// * `test_args` - The arguments after `--`.
///
/// # Returns
///
/// The command, ready for [`run`].
///
/// # Errors
///
/// [`Error::UnknownPackage`] when the binary's package is not in the metadata, or
/// [`Error::LibraryPath`].
pub fn direct_command(
    binary: &TestBinary,
    context: &RunContext<'_>,
    test_args: &[OsString],
) -> Result<Command> {
    let package = context
        .workspace
        .package(&binary.id.package_id)
        .ok_or_else(|| Error::UnknownPackage { id: binary.id.package_id.clone() })?;
    let mut command = Command::new(&binary.executable);
    command
        .args(test_args)
        .current_dir(package.root())
        .envs(binary_env(binary, package, context)?);
    Ok(command)
}

/// The command that runs one test binary through `cargo test`.
///
/// # Arguments
///
/// * `cargo` - The cargo executable.
/// * `binary` - The test binary.
/// * `cargo_args` - The user's cargo arguments; package and target selection are replaced.
/// * `test_args` - The arguments after `--`.
///
/// # Returns
///
/// `cargo test <args> --package <id> <target selector> -- <test args>`.
pub fn via_cargo_command(
    cargo: &Path,
    binary: &TestBinary,
    cargo_args: &CargoArgs,
    test_args: &[OsString],
) -> Command {
    let mut command = Command::new(cargo);
    command
        .arg("test")
        .args(cargo_args.without_selection().as_slice())
        .arg("--package")
        .arg(&binary.id.package_id);
    let name = binary.id.name.as_str();
    match binary.id.kind {
        TargetKind::Lib => command.arg("--lib"),
        TargetKind::Bin => command.args(["--bin", name]),
        TargetKind::Test => command.args(["--test", name]),
        TargetKind::Bench => command.args(["--bench", name]),
        TargetKind::Example => command.args(["--example", name]),
    };
    if !test_args.is_empty() {
        command.arg("--").args(test_args);
    }
    command
}

/// The command for a plain `cargo test`.
///
/// # Arguments
///
/// * `cargo` - The cargo executable.
/// * `cargo_args` - The user's cargo arguments.
/// * `extra` - Arguments placed before them, such as `--doc`.
/// * `test_args` - The arguments after `--`.
///
/// # Returns
///
/// `cargo test <extra> <args> -- <test args>`.
pub fn cargo_test_command(
    cargo: &Path,
    cargo_args: &CargoArgs,
    extra: &[&str],
    test_args: &[OsString],
) -> Command {
    let mut command = Command::new(cargo);
    command.arg("test").args(extra).args(cargo_args.as_slice());
    if !test_args.is_empty() {
        command.arg("--").args(test_args);
    }
    command
}

/// Runs a command to completion with inherited stdio.
///
/// # Arguments
///
/// * `command` - The command.
///
/// # Returns
///
/// Its exit status and how long it took.
///
/// # Errors
///
/// When it cannot be started.
pub fn run(command: &mut Command) -> Result<RunOutcome> {
    let started = Instant::now();
    let status = command.status().map_err(Error::io(format!(
        "running {}",
        command.get_program().to_string_lossy()
    )))?;
    Ok(RunOutcome { status, duration: started.elapsed() })
}
```

Add `pub mod exec;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core exec::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/exec.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(exec): direct and via-cargo runners with cargo test's environment"
```

### Task 11: `report`: status lines, progress and the summary (design 6)

**Files:**
- Create: `crates/cargo-testcache-core/src/report.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod report;`)

- [ ] **Step 1: Write the tests**

Create `report.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;

    const CATEGORIES: [ReasonCategory; 5] = [
        ReasonCategory::Changed,
        ReasonCategory::NeverPassed,
        ReasonCategory::AlwaysRun,
        ReasonCategory::NoCache,
        ReasonCategory::Unavailable,
    ];

    // Property: the summary line states the counts, lists reasons only when something ran, and
    // mentions failures only when there were some.
    #[hegel::test]
    fn the_summary_line_states_the_counts(tc: TestCase) {
        let events = tc.draw(gs::vecs(gs::optional(gs::tuples!(
            gs::sampled_from(&CATEGORIES).print_as_debug(),
            gs::booleans()
        ))).max_size(8));
        let mut tally = Tally::new(events.len());
        for event in &events {
            match event {
                None => tally.skipped(),
                Some((category, passed)) => tally.ran(*category, *passed),
            }
        }

        let line = tally.to_string();

        let run = events.iter().flatten().count();
        let failed = events.iter().flatten().filter(|(_, passed)| !passed).count();
        let noun = if events.len() == 1 { "binary" } else { "binaries" };
        assert!(line.starts_with(&format!("testcache: {} {noun}, {run} run", events.len())), "{line}");
        assert!(line.contains(&format!("{} skipped", events.len() - run)), "{line}");
        assert_eq!(line.contains('('), run > 0, "{line}");
        assert_eq!(line.contains("failed"), failed > 0, "{line}");
        assert_eq!(tally.any_failed(), failed > 0);
    }

    // Property: ages use the largest whole unit.
    #[hegel::test]
    fn ages_use_the_largest_whole_unit(tc: TestCase) {
        let seconds = tc.draw(gs::integers::<u64>().max_value(10_000_000));

        let age = format_age(Duration::from_secs(seconds));

        let expected = match seconds {
            0..60 => format!("{seconds}s"),
            60..3_600 => format!("{}m", seconds / 60),
            3_600..86_400 => format!("{}h", seconds / 3_600),
            _ => format!("{}d", seconds / 86_400),
        };
        assert_eq!(age, expected);
    }

    // Property: unit tests are labelled `unittests <src>`, other targets by their source, and
    // every label names the package and target.
    #[hegel::test]
    fn labels_look_like_cargos(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&[TargetKind::Lib, TargetKind::Bin,
            TargetKind::Test, TargetKind::Bench, TargetKind::Example]).print_as_debug());
        let binary = TestBinary {
            id: TargetId { package_id: "id".to_owned(), kind, name: "t".to_owned() },
            src_path: PathBuf::from("/ws/a/src/lib.rs"),
            executable: PathBuf::from("/ws/target/debug/deps/t-1"),
        };

        let label = label(&binary, "a", Path::new("/ws/a"));

        let unit = matches!(kind, TargetKind::Lib | TargetKind::Bin);
        assert_eq!(label.starts_with("unittests "), unit, "{label}");
        assert!(label.contains(&format!("src{}lib.rs", std::path::MAIN_SEPARATOR)), "{label}");
        assert!(label.ends_with("(a/t)"), "{label}");
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p cargo-testcache-core report::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! What a run prints on stderr (stdout is the tests' own output): a line per binary in Cargo's
//! style, a progress bar while hashing, and the closing summary line.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

use crate::discover::{TargetKind, TestBinary};
use crate::plan::ReasonCategory;

/// Counts for the summary line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    binaries: usize,
    skipped: usize,
    failed: usize,
    run: BTreeMap<ReasonCategory, usize>,
    /// A whole-workspace `cargo test` (the via-cargo shortcut) failed; which binary is unknown.
    whole_run_failed: bool,
    doc_tests_failed: bool,
}

impl Tally {
    /// An empty tally for this many binaries.
    pub fn new(binaries: usize) -> Self {
        Self { binaries, ..Self::default() }
    }

    /// Counts a skipped binary.
    pub fn skipped(&mut self) {
        self.skipped += 1;
    }

    /// Counts a binary that ran.
    ///
    /// # Arguments
    ///
    /// * `category` - Why it ran.
    /// * `passed` - Whether it passed.
    pub fn ran(&mut self, category: ReasonCategory, passed: bool) {
        *self.run.entry(category).or_default() += 1;
        if !passed {
            self.failed += 1;
        }
    }

    /// Counts binaries that ran together in one `cargo test`.
    ///
    /// # Arguments
    ///
    /// * `categories` - Why each ran.
    /// * `passed` - Whether the whole run passed.
    pub fn ran_together(
        &mut self,
        categories: impl IntoIterator<Item = ReasonCategory>,
        passed: bool,
    ) {
        for category in categories {
            *self.run.entry(category).or_default() += 1;
        }
        self.whole_run_failed |= !passed;
    }

    /// Records the doc tests' result.
    pub fn doc_tests(&mut self, passed: bool) {
        self.doc_tests_failed |= !passed;
    }

    /// Whether anything failed.
    pub fn any_failed(&self) -> bool {
        self.failed > 0 || self.whole_run_failed || self.doc_tests_failed
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let noun = if self.binaries == 1 { "binary" } else { "binaries" };
        let run: usize = self.run.values().sum();
        write!(formatter, "testcache: {} {noun}, {run} run", self.binaries)?;
        if run > 0 {
            let reasons: Vec<String> = self
                .run
                .iter()
                .map(|(category, count)| format!("{count} {}", category.label()))
                .collect();
            write!(formatter, " ({})", reasons.join(", "))?;
        }
        write!(formatter, ", {} skipped", self.skipped)?;
        if self.failed > 0 {
            write!(formatter, "; {} failed", self.failed)?;
        }
        if self.whole_run_failed {
            formatter.write_str("; cargo test failed")?;
        }
        if self.doc_tests_failed {
            formatter.write_str("; doc tests failed")?;
        }
        Ok(())
    }
}

/// A binary's label, as Cargo prints it after "Running": `unittests src/lib.rs (pkg/target)`.
///
/// # Arguments
///
/// * `binary` - The binary.
/// * `package_name` - Its package's name.
/// * `package_root` - Its package's directory, to shorten the source path.
///
/// # Returns
///
/// The label.
pub fn label(binary: &TestBinary, package_name: &str, package_root: &Path) -> String {
    let source = binary.src_path.strip_prefix(package_root).unwrap_or(&binary.src_path);
    let prefix = match binary.id.kind {
        TargetKind::Lib | TargetKind::Bin => "unittests ",
        TargetKind::Test | TargetKind::Bench | TargetKind::Example => "",
    };
    format!("{prefix}{} ({package_name}/{})", source.display(), binary.id.name)
}

/// One status line: a right-aligned verb as Cargo prints them, the label, and a detail.
///
/// # Arguments
///
/// * `verb` - Such as "Skipped".
/// * `label` - From [`label`].
/// * `detail` - Such as the reason it runs.
///
/// # Returns
///
/// The line.
pub fn status_line(verb: &str, label: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{verb:>12} {label}: {detail}"),
        None => format!("{verb:>12} {label}"),
    }
}

/// An age in its largest whole unit: `42s`, `5m`, `3h`, `2d`.
pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// A progress bar on stderr. indicatif hides it when stderr is not a terminal, so piped output
/// gets only the plain lines printed through [`Progress::line`].
#[derive(Debug, Clone)]
pub struct Progress(ProgressBar);

impl Progress {
    /// Starts a bar.
    ///
    /// # Arguments
    ///
    /// * `length` - How many steps.
    /// * `message` - What is being done, such as "hashing test binaries".
    ///
    /// # Returns
    ///
    /// The bar.
    pub fn new(length: usize, message: impl Into<Cow<'static, str>>) -> Self {
        let bar = ProgressBar::new(length as u64).with_message(message);
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg} [{pos}/{len}] {elapsed}")
                .expect("the progress template is valid"),
        );
        Self(bar)
    }

    /// Prints a line above the bar.
    pub fn line(&self, text: &str) {
        self.0.suspend(|| eprintln!("{text}"));
    }

    /// Runs `work` with the bar cleared, so a test binary's output is not drawn over.
    pub fn suspended<R>(&self, work: impl FnOnce() -> R) -> R {
        self.0.suspend(work)
    }

    /// Advances the bar by one step. Safe to call from several threads.
    pub fn inc(&self) {
        self.0.inc(1);
    }

    /// Removes the bar.
    pub fn finish(&self) {
        self.0.finish_and_clear();
    }
}
```

Add `pub mod report;` to `lib.rs`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p cargo-testcache-core report::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/report.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(report): status lines, progress bar and summary line"
```

### Task 12: `app`: the whole run (design 4.3)

**Files:**
- Create: `crates/cargo-testcache-core/src/app.rs`
- Modify: `crates/cargo-testcache-core/src/lib.rs` (add `pub mod app;`)

`app` wires the modules together and spawns processes; its behaviour is tested end to end by
task 14's fixture tests. Its one pure helper, the run order, gets a property test here.

- [ ] **Step 1: Write the test**

Create `app.rs` with Step 3's items as `todo!()` stubs, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;
    use crate::workspace::testing::{ID, metadata};

    // Property: binaries run in Cargo's order, by kind then name within a package, and a binary
    // of a package missing from the metadata is an error.
    #[hegel::test]
    fn binaries_run_in_cargos_order(tc: TestCase) {
        let targets = tc.draw(gs::vecs(gs::tuples!(
            gs::sampled_from(&[TargetKind::Example, TargetKind::Test, TargetKind::Lib,
                TargetKind::Bin]).print_as_debug(),
            gs::text().alphabet("ab").min_size(1).max_size(2)
        )).max_size(6));
        let unknown = tc.draw(gs::booleans());
        let workspace = Workspace::from_json(&metadata("0.1.0", None)).expect("valid metadata");
        let mut build = Build::default();
        for (kind, name) in &targets {
            build.binaries.push(TestBinary {
                id: TargetId { package_id: ID.to_owned(), kind: *kind, name: name.clone() },
                src_path: PathBuf::from("/ws/crates/a/src/lib.rs"),
                executable: PathBuf::from(format!("/ws/target/debug/deps/{name}-1")),
            });
        }
        if unknown {
            let mut stray = build.binaries.first().cloned().unwrap_or_else(|| TestBinary {
                id: TargetId { package_id: String::new(), kind: TargetKind::Lib, name: "x".into() },
                src_path: PathBuf::new(),
                executable: PathBuf::new(),
            });
            stray.id.package_id = "elsewhere".to_owned();
            build.binaries.push(stray);
        }

        let ordered = ordered_binaries(&workspace, &build);

        if unknown {
            assert!(matches!(ordered, Err(Error::UnknownPackage { .. })));
            return;
        }
        let order: Vec<(TargetKind, &str)> = ordered
            .expect("every package is known")
            .iter()
            .map(|(binary, _)| (binary.id.kind, binary.id.name.as_str()))
            .collect();
        let mut expected: Vec<(TargetKind, &str)> =
            targets.iter().map(|(kind, name)| (*kind, name.as_str())).collect();
        expected.sort();
        assert_eq!(order, expected);
    }
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p cargo-testcache-core app::tests`
Expected: FAIL with "not yet implemented".

- [ ] **Step 3: Implement**

```rust
//! One `cargo testcache` run: build, hash, decide, run, record, report (design section 4).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use crate::args::CargoArgs;
use crate::discover::{self, Build, TargetKind, TestBinary};
use crate::error::{Error, Result};
use crate::exec::{self, LIBRARY_PATH_VAR, RunContext, Toolchain};
use crate::inputs::InputIndex;
use crate::key::{self, Digest, KeyParts};
use crate::plan::{self, Decision, Policy};
use crate::report::{self, Progress, Tally};
use crate::store::Store;
use crate::workspace::{Package, Workspace};

/// The exit status `cargo test` uses for failures, used when a process died without one.
const FAILURE_CODE: i32 = 101;

/// How `cargo testcache` was asked to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `status`: list what would run and what would be skipped, run nothing.
    pub status_only: bool,
    /// `--no-cache`: run everything, record the passes.
    pub no_cache: bool,
    /// `--explain`: print why each binary runs.
    pub explain: bool,
    /// `--via-cargo`: run each binary through `cargo test`.
    pub via_cargo: bool,
    /// `--no-doc`: skip doc tests.
    pub no_doc: bool,
    /// The arguments for `cargo test`.
    pub cargo_args: CargoArgs,
    /// The arguments after `--`.
    pub test_args: Vec<OsString>,
}

/// How the run ended, for the exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Everything that ran passed.
    Passed,
    /// A test binary or the doc tests failed.
    TestsFailed,
    /// Exit with this status: the build failed, or `cargo test` ran unchanged.
    Exited(i32),
}

/// The cargo to run: `$CARGO` (set when Cargo runs `cargo testcache`), else `cargo`.
pub fn cargo_path() -> PathBuf {
    env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// Runs `cargo testcache` (or `cargo testcache status`).
///
/// # Arguments
///
/// * `options` - What was asked for.
///
/// # Returns
///
/// How the run ended.
///
/// # Errors
///
/// When a command cannot be started, the metadata or configuration is invalid, or the store
/// cannot be used. Test failures are an [`Outcome`], not an error.
pub fn run(options: &Options) -> Result<Outcome> {
    let cargo = cargo_path();
    if let Some(flag) = options.cargo_args.unsupported_flag() {
        return Err(Error::UnsupportedArgument {
            argument: flag.to_owned(),
            reason: "cargo-testcache sets it itself",
        });
    }
    if let Some(flag) = options.cargo_args.passthrough_flag() {
        tracing::info!("{flag} given: running cargo test without the cache");
        let mut command =
            exec::cargo_test_command(&cargo, &options.cargo_args, &[], &options.test_args);
        let outcome = exec::run(&mut command)?;
        return Ok(Outcome::Exited(outcome.status.code().unwrap_or(FAILURE_CODE)));
    }
    let workspace = Workspace::load(&cargo, &options.cargo_args)?;
    let (status, build) = discover::build(&cargo, &options.cargo_args)?;
    if !status.success() {
        return Ok(Outcome::Exited(status.code().unwrap_or(FAILURE_CODE)));
    }
    let mut store = Store::open(&workspace.cache_dir())?;
    let session = Session { cargo: &cargo, workspace: &workspace, build: &build, options };
    let planned = session.plan(&store)?;
    if options.status_only {
        return Ok(report_status(&planned));
    }
    session.execute(&mut store, planned)
}

/// Forgets every recorded pass of the workspace.
///
/// # Arguments
///
/// * `cargo_args` - The cargo arguments (`--manifest-path`, `--target-dir` select the store).
///
/// # Returns
///
/// Whether there was anything to forget.
///
/// # Errors
///
/// When `cargo metadata` fails or the store cannot be cleared.
pub fn clear(cargo_args: &CargoArgs) -> Result<bool> {
    let workspace = Workspace::load(&cargo_path(), cargo_args)?;
    Store::clear(&workspace.cache_dir())
}

/// One test binary with its key and decision.
struct Planned<'a> {
    binary: &'a TestBinary,
    label: String,
    parts: std::result::Result<KeyParts, String>,
    decision: Decision,
}

/// What every step of a run shares.
struct Session<'a> {
    cargo: &'a Path,
    workspace: &'a Workspace,
    build: &'a Build,
    options: &'a Options,
}

/// What computing a key needs besides the binary.
struct KeyContext<'a> {
    workspace: &'a Workspace,
    inputs: &'a InputIndex,
    digests: &'a HashMap<PathBuf, std::result::Result<Digest, String>>,
    environment: &'a BTreeMap<OsString, OsString>,
    tool: String,
    args: Digest,
}

impl Session<'_> {
    /// Hashes everything and decides, for each binary in run order, whether it runs.
    fn plan(&self, store: &Store) -> Result<Vec<Planned<'_>>> {
        let binaries = ordered_binaries(self.workspace, self.build)?;
        let spawned: Vec<_> = binaries
            .iter()
            .map(|(binary, package)| spawned_paths(self.workspace, self.build, binary, package))
            .collect();
        // `.iter().flatten()` over `Result`s yields only the `Ok` values.
        let to_hash = binaries
            .iter()
            .map(|(binary, _)| binary.executable.as_path())
            .chain(spawned.iter().flatten().flatten().map(|(_, path)| path.as_path()));
        let digests = hash_files(to_hash);
        let inputs = InputIndex::collect(self.workspace)?;
        let environment: BTreeMap<OsString, OsString> = env::vars_os().collect();
        let always_run = self.workspace.always_run()?;
        let context = KeyContext {
            workspace: self.workspace,
            inputs: &inputs,
            digests: &digests,
            environment: &environment,
            tool: KeyParts::tool_id(),
            args: KeyParts::args_digest(&self.options.test_args),
        };
        Ok(binaries
            .into_iter()
            .zip(spawned)
            .map(|((binary, package), spawned)| {
                let parts = key_parts(binary, package, spawned, &context);
                let policy = Policy {
                    no_cache: self.options.no_cache,
                    always_run: always_run.matches(package.name(), &binary.id.name),
                };
                Planned {
                    decision: plan::decide(&parts, store.lookup(&binary.id), policy),
                    label: report::label(binary, package.name(), package.root()),
                    binary,
                    parts,
                }
            })
            .collect())
    }

    /// Runs what has to run, records the passes, runs doc tests and prints the summary.
    fn execute(&self, store: &mut Store, planned: Vec<Planned<'_>>) -> Result<Outcome> {
        let options = self.options;
        let to_run = planned.iter().filter(|p| matches!(p.decision, Decision::Run(_))).count();
        if options.via_cargo && to_run == planned.len() && to_run > 0 && !options.no_doc {
            return self.run_whole(store, planned);
        }
        let toolchain = if options.via_cargo || to_run == 0 {
            None
        } else {
            Some(Toolchain::detect(self.cargo, self.workspace.root())?)
        };
        let context = toolchain.as_ref().map(|toolchain| RunContext {
            workspace: self.workspace,
            build: self.build,
            toolchain,
            inherited_library_path: env::var_os(LIBRARY_PATH_VAR),
        });
        let fail_fast = !options.cargo_args.has("--no-fail-fast");
        let mut tally = Tally::new(planned.len());
        let progress = Progress::new(planned.len(), "test binaries");
        for planned in planned {
            match planned.decision {
                Decision::Skip { passed_at } => {
                    let age = report::format_age(Duration::from_secs(unix_now().saturating_sub(passed_at)));
                    progress.line(&report::status_line(
                        "Skipped",
                        &planned.label,
                        Some(&format!("passed {age} ago")),
                    ));
                    tally.skipped();
                }
                Decision::Run(reason) => {
                    let detail = options.explain.then(|| reason.to_string());
                    progress.line(&report::status_line("Running", &planned.label, detail.as_deref()));
                    let mut command = match &context {
                        Some(context) => {
                            exec::direct_command(planned.binary, context, &options.test_args)?
                        }
                        None => exec::via_cargo_command(
                            self.cargo,
                            planned.binary,
                            &options.cargo_args,
                            &options.test_args,
                        ),
                    };
                    let outcome = progress.suspended(|| exec::run(&mut command))?;
                    let passed = outcome.passed();
                    tally.ran(reason.category(), passed);
                    if let Ok(parts) = planned.parts
                        && let Some(record) = plan::pass_record(
                            &planned.binary.id,
                            parts,
                            passed,
                            outcome.duration,
                            unix_now(),
                        )
                    {
                        store.record(record)?;
                    }
                    if !passed && fail_fast {
                        break;
                    }
                }
            }
            progress.inc();
        }
        progress.finish();
        self.run_doc_tests(&mut tally, fail_fast)?;
        eprintln!("{tally}");
        Ok(if tally.any_failed() { Outcome::TestsFailed } else { Outcome::Passed })
    }

    /// The via-cargo shortcut when nothing is cached: one plain `cargo test` (doc tests
    /// included), recording every binary if it passes.
    fn run_whole(&self, store: &mut Store, planned: Vec<Planned<'_>>) -> Result<Outcome> {
        let options = self.options;
        eprintln!("testcache: nothing is cached; running cargo test once");
        let mut command =
            exec::cargo_test_command(self.cargo, &options.cargo_args, &[], &options.test_args);
        let outcome = exec::run(&mut command)?;
        let passed = outcome.passed();
        let mut tally = Tally::new(planned.len());
        tally.ran_together(
            planned.iter().filter_map(|p| match &p.decision {
                Decision::Run(reason) => Some(reason.category()),
                Decision::Skip { .. } => None,
            }),
            passed,
        );
        let now = unix_now();
        for planned in planned {
            if let Ok(parts) = planned.parts
                && let Some(record) =
                    plan::pass_record(&planned.binary.id, parts, passed, outcome.duration, now)
            {
                store.record(record)?;
            }
        }
        eprintln!("{tally}");
        Ok(if passed { Outcome::Passed } else { Outcome::TestsFailed })
    }

    /// Runs `cargo test --doc` as `cargo test` would have: unless `--no-doc`, a target
    /// selection, no library, or an earlier failure under fail-fast rules it out.
    fn run_doc_tests(&self, tally: &mut Tally, fail_fast: bool) -> Result<()> {
        let options = self.options;
        let has_lib = self.build.binaries.iter().any(|binary| binary.id.kind == TargetKind::Lib);
        if options.no_doc
            || options.cargo_args.selects_targets()
            || !has_lib
            || (fail_fast && tally.any_failed())
        {
            return Ok(());
        }
        eprintln!("{}", report::status_line("Doc-tests", "not cached", None));
        let mut command =
            exec::cargo_test_command(self.cargo, &options.cargo_args, &["--doc"], &options.test_args);
        tally.doc_tests(exec::run(&mut command)?.passed());
        Ok(())
    }
}

/// Prints what `status` found.
fn report_status(planned: &[Planned<'_>]) -> Outcome {
    let mut tally = Tally::new(planned.len());
    let now = unix_now();
    for planned in planned {
        match &planned.decision {
            Decision::Skip { passed_at } => {
                let age = report::format_age(Duration::from_secs(now.saturating_sub(*passed_at)));
                let detail = format!("passed {age} ago");
                eprintln!("{}", report::status_line("Would skip", &planned.label, Some(&detail)));
                tally.skipped();
            }
            Decision::Run(reason) => {
                let detail = reason.to_string();
                eprintln!("{}", report::status_line("Would run", &planned.label, Some(&detail)));
                tally.ran(reason.category(), true);
            }
        }
    }
    eprintln!("{tally}");
    Outcome::Passed
}

/// The test binaries with their packages, in the order `cargo test` runs them.
fn ordered_binaries<'a>(
    workspace: &'a Workspace,
    build: &'a Build,
) -> Result<Vec<(&'a TestBinary, &'a Package)>> {
    let mut binaries = build
        .binaries
        .iter()
        .map(|binary| {
            workspace
                .package(&binary.id.package_id)
                .map(|package| (binary, package))
                .ok_or_else(|| Error::UnknownPackage { id: binary.id.package_id.clone() })
        })
        .collect::<Result<Vec<_>>>()?;
    binaries.sort_by(|(a, a_package), (b, b_package)| {
        (a_package.name(), a.id.kind, &a.id.name).cmp(&(b_package.name(), b.id.kind, &b.id.name))
    });
    Ok(binaries)
}

/// The binaries a test may spawn, by their name in the key: its package's bins for an
/// integration test or benchmark, and the configured `spawns`.
///
/// # Returns
///
/// The names and paths, or why they cannot be known.
fn spawned_paths(
    workspace: &Workspace,
    build: &Build,
    binary: &TestBinary,
    package: &Package,
) -> std::result::Result<Vec<(String, PathBuf)>, String> {
    let mut paths = Vec::new();
    if binary.id.kind.spawns_package_bins()
        && let Some(bins) = build.package_bins.get(&binary.id.package_id)
    {
        paths.extend(bins.iter().map(|(name, path)| (format!("bin {name}"), path.clone())));
    }
    let mut spawns = workspace.spawns(package).peekable();
    if spawns.peek().is_some() {
        let dir = workspace.artifact_dir(&binary.executable).ok_or_else(|| {
            format!("{} is not under the build directory", binary.executable.display())
        })?;
        paths.extend(spawns.map(|spawn| (format!("spawns {spawn}"), dir.join(spawn))));
    }
    Ok(paths)
}

/// Hashes every distinct file once, in parallel, with a progress bar.
fn hash_files<'a>(
    paths: impl Iterator<Item = &'a Path>,
) -> HashMap<PathBuf, std::result::Result<Digest, String>> {
    let unique: HashSet<&Path> = paths.collect();
    let progress = Progress::new(unique.len(), "hashing test binaries");
    let digests = unique
        .into_par_iter()
        .map(|path| {
            let digest = Digest::of_file(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()));
            progress.inc();
            (path.to_path_buf(), digest)
        })
        .collect();
    progress.finish();
    digests
}

/// A binary's key parts, or why one of them cannot be computed.
fn key_parts(
    binary: &TestBinary,
    package: &Package,
    spawned: std::result::Result<Vec<(String, PathBuf)>, String>,
    context: &KeyContext<'_>,
) -> std::result::Result<KeyParts, String> {
    let digest = |path: &Path| {
        context
            .digests
            .get(path)
            .cloned()
            .unwrap_or_else(|| Err(format!("{} was not hashed", path.display())))
    };
    Ok(KeyParts {
        tool: context.tool.clone(),
        binary: digest(&binary.executable)?,
        spawned: spawned?
            .into_iter()
            .map(|(name, path)| Ok((name, digest(&path)?)))
            .collect::<std::result::Result<_, String>>()?,
        inputs: context.inputs.for_package(&binary.id.package_id)?,
        env: key::env_part(context.workspace.env_names(package), context.environment),
        args: context.args,
    })
}

/// Now, in seconds since the Unix epoch (0 if the clock is before it).
fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}
```

Add `pub mod app;` to `lib.rs`.

- [ ] **Step 4: Run the test to see it pass**

Run: `cargo test -p cargo-testcache-core app::tests`
Expected: PASS.

- [ ] **Step 5: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache-core/src/app.rs crates/cargo-testcache-core/src/lib.rs
git commit -m "feat(app): build, hash, decide, run, record and report"
```

### Task 13: the binary: command line, logging, exit status (design 6)

**Files:**
- Modify: `crates/cargo-testcache/src/main.rs` (replace)

- [ ] **Step 1: Write the test**

The test goes at the bottom of `main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    // Property: every option clap defines is recognized wherever it sits before `--`, the
    // rest goes to cargo, and clap then parses what was recognized.
    #[hegel::test]
    fn own_options_are_recognized_among_cargo_arguments(tc: TestCase) {
        let own = tc.draw(gs::subsequences(
            &["--no-cache", "--explain", "--via-cargo", "--no-doc", "--log-format=json"][..],
        ));
        let cargo = ["-p", "alpha", "--release"];
        let status = tc.draw(gs::booleans());
        let mut argv: Vec<OsString> = vec!["testcache".into()];
        if status {
            argv.push("status".into());
        }
        argv.extend(cargo[..2].iter().map(OsString::from));
        argv.extend(own.iter().map(OsString::from));
        argv.push(cargo[2].into());
        argv.extend(["--", "--explain"].map(OsString::from));

        let split = split_command_line(argv);
        let cli = Cli::try_parse_from(
            std::iter::once(OsString::from("cargo-testcache")).chain(split.own),
        )
        .expect("the recognized options parse");

        assert_eq!(split.cargo.as_slice(), cargo.map(OsString::from).as_slice());
        assert_eq!(split.test, [OsString::from("--explain")]);
        assert_eq!(cli.explain, own.contains(&"--explain"));
        assert_eq!(cli.via_cargo, own.contains(&"--via-cargo"));
        assert_eq!(cli.log_format.is_some(), own.contains(&"--log-format=json"));
        assert_eq!(matches!(cli.mode, Some(Mode::Status)), status);
    }
}
```

If `gs::subsequences` takes its argument differently in this hegeltest version, draw one boolean
per option instead and keep the options whose boolean is true.

- [ ] **Step 2: Implement**

Replace `crates/cargo-testcache/src/main.rs` (keeping the test module from Step 1 at the end):

```rust
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
    override_usage = "cargo testcache [OPTIONS] [CARGO TEST ARGS] [-- TEST BINARY ARGS]\n       \
                      cargo testcache status [OPTIONS] [CARGO TEST ARGS]\n       \
                      cargo testcache clear"
)]
struct Cli {
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
    Status,
    /// Forget every recorded pass.
    Clear,
}

// `main` returns an `ExitCode` instead of calling `exit`, so destructors run (the store's lock
// is released) and errors are logged through tracing first.
fn main() -> ExitCode {
    let SplitArgs { own, cargo, test } = split_command_line(std::env::args_os().skip(1));
    // `--help`, `--version` and usage errors print and exit here.
    let cli = Cli::parse_from(std::iter::once(OsString::from("cargo-testcache")).chain(own));
    let (config, dotenv_warnings) = load_config();
    let config = config.map(|config| match cli.log_format {
        Some(log_format) => config.with_log_format(log_format),
        None => config,
    });
    telemetry::init(config.as_ref().map_or(LogFormat::default(), Config::log_format));
    for warning in dotenv_warnings {
        warn!("{warning}");
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
            // `{:#}` prints the whole context chain: "cargo testcache: running cargo ...".
            error!("{run_error:#}");
            ExitCode::FAILURE
        }
    }
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
    // `build` adds clap's own `--help`/`-h`, `--version`/`-V` and `help` subcommand.
    command.build();
    let options: HashMap<String, Arity> = command
        .get_arguments()
        .flat_map(|argument| {
            let arity =
                if argument.get_action().takes_values() { Arity::Value } else { Arity::Switch };
            let long = argument.get_long().map(|long| format!("--{long}"));
            let short = argument.get_short().map(|short| format!("-{short}"));
            long.into_iter().chain(short).map(move |name| (name, arity))
        })
        .collect();
    let subcommands: Vec<&str> = command.get_subcommands().map(clap::Command::get_name).collect();
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
                entry.map_err(|error| warnings.push(format!("skipping a .env line: {error}"))).ok()
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
```

Also give `crates/cargo-testcache/Cargo.toml` an `about` source: clap's `about` comes from the
`Cli` doc comment above, so nothing else is needed.

- [ ] **Step 3: Run the test and try the binary**

Run: `cargo test -p cargo-testcache --bin cargo-testcache`
Expected: PASS.

Run: `cargo run -q -p cargo-testcache -- testcache --help`
Expected: the usage block from `override_usage`, the four flags plus `--log-format`, and the
`status` and `clear` subcommands.

Run, in this repository: `cargo run -q -p cargo-testcache -- testcache -p cargo-testcache-core`
then the same again.
Expected: the first run runs every binary of the core crate and ends with
`testcache: N binaries, N run (N never passed), 0 skipped`; the second skips them all
(`0 run, N skipped`), then runs the doc tests.

- [ ] **Step 4: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache/src/main.rs
git commit -m "feat(cli): cargo testcache command line, status and clear"
```

### Task 14: the fixture workspace and end-to-end tests (design section 9, items 1 to 9)

**Files:**
- Create: `crates/cargo-testcache/tests/fixtures/ws/` (the files below)
- Create: `crates/cargo-testcache/tests/fixture.rs`

The fixture has four packages: `probe` (test support), `alpha` (a library with a unit test, an
integration test that reads two input files, and a doc test), `beta` (a binary `betatool` with a
unit test, and an integration test that spawns `betatool`), and `gamma` (a library depending on
`alpha`). Every test calls `probe::record()`, which appends the binary's name to
`$FIXTURE_RUN_LOG`, so the tests see which binaries actually ran, independently of what
`cargo testcache` prints. None of the `FIXTURE_*` control variables except `FIXTURE_ENV` is
declared, so they never change a key.

- [ ] **Step 1: Create the fixture workspace**

`crates/cargo-testcache/tests/fixtures/ws/Cargo.toml`:

```toml
# A small workspace the integration tests copy to a temporary directory and run
# `cargo testcache` against. Its own `[workspace]` table makes it a separate root, not a member
# of the cargo-testcache workspace.
[workspace]
members = ["probe", "alpha", "beta", "gamma"]
resolver = "3"

[workspace.metadata.testcache]
inputs = ["data/**"]
env = ["FIXTURE_ENV"]
```

`crates/cargo-testcache/tests/fixtures/ws/data/value.txt`:

```
0
```

`crates/cargo-testcache/tests/fixtures/ws/probe/Cargo.toml`:

```toml
[package]
name = "probe"
version = "0.1.0"
edition = "2024"
publish = false
```

`crates/cargo-testcache/tests/fixtures/ws/probe/src/lib.rs`:

```rust
//! Test support for the fixture workspace: lets each test report that it ran, dump its
//! environment, or fail on demand, through variables the cache does not key on.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Call first in every fixture test.
///
/// - `FIXTURE_RUN_LOG`: appends this binary's name and a newline.
/// - `FIXTURE_DUMP_DIR`: writes `<name>.env`, the working directory and the environment.
/// - `FIXTURE_FAIL_DIR`: panics if a file named after this binary exists there.
pub fn record() {
    let name = binary_name();
    if let Some(log) = std::env::var_os("FIXTURE_RUN_LOG") {
        let mut file =
            OpenOptions::new().create(true).append(true).open(log).expect("open the run log");
        writeln!(file, "{name}").expect("append to the run log");
    }
    if let Some(dir) = std::env::var_os("FIXTURE_DUMP_DIR") {
        let mut vars: Vec<String> = std::env::vars_os()
            .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
            .collect();
        vars.sort();
        let cwd = std::env::current_dir().expect("working directory");
        let dump = format!("cwd={}\n{}\n", cwd.display(), vars.join("\n"));
        fs::write(PathBuf::from(dir).join(format!("{name}.env")), dump).expect("write the dump");
    }
    if let Some(dir) = std::env::var_os("FIXTURE_FAIL_DIR") {
        assert!(!PathBuf::from(dir).join(&name).exists(), "{name} was told to fail");
    }
}

/// The test binary's name without Cargo's hash: `alpha` for `alpha-0123abcd`.
fn binary_name() -> String {
    let exe = std::env::current_exe().expect("current executable");
    let stem = exe.file_stem().expect("a file name").to_string_lossy().into_owned();
    stem.rsplit_once('-').map_or_else(|| stem.clone(), |(name, _)| name.to_owned())
}
```

`crates/cargo-testcache/tests/fixtures/ws/alpha/Cargo.toml` (the metadata fields exercise the
`CARGO_PKG_*` values in the conformance test):

```toml
[package]
name = "alpha"
version = "0.1.0-beta.1"
edition = "2024"
rust-version = "1.89"
authors = ["Example Author <author@example.com>", "Second Author"]
description = "Fixture library."
license-file = "LICENSE.txt"
publish = false

[dev-dependencies]
probe = { path = "../probe" }

[package.metadata.testcache]
inputs = ["fixtures/**"]
```

`crates/cargo-testcache/tests/fixtures/ws/alpha/LICENSE.txt`: `Fixture license.`
`crates/cargo-testcache/tests/fixtures/ws/alpha/README.md`: `# alpha` (Cargo picks it up as
`CARGO_PKG_README` without a `readme` key).
`crates/cargo-testcache/tests/fixtures/ws/alpha/fixtures/extra.txt`: `0`

`crates/cargo-testcache/tests/fixtures/ws/alpha/src/lib.rs`:

```rust
//! Fixture library `alpha`.

/// Changed by the tests to rebuild `alpha` and everything that depends on it.
pub const STAMP: &str = "0";

/// Returns [`STAMP`].
///
/// ```
/// assert!(!alpha::stamp().is_empty());
/// ```
pub fn stamp() -> &'static str {
    STAMP
}

#[cfg(test)]
mod tests {
    #[test]
    fn alpha_unit() {
        probe::record();
        println!("{}", super::stamp());
    }
}
```

`crates/cargo-testcache/tests/fixtures/ws/alpha/tests/reads_input.rs`:

```rust
//! Reads a workspace input and a package input, relative to the package root.

#[test]
fn reads_input() {
    probe::record();
    let value = std::fs::read_to_string("../data/value.txt").expect("read data/value.txt");
    let extra = std::fs::read_to_string("fixtures/extra.txt").expect("read fixtures/extra.txt");
    println!("{} {value} {extra}", alpha::stamp());
}
```

`crates/cargo-testcache/tests/fixtures/ws/beta/Cargo.toml`:

```toml
[package]
name = "beta"
version = "0.1.0"
edition = "2024"
publish = false

[[bin]]
name = "betatool"
path = "src/main.rs"

[dev-dependencies]
probe = { path = "../probe" }
```

`crates/cargo-testcache/tests/fixtures/ws/beta/src/main.rs`:

```rust
//! Fixture binary `betatool`.

/// Changed by the tests to rebuild `betatool` alone.
const STAMP: &str = "0";

fn main() {
    println!("betatool {STAMP}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn betatool_unit() {
        probe::record();
        println!("{}", super::STAMP);
    }
}
```

`crates/cargo-testcache/tests/fixtures/ws/beta/tests/spawns_tool.rs`:

```rust
//! Spawns the package's binary through `CARGO_BIN_EXE_betatool`.

#[test]
fn spawns_tool() {
    probe::record();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_betatool"))
        .output()
        .expect("run betatool");
    assert!(output.status.success());
}
```

`crates/cargo-testcache/tests/fixtures/ws/gamma/Cargo.toml`:

```toml
[package]
name = "gamma"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
alpha = { path = "../alpha" }

[dev-dependencies]
probe = { path = "../probe" }
```

`crates/cargo-testcache/tests/fixtures/ws/gamma/src/lib.rs`:

```rust
//! Fixture library `gamma`, which depends on `alpha`.

/// `alpha`'s stamp, twice.
pub fn twice() -> String {
    format!("{0}{0}", alpha::stamp())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gamma_unit() {
        probe::record();
        println!("{}", super::twice());
    }
}
```

Check the fixture builds and its binary names are as expected:

Run: `cd crates/cargo-testcache/tests/fixtures/ws && FIXTURE_RUN_LOG=/tmp/fixture-runs cargo test -q --target-dir /tmp/fixture-target; sort -u /tmp/fixture-runs; rm -rf /tmp/fixture-runs /tmp/fixture-target Cargo.lock; cd -`
Expected: all tests pass, and the log holds exactly `alpha`, `betatool`, `gamma`, `reads_input`,
`spawns_tool`. (If a name differs, for instance a bin's test executable uses an underscore form,
change `ALL` in Step 2 to the names the log shows.)

- [ ] **Step 2: Write the tests**

`crates/cargo-testcache/tests/fixture.rs`:

```rust
//! Runs `cargo testcache` against a copy of the fixture workspace in `tests/fixtures/ws`
//! (design section 9, items 1 to 9).
//!
//! Each test copies the fixture to a temporary directory with its own `CARGO_HOME`, target
//! directory and separate build directory, runs `cargo testcache` (and sometimes `cargo test`)
//! through `cargo` with this crate's binary first on `PATH`, and reads which test binaries ran
//! from the run log that every fixture test appends to. Each case builds the fixture, so the
//! tests draw few cases.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hegel::TestCase;
use hegel::generators::{self as gs, Generator};
use tempfile::TempDir;
use walkdir::WalkDir;

/// Every fixture test binary that records itself.
const ALL: [&str; 5] = ["alpha", "betatool", "gamma", "reads_input", "spawns_tool"];

/// Variables passed through from this process; everything else is cleared, so neither this
/// crate's own `CARGO_*` test variables nor CI's `RUSTFLAGS` reach the fixture's builds.
/// `LLVM_PROFILE_FILE` lets coverage runs count the binary's code.
const INHERITED: [&str; 7] = [
    "HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "TMPDIR",
    "LLVM_PROFILE_FILE",
    "SystemRoot",
    "USERPROFILE",
];

/// A private copy of the fixture workspace.
struct Fixture {
    /// Deleted when the fixture is dropped.
    temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("ws");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ws");
        copy_tree(&source, &root);
        fs::create_dir(temp.path().join("fail")).expect("create the fail directory");
        Self { temp, root }
    }

    fn run_log(&self) -> PathBuf {
        self.temp.path().join("runs.log")
    }

    /// `cargo <args>` in the fixture, with a controlled environment.
    fn cargo(&self, args: &[&str], extra_env: &[(&str, &OsStr)]) -> Output {
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_cargo-testcache"))
            .parent()
            .expect("the binary has a directory")
            .to_path_buf();
        let path = env::join_paths(
            std::iter::once(bin_dir).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .expect("a valid PATH");
        let mut command = Command::new("cargo");
        command.args(args).current_dir(&self.root).env_clear();
        for name in INHERITED {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("PATH", path)
            // An empty CARGO_HOME: cargo looks for subcommands in `$CARGO_HOME/bin` before
            // `PATH`, and an installed cargo-testcache there must not win.
            .env("CARGO_HOME", self.temp.path().join("cargo-home"))
            .env("CARGO_TARGET_DIR", self.temp.path().join("target"))
            .env("CARGO_BUILD_BUILD_DIR", self.temp.path().join("build"))
            .env("FIXTURE_RUN_LOG", self.run_log())
            .env("FIXTURE_FAIL_DIR", self.temp.path().join("fail"))
            .envs(extra_env.iter().copied());
        command.output().expect("run cargo")
    }

    /// `cargo testcache <args>`.
    fn testcache(&self, args: &[&str], extra_env: &[(&str, &OsStr)]) -> Output {
        let args: Vec<&str> = std::iter::once("testcache").chain(args.iter().copied()).collect();
        self.cargo(&args, extra_env)
    }

    /// `cargo testcache <args>`, which must succeed.
    fn testcache_ok(&self, args: &[&str]) -> Output {
        let output = self.testcache(args, &[]);
        assert!(output.status.success(), "cargo testcache {args:?} failed:\n{}", stderr(&output));
        output
    }

    /// The binaries that ran since the last call.
    fn take_runs(&self) -> BTreeSet<String> {
        let runs = fs::read_to_string(self.run_log()).unwrap_or_default();
        let _ = fs::remove_file(self.run_log());
        runs.lines().map(str::to_owned).collect()
    }

    fn write(&self, relative: &str, contents: &str) {
        fs::write(self.root.join(relative), contents).expect("write a fixture file");
    }

    /// Sets the `STAMP` constant in a fixture source file.
    fn set_stamp(&self, relative: &str, stamp: u32) {
        let path = self.root.join(relative);
        let source = fs::read_to_string(&path).expect("read the source");
        let updated: Vec<String> = source
            .lines()
            .map(|line| {
                if line.contains("STAMP: &str = ") {
                    let prefix = line.split('"').next().expect("a prefix");
                    format!("{prefix}\"{stamp}\";")
                } else {
                    line.to_owned()
                }
            })
            .collect();
        fs::write(&path, updated.join("\n") + "\n").expect("write the source");
    }

    fn fail(&self, name: &str, fail: bool) {
        let path = self.temp.path().join("fail").join(name);
        if fail {
            fs::write(path, "").expect("create the fail file");
        } else {
            fs::remove_file(path).expect("remove the fail file");
        }
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in WalkDir::new(from).into_iter().filter_entry(|entry| {
        entry.file_name() != "target" && entry.file_name() != "Cargo.lock"
    }) {
        let entry = entry.expect("walk the fixture");
        let target = to.join(entry.path().strip_prefix(from).expect("inside the fixture"));
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).expect("create a directory");
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

// Property (9.1): the first run runs every binary and the second skips every one, with either
// runner.
#[hegel::test(test_cases = 2)]
fn the_second_run_skips_every_binary(tc: TestCase) {
    let via_cargo = tc.draw(gs::booleans());
    let runner: &[&str] = if via_cargo { &["--via-cargo"] } else { &[] };
    let fixture = Fixture::new();

    fixture.testcache_ok(runner);
    assert_eq!(fixture.take_runs(), names(&ALL));

    let second = fixture.testcache_ok(runner);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
    assert!(stderr(&second).contains("0 run"), "{}", stderr(&second));
}

// Property (9.2, 9.6): changing one crate re-runs exactly the binaries that depend on it; for
// `beta`, the integration test re-runs because the binary it spawns changed.
#[hegel::test(test_cases = 3)]
fn changing_a_crate_reruns_only_its_dependents(tc: TestCase) {
    let (file, expected): (&str, &[&str]) = tc.draw(
        gs::sampled_from(&[
            ("alpha/src/lib.rs", &["alpha", "gamma", "reads_input"][..]),
            ("gamma/src/lib.rs", &["gamma"][..]),
            ("beta/src/main.rs", &["betatool", "spawns_tool"][..]),
        ])
        .print_as_debug(),
    );
    let stamp = tc.draw(gs::integers::<u32>().min_value(1));
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    fixture.set_stamp(file, stamp);
    let output = fixture.testcache_ok(&["--explain"]);

    assert_eq!(fixture.take_runs(), names(expected));
    if file.starts_with("beta") {
        assert!(stderr(&output).contains("spawned bin betatool changed"), "{}", stderr(&output));
    }
}

// Property (9.3): changing a declared input, or a declared variable, re-runs every binary the
// declaration covers.
#[hegel::test(test_cases = 3)]
fn changing_a_declared_input_reruns_what_it_covers(tc: TestCase) {
    let change = tc.draw(gs::sampled_from(&["data", "package input", "env"]).print_as_debug());
    let value = tc.draw(gs::integers::<u32>().min_value(1)).to_string();
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    let expected: &[&str] = match change {
        "data" => {
            fixture.write("data/value.txt", &value);
            &ALL
        }
        "package input" => {
            fixture.write("alpha/fixtures/extra.txt", &value);
            &["alpha", "reads_input"]
        }
        _ => &ALL,
    };
    let env_value = OsStr::new(&value);
    let extra_env: &[(&str, &OsStr)] =
        if change == "env" { &[("FIXTURE_ENV", env_value)] } else { &[] };
    let output = fixture.testcache(&[], extra_env);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fixture.take_runs(), names(expected));
}

// Property (9.4): a failing binary is not recorded, so it runs again, and only it.
#[hegel::test(test_cases = 2)]
fn a_failing_binary_runs_again(tc: TestCase) {
    let failing = tc.draw(gs::sampled_from(&ALL).print_as_debug());
    let fixture = Fixture::new();
    fixture.fail(failing, true);

    let output = fixture.testcache(&["--no-fail-fast"], &[]);
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    assert_eq!(fixture.take_runs(), names(&ALL));

    fixture.fail(failing, false);
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), names(&[failing]));
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
}

// Property (9.5): different arguments after `--` re-run everything, the same ones again skip.
#[hegel::test(test_cases = 2)]
fn changed_test_arguments_rerun(tc: TestCase) {
    let argument = tc.draw(
        gs::sampled_from(&["--test-threads=1", "--nocapture", "--include-ignored"]).print_as_debug(),
    );
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    fixture.testcache_ok(&["--", argument]);
    assert_eq!(fixture.take_runs(), names(&ALL));
    fixture.testcache_ok(&["--", argument]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
}

// Property (9.7): --no-cache runs everything and records it.
#[hegel::test(test_cases = 1)]
fn no_cache_runs_everything(tc: TestCase) {
    let explain = tc.draw(gs::booleans());
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    let args: &[&str] = if explain { &["--no-cache", "--explain"] } else { &["--no-cache"] };
    fixture.testcache_ok(args);
    assert_eq!(fixture.take_runs(), names(&ALL));
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
}

// Property (9.8): `status` runs nothing and records nothing, and `clear` forgets every pass.
#[hegel::test(test_cases = 2)]
fn status_changes_nothing_and_clear_forgets(tc: TestCase) {
    let ran_before = tc.draw(gs::booleans());
    let fixture = Fixture::new();
    if ran_before {
        fixture.testcache_ok(&[]);
        fixture.take_runs();
    }

    let status = fixture.testcache_ok(&["status"]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
    let verb = if ran_before { "Would skip" } else { "Would run" };
    assert!(stderr(&status).contains(verb), "{}", stderr(&status));

    fixture.testcache_ok(&[]);
    let expected = if ran_before { BTreeSet::new() } else { names(&ALL) };
    assert_eq!(fixture.take_runs(), expected);

    fixture.testcache_ok(&["clear"]);
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), names(&ALL));
}

// Property (9.9, conformance): every test binary sees the same working directory and the same
// environment under `cargo testcache` (direct runner) as under `cargo test`.
#[hegel::test(test_cases = 1)]
fn the_direct_runner_matches_cargo_test(tc: TestCase) {
    let cargo_first = tc.draw(gs::booleans());
    let fixture = Fixture::new();
    let under_cargo = fixture.temp.path().join("dump-cargo");
    let under_testcache = fixture.temp.path().join("dump-testcache");
    for dir in [&under_cargo, &under_testcache] {
        fs::create_dir(dir).expect("create a dump directory");
    }
    let run_cargo = || {
        let output = fixture.cargo(&["test"], &[("FIXTURE_DUMP_DIR", under_cargo.as_os_str())]);
        assert!(output.status.success(), "{}", stderr(&output));
    };
    let run_testcache = || {
        let output = fixture
            .testcache(&["--no-cache"], &[("FIXTURE_DUMP_DIR", under_testcache.as_os_str())]);
        assert!(output.status.success(), "{}", stderr(&output));
    };
    if cargo_first {
        run_cargo();
        run_testcache();
    } else {
        run_testcache();
        run_cargo();
    }

    let read = |dir: &Path, name: &str| -> Vec<String> {
        fs::read_to_string(dir.join(format!("{name}.env")))
            .unwrap_or_else(|error| panic!("{name} dumped its environment: {error}"))
            .lines()
            .filter(|line| !line.starts_with("FIXTURE_DUMP_DIR="))
            .map(str::to_owned)
            .collect()
    };
    for name in ALL {
        assert_eq!(read(&under_testcache, name), read(&under_cargo, name), "{name}");
    }
}
```

- [ ] **Step 3: Run the fixture tests**

Run: `cargo test -p cargo-testcache --test fixture -- --nocapture`
Expected: PASS, 8 tests. They build the fixture many times, so expect a few minutes.

If `the_direct_runner_matches_cargo_test` fails, its message shows the two environments side by
side. Fix the difference in `exec::binary_env` or `exec::library_path` (task 10) so the direct
runner gives what Cargo gives, rerun, and note what changed in the changelog's Decisions. Do not
loosen the test: a variable that legitimately cannot match must be argued for in the Decisions
section and filtered by name in `read`, with a comment saying why.

- [ ] **Step 4: Gate and commit**

```bash
just fmt && just gate
git add crates/cargo-testcache/tests
git commit -m "test: fixture workspace, end-to-end and conformance tests"
```

### Task 15: CI on macOS, documentation, changelog

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`, `.env.example`,
  `docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`
- Create: `changelog/<date +%Y-%m-%d-%H%M>-cargo-testcache-v1.md`

- [ ] **Step 1: CI job for macOS**

The `test` job already runs the fixture tests on Linux (under coverage). Add, after the `msrv`
job in `.github/workflows/ci.yml`:

```yaml
  # The fixture tests on macOS, where Cargo sets DYLD_FALLBACK_LIBRARY_PATH rather than
  # LD_LIBRARY_PATH: the direct runner's conformance test must pass on both before direct
  # execution can stay the default (design section 10, question 1).
  fixture-macos:
    name: fixture tests (macOS)
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - run: rustup toolchain install
      - uses: Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0 # v2.9.3
        with:
          save-if: ${{ github.ref == 'refs/heads/main' }}
      - run: cargo test -p cargo-testcache --test fixture --locked
```

- [ ] **Step 2: README**

Replace the first line of `README.md` (`# cargo-testcache`) with:

````markdown
# cargo-testcache

`cargo testcache` runs `cargo test`, but skips every test binary that already passed with the
same executable, the same binaries it spawns, the same declared input files and environment, and
the same test arguments, the way Go's test cache does. Failures are never cached. The design is
in `docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`.

## Usage

```
cargo install --path crates/cargo-testcache
cargo testcache [OPTIONS] [CARGO TEST ARGS] [-- TEST BINARY ARGS]
cargo testcache status [CARGO TEST ARGS]     # what would run, and why; runs nothing
cargo testcache clear                        # forget every recorded pass
```

| Option | Effect |
|---|---|
| `--no-cache` | Run everything and record the passes |
| `--explain` | Print why each binary runs (binary changed, input `x` changed, env `Y` changed, ...) |
| `--via-cargo` | Run each binary through `cargo test -p <pkg> --test <name>` instead of directly |
| `--no-doc` | Skip doc tests (they are never cached) |

Declare what tests read at runtime in the root `Cargo.toml`; a package's own table adds to it,
with `inputs` relative to the package:

```toml
[workspace.metadata.testcache]
inputs = ["migrations/**", "tests/fixtures/**"]  # files read at runtime
env = ["DATABASE_IMAGE"]                         # variables whose values matter
always_run = ["*/live_*"]                        # package/target patterns never skipped
spawns = ["myapp-server"]                        # binaries spawned by path, under target/<profile>
```

The record lives in `<target-dir>/testcache/passes.jsonl`; `cargo clean` removes it too. Keep a
plain `cargo test` in CI: the cache is for local iteration.
````

- [ ] **Step 3: Spec status and `.env.example`**

In the design, change `Status: draft, 2026-11-04.` to
`Status: implemented (v1), 2026-11-04; see changelog/ for decisions taken during implementation.`

`.env.example` needs no new variable (the tool reads only `CARGO_TESTCACHE_LOG_FORMAT` and
`RUST_LOG`). Check that it no longer mentions the shutdown timeout.

- [ ] **Step 4: Changelog**

Create `changelog/$(date +%Y-%m-%d-%H%M)-cargo-testcache-v1.md` with the clock's local time in
the heading (`date "+%Y-%m-%d %H:%M %Z"`):

```markdown
# YYYY-MM-DD HH:MM TZ: cargo-testcache v1

## Summary

Implemented the design in `docs/superpowers/specs/2026-11-04-cargo-testcache-design.md` following
`docs/superpowers/plans/2026-11-04-cargo-testcache.md`: `cargo testcache` builds with
`cargo test --no-run`, keys every test binary on its executable, the binaries it spawns, its
declared inputs and environment and the test arguments, and skips binaries whose key matches a
recorded pass.

## Changed

- `crates/cargo-testcache-core`: new modules `key`, `args`, `manifest`, `workspace`, `discover`,
  `inputs`, `store`, `plan`, `exec`, `report`, `app`; `config` and `error` reworked; the
  template's async `app` removed.
- `crates/cargo-testcache/src/main.rs`: the command line (`status`, `clear`, `--no-cache`,
  `--explain`, `--via-cargo`, `--no-doc`); `shutdown.rs` and `tests/startup.rs` removed.
- `crates/cargo-testcache/tests/fixture.rs` and `tests/fixtures/ws`: end-to-end and conformance
  tests.
- `Cargo.toml`: dependencies for hashing, globbing, walking, progress and parallelism; tokio,
  console-subscriber and nix removed; MSRV 1.89.
- `justfile`, `.github/workflows/ci.yml`: tokio-console recipes removed; macOS fixture job added.
- `README.md`, `.env.example`, `.claude/CLAUDE.md`, the design's status line.

## Tests

(Fill in from the actual runs: `just gate` result, the number of tests, the fixture test time,
`just deny`, `just msrv`.)

## Decisions

(Copy the "Decisions taken in this plan" list from the plan, plus anything the conformance test
forced in task 14.)

## Not done

- Per-test selection, doc-test caching, `--jobs`, a nextest runner, the determinism check and a
  runtime-reads lint: design section 10 and the "future work" notes.
- libtest's passed/ignored counts in records (Decision 3).
- Windows is untested: no CI job runs there.
```

Replace both parenthesized paragraphs with the real content before committing.

- [ ] **Step 5: Final verification**

Run each and read its output:

```bash
just fmt && just gate
just deny
just msrv
```

Expected: all pass. Then try the tool on this repository twice (`cargo run -q -p
cargo-testcache -- testcache`) and confirm the second run skips every binary.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "docs: README usage, CI on macOS, changelog for cargo-testcache v1"
```
