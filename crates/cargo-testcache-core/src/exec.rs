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
        Self {
            sysroot,
            host,
            cargo,
        }
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
            return Err(Error::CommandFailed {
                command: COMMAND.to_owned(),
                status: output.status,
            });
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
        let bundled = sysroot
            .join("bin")
            .join(format!("cargo{}", env::consts::EXE_SUFFIX));
        let cargo = if cargo.is_absolute() || !bundled.is_file() {
            cargo.to_path_buf()
        } else {
            bundled
        };
        Ok(Self::new(sysroot, host.to_owned(), cargo))
    }

    /// `<sysroot>/lib/rustlib/<host>/lib`, where the standard library's dynamic libraries live.
    fn host_libdir(&self) -> PathBuf {
        self.sysroot
            .join("lib")
            .join("rustlib")
            .join(&self.host)
            .join("lib")
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
    let mut vars: Vec<(OsString, OsString)> = package
        .env()
        .iter()
        .map(|(name, value)| (OsString::from(name), value.clone()))
        .collect();
    vars.push(("CARGO".into(), context.toolchain.cargo.clone().into()));
    if binary.id.kind.spawns_package_bins()
        && let Some(bins) = context.build.package_bins.get(&binary.id.package_id)
    {
        vars.extend(
            bins.iter()
                .map(|(name, path)| (format!("CARGO_BIN_EXE_{name}").into(), path.into())),
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
    let mut dirs: Vec<PathBuf> = context
        .build
        .linked_paths
        .iter()
        .filter(in_target)
        .cloned()
        .collect();
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
    home_lib
        .into_iter()
        .chain(["/usr/local/lib", "/usr/lib"].map(PathBuf::from))
        .collect()
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
        .ok_or_else(|| Error::UnknownPackage {
            id: binary.id.package_id.clone(),
        })?;
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
    Ok(RunOutcome {
        status,
        duration: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, HashMap};
    use std::ffi::OsStr;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetId;
    use crate::workspace::testing::{ID, metadata};

    const KINDS: [TargetKind; 5] = [
        TargetKind::Lib,
        TargetKind::Bin,
        TargetKind::Test,
        TargetKind::Bench,
        TargetKind::Example,
    ];

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
            id: TargetId {
                package_id: ID.to_owned(),
                kind,
                name: "t".to_owned(),
            },
            src_path: PathBuf::from("/ws/crates/a/tests/t.rs"),
            executable: PathBuf::from("/cache/build/debug/deps/t-0123"),
        }
    }

    fn toolchain() -> Toolchain {
        Toolchain::new(
            PathBuf::from("/sys"),
            "h".to_owned(),
            PathBuf::from("/c/cargo"),
        )
    }

    fn get<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }

    /// Test arguments as a process could receive them: an OS argument is a C string, so it
    /// never contains NUL (`Command` would replace such an argument with a placeholder).
    fn draw_test_args(tc: &TestCase) -> Vec<OsString> {
        tc.draw(gs::vecs(gs::text().exclude_characters("\0").max_size(6)).max_size(3))
            .into_iter()
            .map(Into::into)
            .collect()
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

        assert_eq!(
            get(&vars, "CARGO_BIN_EXE_tool").is_some(),
            kind.spawns_package_bins()
        );
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
        let test_args = draw_test_args(&tc);
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
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            test_args
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
    }

    // Property: the via-cargo command selects exactly the binary's target, keeps the other
    // cargo arguments, and passes the test arguments after `--`.
    #[hegel::test]
    fn via_cargo_commands_select_one_target(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        let test_args = draw_test_args(&tc);
        let cargo_args = CargoArgs::new(
            ["--release", "-p", "other", "--test", "x", "--features", "f"]
                .map(OsString::from)
                .into(),
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
