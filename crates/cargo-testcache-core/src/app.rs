//! One `cargo testcache` run: build, hash, decide, run, record, report (design section 4).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use crate::args::CargoArgs;
use crate::cargo_config::CargoConfig;
use crate::discover::{self, Build, TargetId, TargetKind, TestBinary};
use crate::error::{Error, Result};
use crate::exec::{self, LIBRARY_PATH_VAR, RunContext, Toolchain};
use crate::inputs::{InputIndex, InputLevel};
use crate::key::{self, Digest, KeyParts, PartResult};
use crate::plan::{self, Change, Decision, Policy, RunReason};
use crate::report::{self, Progress, Tally};
use crate::store::Store;
use crate::workspace::{Package, Workspace};

/// The exit status `cargo test` uses for failures, used when a process died without one.
const FAILURE_CODE: i32 = 101;

/// Why a via-cargo pass of one target is not recorded.
const DIFFERENT_BINARY: &str =
    "Cargo builds a different binary for this target alone (feature unification)";

/// Why the direct runner cannot be used: Cargo's configuration has an `[env]` table.
const CONFIG_ENV: &str = "Cargo's configuration has an [env] table, which only Cargo applies";

/// Why the direct runner cannot be used: Cargo's configuration sets a target runner.
const CONFIG_RUNNER: &str =
    "Cargo's configuration sets a target runner (target.<triple>.runner), which only Cargo applies";

/// Why the direct runner cannot be used: the environment sets a target runner.
const ENV_RUNNER: &str =
    "CARGO_TARGET_<TRIPLE>_RUNNER sets a target runner, which only Cargo applies";

/// Why a pass is not recorded when a binary changed while it ran.
const CHANGED_DURING_RUN: &str =
    "the binary changed during the run (another build relinked it); the pass is not recorded";

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
///
/// # Returns
///
/// The path or command name.
pub fn cargo_path() -> PathBuf {
    // `map_or_else(default, f)` is `f(value) if value is not None else default()`.
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
        // `status` runs nothing, so it cannot hand over to `cargo test` either.
        if options.status_only {
            return Err(Error::UnsupportedArgument {
                argument: flag.to_owned(),
                reason: "status cannot run cargo test unchanged",
            });
        }
        tracing::info!("{flag} given: running cargo test without the cache");
        let mut command =
            exec::cargo_test_command(&cargo, &options.cargo_args, &[], &options.test_args);
        let outcome = exec::run(&mut command)?;
        // `code()` is `None` when a signal killed the process.
        return Ok(Outcome::Exited(
            outcome.status.code().unwrap_or(FAILURE_CODE),
        ));
    }
    let workspace = Workspace::load(&cargo, &options.cargo_args)?;
    let (status, build) = discover::build(&cargo, &options.cargo_args)?;
    if !status.success() {
        return Ok(Outcome::Exited(status.code().unwrap_or(FAILURE_CODE)));
    }
    let mut store = Store::open(&workspace.cache_dir())?;
    let cargo_config = CargoConfig::load(&options.cargo_args);
    let session = Session {
        cargo: &cargo,
        workspace: &workspace,
        build: &build,
        cargo_config: &cargo_config,
        options,
    };
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
// The `'a` lifetime says a `Planned` borrows its binary from a `Build` and cannot outlive it;
// Python has no equivalent, since there every reference keeps its object alive.
struct Planned<'a> {
    binary: &'a TestBinary,
    label: String,
    parts: PartResult<KeyParts>,
    /// The binaries it may spawn, by their name in the key: hashed again before a pass is
    /// recorded. Empty when they could not be known, and then `parts` is an error.
    spawned: Vec<(String, PathBuf)>,
    decision: Decision,
    /// Why it runs, naming the tables that declare each changed input; `None` when it is
    /// skipped. Worked out while planning, when the input index is at hand.
    explanation: Option<String>,
}

/// What every step of a run shares.
struct Session<'a> {
    cargo: &'a Path,
    workspace: &'a Workspace,
    build: &'a Build,
    /// Read once per run; its digest is part of every binary's key.
    cargo_config: &'a CargoConfig,
    options: &'a Options,
}

/// What computing a key needs besides the binary.
struct KeyContext<'a> {
    workspace: &'a Workspace,
    inputs: &'a InputIndex,
    digests: &'a HashMap<PathBuf, PartResult<Digest>>,
    environment: &'a BTreeMap<OsString, OsString>,
    tool: String,
    args: Digest,
    cargo_config: PartResult<Digest>,
}

// `Session<'_>`: the impl works for a `Session` borrowing for any lifetime; `'_` lets the
// compiler name it.
impl Session<'_> {
    /// Hashes everything and decides, for each binary in run order, whether it runs.
    fn plan(&self, store: &Store) -> Result<Vec<Planned<'_>>> {
        let binaries = ordered_binaries(self.workspace, self.build)?;
        let spawned: Vec<_> = binaries
            .iter()
            .map(|(binary, package)| spawned_paths(self.workspace, self.build, binary, package))
            .collect();
        // `.iter().flatten()` over `Result`s yields only the `Ok` values; the second `flatten`
        // walks each `Vec` inside.
        let to_hash = binaries
            .iter()
            .map(|(binary, _)| binary.executable.as_path())
            .chain(
                spawned
                    .iter()
                    .flatten()
                    .flatten()
                    .map(|(_, path)| path.as_path()),
            );
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
            cargo_config: self.cargo_config.digest(),
        };
        Ok(binaries
            .into_iter()
            .zip(spawned)
            .map(|((binary, package), spawned)| {
                // `as_deref` borrows the paths; the error, only when there is one, is copied.
                let parts = key_parts(
                    binary,
                    package,
                    spawned.as_deref().map_err(String::clone),
                    &context,
                );
                let policy = Policy {
                    no_cache: self.options.no_cache,
                    always_run: always_run.matches(package.name(), &binary.id.name),
                };
                let decision = plan::decide(&parts, store.lookup(&binary.id), policy);
                let explanation = match &decision {
                    Decision::Run(reason) => Some(explain(reason, &binary.id, &inputs)),
                    Decision::Skip { .. } => None,
                };
                Planned {
                    decision,
                    explanation,
                    label: report::label(binary, package.name(), package.root()),
                    binary,
                    parts,
                    spawned: spawned.unwrap_or_default(),
                }
            })
            .collect())
    }

    /// Runs what has to run, records the passes, runs doc tests and prints the summary.
    fn execute(&self, store: &mut Store, planned: Vec<Planned<'_>>) -> Result<Outcome> {
        let options = self.options;
        let to_run = planned
            .iter()
            .filter(|planned| matches!(planned.decision, Decision::Run(_)))
            .count();
        // Why the direct runner cannot be used, if something rules it out: only Cargo applies a
        // configuration's `[env]` table or a target runner to the binaries it runs.
        let mut ruled_out = if self.cargo_config.sets_env() {
            Some(CONFIG_ENV)
        } else if self.cargo_config.sets_runner() {
            Some(CONFIG_RUNNER)
        } else {
            None
        };
        // The toolchain is needed only by the direct runner, and to see whether a runner set in
        // the environment rules the direct runner out; `--via-cargo` never needs rustc.
        let toolchain = if to_run > 0 && !options.via_cargo && ruled_out.is_none() {
            Some(Toolchain::detect(self.cargo)?)
        } else {
            None
        };
        if toolchain
            .as_ref()
            .is_some_and(|toolchain| exec::configured_runner(toolchain, &options.cargo_args))
        {
            ruled_out = Some(ENV_RUNNER);
        }
        if let Some(why) = ruled_out
            && !options.via_cargo
            && to_run > 0
        {
            tracing::info!("using the via-cargo runner: {why}");
        }
        let via_cargo = options.via_cargo || ruled_out.is_some();
        if via_cargo && to_run == planned.len() && to_run > 0 && !options.no_doc {
            return self.run_whole(store, planned);
        }
        // `filter` turns `Some` into `None` when a runner forces cargo: then no direct-run
        // context exists, and each binary gets a via-cargo command below.
        let context = toolchain
            .as_ref()
            .filter(|_| !via_cargo)
            .map(|toolchain| RunContext {
                workspace: self.workspace,
                build: self.build,
                toolchain,
                inherited_library_path: env::var_os(LIBRARY_PATH_VAR),
            });
        let fail_fast = !options.cargo_args.has("--no-fail-fast");
        let total = planned.len();
        let mut tally = Tally::new(total);
        let progress = Progress::new(total, "test binaries");
        // `enumerate` pairs each item with its index, like Python's `enumerate`.
        for (index, planned) in planned.into_iter().enumerate() {
            match planned.decision {
                Decision::Skip { passed_at } => {
                    let detail = passed_ago(unix_now(), passed_at);
                    progress.line(&report::status_line(
                        "Skipped",
                        &planned.label,
                        Some(&detail),
                    ));
                    tally.skipped();
                }
                // `ref` borrows the reason instead of moving it out of `planned`, which
                // `record_pass` takes whole once the reason is no longer used.
                Decision::Run(ref reason) => {
                    // `then_some` gives `Some(value)` when true, else `None`; `flatten` turns the
                    // `Option<Option<&str>>` into an `Option<&str>`.
                    let detail = options
                        .explain
                        .then_some(planned.explanation.as_deref())
                        .flatten();
                    progress.line(&report::status_line("Running", &planned.label, detail));
                    progress.set_message(format!("running {}", planned.label));
                    // The direct runner runs the hashed executable itself; through Cargo, the
                    // target alone may build a different one, whose pass the key cannot vouch for.
                    let (mut command, same_binary) = match &context {
                        Some(context) => (
                            exec::direct_command(planned.binary, context, &options.test_args)?,
                            true,
                        ),
                        None => (
                            exec::via_cargo_command(
                                self.cargo,
                                planned.binary,
                                &options.cargo_args,
                                &options.test_args,
                            ),
                            progress.suspended(|| {
                                exec::via_cargo_builds_same_binary(
                                    self.cargo,
                                    planned.binary,
                                    &options.cargo_args,
                                )
                            })?,
                        ),
                    };
                    let outcome = progress.suspended(|| exec::run(&mut command))?;
                    let passed = outcome.passed();
                    tally.ran(reason.category(), passed);
                    // Only a pass of the hashed executable is recorded.
                    if !same_binary {
                        if passed {
                            progress.line(&report::status_line(
                                "Not recorded",
                                &planned.label,
                                Some(&different_binary_detail(ruled_out)),
                            ));
                        }
                    } else {
                        record_pass(store, planned, passed, outcome.duration, |line| {
                            progress.line(line);
                        })?;
                    }
                    if !passed && fail_fast {
                        tally.not_run(total - index - 1);
                        break;
                    }
                }
            }
            progress.inc();
        }
        progress.finish();
        self.run_doc_tests(&mut tally, fail_fast)?;
        eprintln!("{tally}");
        Ok(if tally.any_failed() {
            Outcome::TestsFailed
        } else {
            Outcome::Passed
        })
    }

    /// The via-cargo shortcut when nothing is cached: one plain `cargo test` (doc tests
    /// included), recording every binary if it passes.
    ///
    /// This runs the user's own arguments, unchanged, so Cargo selects the same packages and
    /// unifies features as the `--no-run` build that was hashed did: it runs the hashed
    /// executables, and needs no per-target build check.
    fn run_whole(&self, store: &mut Store, planned: Vec<Planned<'_>>) -> Result<Outcome> {
        let options = self.options;
        if options.explain {
            for planned in &planned {
                if let Some(detail) = &planned.explanation {
                    eprintln!(
                        "{}",
                        report::status_line("Running", &planned.label, Some(detail))
                    );
                }
            }
        }
        eprintln!("testcache: every binary must run; running cargo test once");
        let mut command =
            exec::cargo_test_command(self.cargo, &options.cargo_args, &[], &options.test_args);
        let outcome = exec::run(&mut command)?;
        let passed = outcome.passed();
        let mut tally = Tally::new(planned.len());
        tally.ran_together(
            planned
                .iter()
                .filter_map(|planned| match &planned.decision {
                    Decision::Run(reason) => Some(reason.category()),
                    Decision::Skip { .. } => None,
                }),
            passed,
        );
        for planned in planned {
            record_pass(store, planned, passed, outcome.duration, |line| {
                eprintln!("{line}");
            })?;
        }
        eprintln!("{tally}");
        Ok(if passed {
            Outcome::Passed
        } else {
            Outcome::TestsFailed
        })
    }

    /// Runs `cargo test --doc` as `cargo test` would have: unless `--no-doc`, a target
    /// selection, no library, or an earlier failure under fail-fast rules it out.
    fn run_doc_tests(&self, tally: &mut Tally, fail_fast: bool) -> Result<()> {
        let options = self.options;
        let has_lib = self
            .build
            .binaries
            .iter()
            .any(|binary| binary.id.kind == TargetKind::Lib);
        if options.no_doc
            || options.cargo_args.selects_targets()
            || !has_lib
            || (fail_fast && tally.any_failed())
        {
            return Ok(());
        }
        eprintln!("{}", report::status_line("Doc-tests", "not cached", None));
        let mut command = exec::cargo_test_command(
            self.cargo,
            &options.cargo_args,
            &["--doc"],
            &options.test_args,
        );
        tally.doc_tests(exec::run(&mut command)?.passed());
        Ok(())
    }
}

/// The detail of a via-cargo pass that is not recorded because Cargo built a different binary.
///
/// # Arguments
///
/// * `ruled_out` - Why the direct runner cannot be used, if something rules it out.
///
/// # Returns
///
/// The detail: it suggests the direct runner, which runs the hashed executable, or says why
/// that runner cannot be used here.
fn different_binary_detail(ruled_out: Option<&str>) -> String {
    match ruled_out {
        None => format!(
            "{DIFFERENT_BINARY}; the direct runner (without --via-cargo) runs the hashed one"
        ),
        Some(why) => format!("{DIFFERENT_BINARY}; the direct runner cannot be used: {why}"),
    }
}

/// Records a binary's pass under its key. Nothing is recorded for a failure, a key that could
/// not be computed, or a binary whose executable or spawned binaries no longer have the digests
/// in its key: another build relinked them between hashing and the end of the run, so the pass
/// may be the other build's. That last case is reported through `say`.
///
/// # Arguments
///
/// * `store` - Where passes are recorded.
/// * `planned` - The binary that ran.
/// * `passed` - Whether it exited 0.
/// * `duration` - How long it ran.
/// * `say` - Prints a status line.
///
/// # Errors
///
/// When the store cannot be written.
fn record_pass(
    store: &mut Store,
    planned: Planned<'_>,
    passed: bool,
    duration: Duration,
    say: impl Fn(&str),
) -> Result<()> {
    let Ok(parts) = planned.parts else {
        return Ok(());
    };
    if !passed {
        return Ok(());
    }
    let binary = Digest::of_file(&planned.binary.executable).ok();
    // Collecting `Option`s into one `Option` gives `None` if any file cannot be read.
    let spawned: Option<BTreeMap<String, Digest>> = planned
        .spawned
        .iter()
        .map(|(name, path)| {
            Digest::of_file(path)
                .ok()
                .map(|digest| (name.clone(), digest))
        })
        .collect();
    if !unchanged_since_hashing(&parts, binary, spawned.as_ref()) {
        say(&report::status_line(
            "Not recorded",
            &planned.label,
            Some(CHANGED_DURING_RUN),
        ));
        return Ok(());
    }
    if let Some(record) = plan::pass_record(&planned.binary.id, parts, passed, duration, unix_now())
    {
        store.record(record)?;
    }
    Ok(())
}

/// Whether the executable and spawned binaries, hashed again after a run, still have the
/// digests in the key.
///
/// # Arguments
///
/// * `parts` - The key parts computed before the run.
/// * `binary` - The executable's digest now, or `None` if it cannot be read.
/// * `spawned` - The spawned binaries' digests now, or `None` if one cannot be read.
///
/// # Returns
///
/// `true` only when every digest is readable and unchanged.
fn unchanged_since_hashing(
    parts: &KeyParts,
    binary: Option<Digest>,
    spawned: Option<&BTreeMap<String, Digest>>,
) -> bool {
    binary == Some(parts.binary) && spawned == Some(&parts.spawned)
}

/// Prints what `status` found.
fn report_status(planned: &[Planned<'_>]) -> Outcome {
    let mut tally = Tally::dry_run(planned.len());
    let now = unix_now();
    for planned in planned {
        match &planned.decision {
            Decision::Skip { passed_at } => {
                let detail = passed_ago(now, *passed_at);
                eprintln!(
                    "{}",
                    report::status_line("Would skip", &planned.label, Some(&detail))
                );
                tally.skipped();
            }
            Decision::Run(reason) => {
                eprintln!(
                    "{}",
                    report::status_line(
                        "Would run",
                        &planned.label,
                        planned.explanation.as_deref()
                    )
                );
                tally.ran(reason.category(), true);
            }
        }
    }
    eprintln!("{tally}");
    Outcome::Passed
}

/// The detail of a skipped binary: `passed 5m ago`.
fn passed_ago(now: u64, passed_at: u64) -> String {
    let age = report::format_age(Duration::from_secs(now.saturating_sub(passed_at)));
    format!("passed {age} ago")
}

/// The test binaries with their packages, in the order `cargo test` runs them.
fn ordered_binaries<'a>(
    workspace: &'a Workspace,
    build: &'a Build,
) -> Result<Vec<(&'a TestBinary, &'a Package)>> {
    // Collecting `Result`s into `Result<Vec<_>>` fails on the first unknown package.
    let mut binaries = build
        .binaries
        .iter()
        .map(|binary| {
            workspace
                .package(&binary.id.package_id)
                .map(|package| (binary, package))
                .ok_or_else(|| Error::UnknownPackage {
                    id: binary.id.package_id.clone(),
                })
        })
        .collect::<Result<Vec<_>>>()?;
    // Tuples compare element by element, like Python's; `TargetKind`'s order is Cargo's.
    binaries.sort_by(|(a, a_package), (b, b_package)| {
        (a_package.name(), a.id.kind, &a.id.name).cmp(&(b_package.name(), b.id.kind, &b.id.name))
    });
    Ok(binaries)
}

/// The binaries a test may spawn, by their name in the key: its package's bins for an
/// integration test or benchmark (`bin <name>`), the workspace's and package's `spawns`
/// (`spawns <path>`), and those of the package's `targets.<name>` tables that name its target
/// (`target <name> spawns <path>`). The name shows the level, so moving a declaration between
/// levels changes the key.
///
/// # Returns
///
/// The names and paths, or why they cannot be known.
fn spawned_paths(
    workspace: &Workspace,
    build: &Build,
    binary: &TestBinary,
    package: &Package,
) -> PartResult<Vec<(String, PathBuf)>> {
    let mut paths = Vec::new();
    if binary.id.kind.spawns_package_bins()
        && let Some(bins) = build.package_bins.get(&binary.id.package_id)
    {
        paths.extend(
            bins.iter()
                .map(|(name, path)| (format!("bin {name}"), path.clone())),
        );
    }
    // `move` lets the inner closure keep `key` after the outer one's call has returned.
    let target_spawns = package
        .config()
        .targets_matching(binary.id.kind, &binary.id.name)
        .flat_map(|(key, target)| {
            target
                .spawns
                .iter()
                .map(move |spawn| (format!("target {key} spawns {spawn}"), spawn.as_str()))
        });
    // `peekable` lets the loop look at the next item without taking it, to see whether there
    // is any before working out the directory.
    let mut spawns = workspace
        .spawns(package)
        .map(|spawn| (format!("spawns {spawn}"), spawn))
        .chain(target_spawns)
        .peekable();
    if spawns.peek().is_some() {
        let dir = workspace.artifact_dir(&binary.executable).ok_or_else(|| {
            format!(
                "{} is not under the build directory",
                binary.executable.display()
            )
        })?;
        paths.extend(spawns.map(|(name, spawn)| (name, dir.join(spawn))));
    }
    Ok(paths)
}

/// Hashes every distinct file once, in parallel, with a progress bar.
fn hash_files<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> HashMap<PathBuf, PartResult<Digest>> {
    let unique: HashSet<&Path> = paths.into_iter().collect();
    let progress = Progress::new(unique.len(), "hashing test binaries");
    // `into_par_iter` (rayon) spreads the work over a thread pool, like a
    // `ThreadPoolExecutor.map` that also collects the results into a dict.
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
    spawned: PartResult<&[(String, PathBuf)]>,
    context: &KeyContext<'_>,
) -> PartResult<KeyParts> {
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
        // Collecting an iterator of `Result`s into one `Result` stops at the first error.
        spawned: spawned?
            .iter()
            .map(|(name, path)| Ok((name.clone(), digest(path)?)))
            .collect::<PartResult<_>>()?,
        inputs: context.inputs.for_binary(&binary.id)?,
        env: key::env_part(context.workspace.env_names(package), context.environment),
        args: context.args,
        cargo_config: context.cargo_config.clone()?,
    })
}

/// Why a binary runs, as `--explain` and `status` print it: the reason, with each changed
/// input followed by the tables that declare it now, such as
/// `input data/x changed (package `alpha`)`.
///
/// # Arguments
///
/// * `reason` - Why it runs.
/// * `binary` - Its target, which decides the tables that apply to it.
/// * `inputs` - This run's inputs.
///
/// # Returns
///
/// The text. An input no table declares any longer gets no note.
fn explain(reason: &RunReason, binary: &TargetId, inputs: &InputIndex) -> String {
    reason.describe(|change| match change {
        Change::Input(path) => levels_note(&inputs.declaring_levels(path, binary)),
        Change::Tool
        | Change::Binary
        | Change::Spawned(_)
        | Change::Env(_)
        | Change::Args
        | Change::CargoConfig => None,
    })
}

/// The levels as a note: `workspace, package `a``, or `None` when there are none.
fn levels_note(levels: &[InputLevel<'_>]) -> Option<String> {
    (!levels.is_empty()).then(|| {
        levels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// Now, in seconds since the Unix epoch (0 if the clock is before it).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;
    use crate::key::testing::{draw_digest, draw_parts};
    use crate::workspace::testing::{ID, metadata};

    // Property: the detail of an unrecorded via-cargo pass suggests the direct runner exactly
    // when nothing rules it out, and otherwise says what does.
    #[hegel::test]
    fn the_hint_names_why_the_direct_runner_cannot_be_used(tc: TestCase) {
        let ruled_out = tc.draw(gs::optional(
            gs::sampled_from(&[CONFIG_ENV, CONFIG_RUNNER, ENV_RUNNER]).print_as_debug(),
        ));

        let detail = different_binary_detail(ruled_out);

        assert!(detail.starts_with(DIFFERENT_BINARY), "{detail}");
        assert_eq!(
            detail.contains("without --via-cargo"),
            ruled_out.is_none(),
            "{detail}"
        );
        if let Some(why) = ruled_out {
            assert!(detail.ends_with(why), "{detail}");
        }
    }

    /// How the files are found when they are hashed again after a run.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Rehashed {
        Unchanged,
        BinaryChanged,
        BinaryUnreadable,
        SpawnedChanged,
        SpawnedAdded,
        SpawnedRemoved,
        SpawnedUnreadable,
    }

    // Property: a pass is recorded only when the executable and every spawned binary, hashed
    // again after the run, are readable and have the digests in the key.
    #[hegel::test]
    fn only_unchanged_binaries_are_recorded(tc: TestCase) {
        let parts = draw_parts(&tc);
        let rehashed = tc.draw(
            gs::sampled_from(&[
                Rehashed::Unchanged,
                Rehashed::BinaryChanged,
                Rehashed::BinaryUnreadable,
                Rehashed::SpawnedChanged,
                Rehashed::SpawnedAdded,
                Rehashed::SpawnedRemoved,
                Rehashed::SpawnedUnreadable,
            ])
            .print_as_debug(),
        );
        let other = draw_digest(&tc);
        let mut binary = Some(parts.binary);
        let mut spawned = Some(parts.spawned.clone());
        let changes_something = match rehashed {
            Rehashed::Unchanged => false,
            Rehashed::BinaryChanged => {
                binary = Some(Digest::of_bytes(&parts.binary.to_string().into_bytes()));
                true
            }
            Rehashed::BinaryUnreadable => {
                binary = None;
                true
            }
            Rehashed::SpawnedChanged => {
                match spawned.as_mut().and_then(|map| map.values_mut().next()) {
                    Some(digest) => {
                        *digest = Digest::of_bytes(&digest.to_string().into_bytes());
                        true
                    }
                    None => false,
                }
            }
            Rehashed::SpawnedAdded => spawned
                .as_mut()
                .is_some_and(|map| map.insert("added".to_owned(), other) != Some(other)),
            Rehashed::SpawnedRemoved => spawned
                .as_mut()
                .is_some_and(|map| map.pop_first().is_some()),
            Rehashed::SpawnedUnreadable => {
                spawned = None;
                true
            }
        };

        let unchanged = unchanged_since_hashing(&parts, binary, spawned.as_ref());

        assert_eq!(unchanged, !changes_something, "{rehashed:?}");
    }

    // Property: binaries run in Cargo's order, by kind then name within a package, and a binary
    // of a package missing from the metadata is an error.
    #[hegel::test]
    fn binaries_run_in_cargos_order(tc: TestCase) {
        let targets = tc.draw(
            gs::vecs(gs::tuples!(
                gs::sampled_from(&[
                    TargetKind::Example,
                    TargetKind::Test,
                    TargetKind::Lib,
                    TargetKind::Bin,
                ])
                .print_as_debug(),
                gs::text().alphabet("ab").min_size(1).max_size(2)
            ))
            .max_size(6),
        );
        let unknown = tc.draw(gs::booleans());
        let workspace = Workspace::from_json(&metadata("0.1.0", None)).expect("valid metadata");
        let mut build = Build::default();
        for (kind, name) in &targets {
            build.binaries.push(TestBinary {
                id: TargetId {
                    package_id: ID.to_owned(),
                    kind: *kind,
                    name: name.clone(),
                },
                src_path: PathBuf::from("/ws/crates/a/src/lib.rs"),
                executable: PathBuf::from(format!("/ws/target/debug/deps/{name}-1")),
            });
        }
        if unknown {
            let mut stray = build
                .binaries
                .first()
                .cloned()
                .unwrap_or_else(|| TestBinary {
                    id: TargetId {
                        package_id: String::new(),
                        kind: TargetKind::Lib,
                        name: "x".into(),
                    },
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
        let mut expected: Vec<(TargetKind, &str)> = targets
            .iter()
            .map(|(kind, name)| (*kind, name.as_str()))
            .collect();
        expected.sort();
        assert_eq!(order, expected);
    }
    /// [`metadata`] where the workspace spawns `ws-tool` and package a (with a lib `a` and an
    /// integration test `reads`) has `fixtures/**` and spawns `pkg-tool`, its target `reads`
    /// adds `fixtures/r/**` and spawns `helper`, and its target `lib` adds `lib/**`.
    fn workspace_with_targets() -> Workspace {
        let mut value: serde_json::Value =
            serde_json::from_slice(&metadata("0.1.0", None)).expect("the fixture is JSON");
        value["metadata"]["testcache"]["spawns"] = serde_json::json!(["ws-tool"]);
        let package = &mut value["packages"][0];
        package["targets"] = serde_json::json!([
            { "name": "a", "kind": ["lib"] },
            { "name": "reads", "kind": ["test"] },
        ]);
        package["metadata"]["testcache"] = serde_json::json!({
            "inputs": ["fixtures/**"],
            "spawns": ["pkg-tool"],
            "targets": {
                "reads": { "inputs": ["fixtures/r/**"], "spawns": ["helper"] },
                "lib": { "inputs": ["lib/**"] },
            },
        });
        Workspace::from_json(value.to_string().as_bytes()).expect("valid metadata")
    }

    /// Draws one of package a's binaries: its library, its test `reads`, or a test `other`
    /// without a table of its own.
    fn draw_binary(tc: &TestCase) -> TestBinary {
        let (kind, name) = tc.draw(
            gs::sampled_from(&[
                (TargetKind::Lib, "a"),
                (TargetKind::Test, "reads"),
                (TargetKind::Test, "other"),
            ])
            .print_as_debug(),
        );
        TestBinary {
            id: TargetId {
                package_id: ID.to_owned(),
                kind,
                name: name.to_owned(),
            },
            src_path: PathBuf::from("/ws/crates/a/src/lib.rs"),
            executable: PathBuf::from(format!("/ws/target/debug/deps/{name}-1")),
        }
    }

    // Property: `--explain` follows a changed input with the levels that declare it for this
    // binary now, and leaves an input no applicable table declares, and other changes, alone.
    #[hegel::test]
    fn explain_names_the_levels_that_declare_an_input(tc: TestCase) {
        let workspace = workspace_with_targets();
        // /ws does not exist, so the walk finds no file; levels come from the globs alone.
        let inputs = InputIndex::collect(&workspace).expect("valid globs");
        let binary = draw_binary(&tc);
        let reads = binary.id.name == "reads";
        let lib = binary.id.kind == TargetKind::Lib;
        let fixture_levels = if reads {
            " (package `a`, target `reads`)"
        } else {
            " (package `a`)"
        };
        let cases = [
            ("data/x".to_owned(), " (workspace)".to_owned()),
            (
                "crates/a/fixtures/r/1".to_owned(),
                fixture_levels.to_owned(),
            ),
            (
                "crates/a/lib/2".to_owned(),
                if lib { " (target `lib`)" } else { "" }.to_owned(),
            ),
            ("elsewhere/3".to_owned(), String::new()),
        ];

        for (path, note) in cases {
            let reason = RunReason::Changed(vec![Change::Binary, Change::Input(path.clone())]);

            let explained = explain(&reason, &binary.id, &inputs);

            assert_eq!(
                explained,
                format!("binary changed; input {path} changed{note}"),
                "{binary:?}"
            );
        }
        assert_eq!(
            explain(&RunReason::NeverPassed, &binary.id, &inputs),
            "never passed"
        );
    }

    // Property: a binary spawns the workspace's and package's `spawns` under `spawns <path>`,
    // and its own targets' under `target <name> spawns <path>`, all under the profile directory.
    #[hegel::test]
    fn target_spawns_are_keyed_with_their_level(tc: TestCase) {
        let workspace = workspace_with_targets();
        let binary = draw_binary(&tc);
        let package = workspace.package(ID).expect("listed");

        let spawned =
            spawned_paths(&workspace, &Build::default(), &binary, package).expect("known paths");

        let mut expected = vec![
            (
                "spawns ws-tool".to_owned(),
                PathBuf::from("/ws/target/debug/ws-tool"),
            ),
            (
                "spawns pkg-tool".to_owned(),
                PathBuf::from("/ws/target/debug/pkg-tool"),
            ),
        ];
        if binary.id.name == "reads" {
            expected.push((
                "target reads spawns helper".to_owned(),
                PathBuf::from("/ws/target/debug/helper"),
            ));
        }
        assert_eq!(spawned, expected, "{binary:?}");
    }
}
