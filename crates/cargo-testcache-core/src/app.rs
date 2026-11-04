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

/// A file's digest, or why it could not be read. Spelled out because the crate's `Result`
/// alias fixes the error type to [`Error`], and these errors are messages for a run reason.
type FileDigest = std::result::Result<Digest, String>;

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
    let session = Session {
        cargo: &cargo,
        workspace: &workspace,
        build: &build,
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
    digests: &'a HashMap<PathBuf, FileDigest>,
    environment: &'a BTreeMap<OsString, OsString>,
    tool: String,
    args: Digest,
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
        let to_run = planned
            .iter()
            .filter(|planned| matches!(planned.decision, Decision::Run(_)))
            .count();
        // The toolchain is needed only by the direct runner, and to see whether a configured
        // target runner rules the direct runner out; `--via-cargo` never needs rustc.
        let toolchain = if to_run > 0 && !options.via_cargo {
            Some(Toolchain::detect(self.cargo)?)
        } else {
            None
        };
        let runner_configured = toolchain
            .as_ref()
            .is_some_and(|toolchain| exec::configured_runner(toolchain, &options.cargo_args));
        if runner_configured {
            tracing::info!("a configured target runner forces the via-cargo runner");
        }
        let via_cargo = options.via_cargo || runner_configured;
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
                Decision::Run(reason) => {
                    // `bool::then` gives `Some(closure())` when true, else `None`.
                    let detail = options.explain.then(|| reason.to_string());
                    progress.line(&report::status_line(
                        "Running",
                        &planned.label,
                        detail.as_deref(),
                    ));
                    progress.set_message(format!("running {}", planned.label));
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
                    // Only a pass with a computable key is recorded.
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
    fn run_whole(&self, store: &mut Store, planned: Vec<Planned<'_>>) -> Result<Outcome> {
        let options = self.options;
        if options.explain {
            for planned in &planned {
                if let Decision::Run(reason) = &planned.decision {
                    let detail = reason.to_string();
                    eprintln!(
                        "{}",
                        report::status_line("Running", &planned.label, Some(&detail))
                    );
                }
            }
        }
        eprintln!("testcache: nothing is cached; running cargo test once");
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
                let detail = reason.to_string();
                eprintln!(
                    "{}",
                    report::status_line("Would run", &planned.label, Some(&detail))
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
        paths.extend(
            bins.iter()
                .map(|(name, path)| (format!("bin {name}"), path.clone())),
        );
    }
    // `peekable` lets the loop look at the next item without taking it, to see whether there
    // is any before working out the directory.
    let mut spawns = workspace.spawns(package).peekable();
    if spawns.peek().is_some() {
        let dir = workspace.artifact_dir(&binary.executable).ok_or_else(|| {
            format!(
                "{} is not under the build directory",
                binary.executable.display()
            )
        })?;
        paths.extend(spawns.map(|spawn| (format!("spawns {spawn}"), dir.join(spawn))));
    }
    Ok(paths)
}

/// Hashes every distinct file once, in parallel, with a progress bar.
fn hash_files<'a>(paths: impl IntoIterator<Item = &'a Path>) -> HashMap<PathBuf, FileDigest> {
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
        // Collecting an iterator of `Result`s into one `Result` stops at the first error.
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
    use crate::workspace::testing::{ID, metadata};

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
}
