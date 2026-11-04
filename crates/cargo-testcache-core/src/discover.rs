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
        let (Some(kind), Some(executable)) = (
            TargetKind::from_cargo(&artifact.target.kind),
            artifact.executable,
        ) else {
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
/// When Cargo cannot be started, its output cannot be read, or waiting for it fails.
pub fn build(cargo: &Path, args: &CargoArgs) -> Result<(ExitStatus, Build)> {
    let mut child = Command::new(cargo)
        .args([
            "test",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .args(args.as_slice())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(Error::io("running cargo test --no-run"))?;
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let build = parse(BufReader::new(stdout));
    let status = child
        .wait()
        .map_err(Error::io("waiting for cargo test --no-run"))?;
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
/// What was built. Lines that are not JSON are logged at debug level and skipped; JSON messages
/// that cannot be read are logged as warnings and skipped.
///
/// # Errors
///
/// When reading fails.
pub fn parse(output: impl BufRead) -> Result<Build> {
    let mut build = Build::default();
    // Split on bytes, not `lines()`, so a line that is not UTF-8 is skipped, not a read error.
    for line in output.split(b'\n') {
        let line = line.map_err(Error::io("reading cargo's build output"))?;
        match serde_json::from_slice::<Message>(&line) {
            Ok(Message::CompilerArtifact(artifact)) => build.add_artifact(artifact),
            Ok(Message::BuildScriptExecuted(script)) => build
                .linked_paths
                .extend(script.linked_paths.iter().map(|path| linked_path(path))),
            Ok(Message::Other) => {}
            Err(error) if serde_json::from_slice::<serde_json::Value>(&line).is_ok() => {
                // Valid JSON that does not fit our types: Cargo's schema may have changed, and a
                // dropped artifact would make a test binary silently vanish.
                tracing::warn!(%error, "skipping a Cargo message that could not be read");
            }
            Err(error) => tracing::debug!(%error, "skipping a line that is not a message"),
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
                gs::sampled_from(&[
                    "lib",
                    "proc-macro",
                    "bin",
                    "test",
                    "bench",
                    "example",
                    "custom-build",
                ])
                .print_as_debug(),
            ),
            name: tc.draw(gs::text().alphabet("ab").min_size(1).max_size(3)),
            test: tc.draw(gs::booleans()),
            executable: tc.draw(gs::booleans()),
        }
    }

    fn line(fed: &Fed) -> String {
        let executable = fed
            .executable
            .then(|| format!("/exe/{}-{}", fed.package, fed.name));
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
        output.push(json!({ "reason": "compiler-artifact", "package_id": "p1" }).to_string());
        output.push(
            json!({ "reason": "build-script-executed", "package_id": "p1",
                "linked_libs": [], "linked_paths": ["native=/t/native", "/t/plain"],
                "cfgs": [], "env": [], "out_dir": "/t/out" })
            .to_string(),
        );

        // Bytes, not a String: a line that is not UTF-8 must be skipped, not abort the parse.
        let mut bytes: Vec<u8> = output.join("\n").into_bytes();
        bytes.extend_from_slice(b"\n\xff\xfe\n");

        let build = parse(Cursor::new(bytes)).expect("parse");

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
        let expected_kinds: Vec<TargetKind> = fed
            .iter()
            .filter(|f| f.test && f.executable && f.kind != "custom-build")
            .map(|f| match f.kind {
                "lib" | "proc-macro" => TargetKind::Lib,
                "bin" => TargetKind::Bin,
                "test" => TargetKind::Test,
                "bench" => TargetKind::Bench,
                "example" => TargetKind::Example,
                other => unreachable!("unexpected kind {other}"),
            })
            .collect();
        let found_kinds: Vec<TargetKind> = build.binaries.iter().map(|b| b.id.kind).collect();
        assert_eq!(found_kinds, expected_kinds);
        let bins: Vec<&Fed> = fed
            .iter()
            .filter(|f| !f.test && f.executable && f.kind == "bin")
            .collect();
        for f in &bins {
            let last = bins
                .iter()
                .rev()
                .find(|g| g.package == f.package && g.name == f.name)
                .expect("f itself matches");
            let want = PathBuf::from(format!("/exe/{}-{}", last.package, last.name));
            assert_eq!(build.package_bins[f.package][&f.name], want, "{f:?}");
        }
        let distinct: std::collections::BTreeSet<(&str, &str)> =
            bins.iter().map(|f| (f.package, f.name.as_str())).collect();
        let bin_count: usize = build.package_bins.values().map(BTreeMap::len).sum();
        assert_eq!(bin_count, distinct.len());
        assert_eq!(
            build.linked_paths,
            [PathBuf::from("/t/native"), PathBuf::from("/t/plain")]
        );
    }

    // Property: a `kind=` prefix is stripped from a linked path; anything else is kept whole.
    #[hegel::test]
    fn linked_paths_lose_their_kind_prefix(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&LINK_KINDS).print_as_debug());
        let path = tc.draw(gs::text().alphabet("/a=b").max_size(8));

        assert_eq!(linked_path(&format!("{kind}={path}")), PathBuf::from(&path));
        if !LINK_KINDS
            .iter()
            .any(|k| path.starts_with(&format!("{k}=")))
        {
            assert_eq!(linked_path(&path), PathBuf::from(&path));
        }
    }

    // Property: the kind order is Cargo's run order: lib, bin, test, bench, example.
    #[hegel::test]
    fn kinds_sort_in_cargos_order(tc: TestCase) {
        let mut kinds = tc.draw(
            gs::vecs(
                gs::sampled_from(&[
                    TargetKind::Example,
                    TargetKind::Test,
                    TargetKind::Lib,
                    TargetKind::Bench,
                    TargetKind::Bin,
                ])
                .print_as_debug(),
            )
            .max_size(6),
        );
        kinds.sort();
        let rank = |kind: &TargetKind| {
            ["lib", "bin", "test", "bench", "example"]
                .iter()
                .position(|name| *name == kind.as_str())
        };
        assert!(
            kinds
                .windows(2)
                .all(|pair| rank(&pair[0]) <= rank(&pair[1]))
        );
    }
}
