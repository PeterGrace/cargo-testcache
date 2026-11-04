//! Running a test binary as `cargo test` would, or through Cargo itself (design 4.4).
//!
//! `cargo test` runs a test binary from its package's directory, with the package's
//! `CARGO_PKG_*` variables, its build script's `OUT_DIR` and `rustc-env` variables,
//! `CARGO_BIN_EXE_<name>` for integration tests and benchmarks, and a dynamic library path that
//! finds the build's and the toolchain's shared libraries. The direct runner rebuilds exactly
//! that; the conformance test in `crates/cargo-testcache/tests/fixture.rs` compares it with
//! Cargo's. The via-cargo runner lets Cargo do it, at the cost of Cargo's freshness check for
//! every binary it runs.
//!
//! Test output is inherited, not captured: it reaches the terminal exactly as under
//! `cargo test`, colours included.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{self, Component, Path, PathBuf};
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

    /// Asks `rustc` (or `$RUSTC`) for the sysroot and host.
    ///
    /// `rustc` runs in this process's working directory, with its environment: rustup picks the
    /// toolchain from there, as it did for the `cargo` that started this tool (which also set
    /// `RUSTUP_TOOLCHAIN` for it).
    ///
    /// # Arguments
    ///
    /// * `cargo` - The cargo this tool runs, which test binaries get as `CARGO`. A bare name
    ///   (no `$CARGO` was set) is replaced by the toolchain's own cargo when it has one, the
    ///   absolute path `cargo test` would give tests; a relative path is made absolute.
    ///
    /// # Returns
    ///
    /// The toolchain.
    ///
    /// # Errors
    ///
    /// When `rustc` cannot run, fails, or prints something unexpected, or when a relative
    /// `cargo` cannot be made absolute.
    pub fn detect(cargo: &Path) -> Result<Self> {
        const PRINT_ARGS: [&str; 4] = ["--print", "sysroot", "--print", "host-tuple"];
        let rustc_var = env::var_os("RUSTC");
        // Built only when an error needs it: `$RUSTC` is named when it chose the compiler.
        let describe = || match &rustc_var {
            Some(rustc) => format!(
                "$RUSTC ({}) {}",
                rustc.to_string_lossy(),
                PRINT_ARGS.join(" ")
            ),
            None => format!("rustc {}", PRINT_ARGS.join(" ")),
        };
        let rustc = rustc_var.as_deref().unwrap_or(OsStr::new("rustc"));
        let output = Command::new(rustc)
            .args(PRINT_ARGS)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|source| Error::io(format!("running {}", describe()))(source))?;
        if !output.status.success() {
            return Err(Error::CommandFailed {
                command: describe(),
                status: output.status,
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let (Some(sysroot), Some(host)) = (lines.next(), lines.next()) else {
            return Err(Error::UnexpectedOutput {
                command: describe(),
                output: text.into_owned(),
            });
        };
        let sysroot = PathBuf::from(sysroot);
        let cargo = resolve_cargo(cargo, &sysroot)?;
        Ok(Self::new(sysroot, host.to_owned(), cargo))
    }

    /// The target triple the test binaries are compiled for, as far as the arguments tell:
    /// `--target`'s value when it is a plain triple, else the host. When `--target` is repeated,
    /// only the last one counts. A target given as a `.json` specification counts as the host,
    /// and so does one set with `CARGO_BUILD_TARGET` or `build.target`. A binary's own triple is
    /// better read from where it was built, which `library_path` does.
    ///
    /// # Arguments
    ///
    /// * `cargo_args` - The user's cargo arguments.
    ///
    /// # Returns
    ///
    /// The triple.
    pub fn target<'a>(&'a self, cargo_args: &'a CargoArgs) -> &'a str {
        cargo_args
            .value("--target")
            .and_then(OsStr::to_str)
            .filter(|value| {
                !value.is_empty() && !value.ends_with(".json") && !value.contains(['/', '\\'])
            })
            .unwrap_or(&self.host)
    }

    /// `<sysroot>/lib/rustlib/<target>/lib`, where the standard library's dynamic libraries
    /// for `target` live.
    fn target_libdir(&self, target: &str) -> PathBuf {
        self.sysroot
            .join("lib")
            .join("rustlib")
            .join(target)
            .join("lib")
    }
}

/// The `CARGO` value for test binaries. An absolute path is kept; a relative path with more than
/// one component is made absolute against the working directory, since the binaries run from
/// their package's directory; a bare name becomes `<sysroot>/bin/cargo` when that file exists,
/// as rustup's `cargo` would report itself, and is kept otherwise.
fn resolve_cargo(cargo: &Path, sysroot: &Path) -> Result<PathBuf> {
    if cargo.is_absolute() {
        return Ok(cargo.to_path_buf());
    }
    if cargo.components().nth(1).is_some() {
        return path::absolute(cargo)
            .map_err(Error::io(format!("making {} absolute", cargo.display())));
    }
    let bundled = sysroot
        .join("bin")
        .join(format!("cargo{}", env::consts::EXE_SUFFIX));
    Ok(if bundled.is_file() {
        bundled
    } else {
        cargo.to_path_buf()
    })
}

/// Whether a runner is configured for the target being tested through
/// `CARGO_TARGET_<TRIPLE>_RUNNER` (the triple upper-cased, with `-` and `.` as `_`). The direct
/// runner cannot reproduce a runner, so the caller falls back to the via-cargo runner.
///
/// Known limitations: a runner set in `.cargo/config.toml` (`target.<triple>.runner` or
/// `target.'cfg(..)'.runner`) is not detected, and the triple is [`Toolchain::target`]'s, so
/// with several `--target` flags only the last one's runner is checked.
///
/// # Arguments
///
/// * `toolchain` - The toolchain, for the host triple.
/// * `cargo_args` - The user's cargo arguments, for `--target`.
///
/// # Returns
///
/// `true` when the variable is set in this process's environment.
pub fn configured_runner(toolchain: &Toolchain, cargo_args: &CargoArgs) -> bool {
    configured_runner_in(toolchain, cargo_args, |name| env::var_os(name))
}

/// [`configured_runner`] with the environment given as a lookup function, for tests.
fn configured_runner_in(
    toolchain: &Toolchain,
    cargo_args: &CargoArgs,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let triple = toolchain
        .target(cargo_args)
        .to_ascii_uppercase()
        .replace(['-', '.'], "_");
    lookup(&format!("CARGO_TARGET_{triple}_RUNNER")).is_some()
}

/// Everything the direct runner needs besides the binary.
#[derive(Debug, Clone)]
pub struct RunContext<'a> {
    /// The workspace.
    pub workspace: &'a Workspace,
    /// What was built (package bins, build scripts' output).
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
    ///
    /// # Returns
    ///
    /// `true` for exit status 0; `false` for any other status or a signal.
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
/// Name and value pairs, to set on top of this process's environment, in order (a later pair
/// wins over an earlier one with the same name). The package's own build script's `OUT_DIR`
/// and `rustc-env` variables come first, so Cargo's own variables win a clash, as in Cargo,
/// with one exception: when the script sets [`LIBRARY_PATH_VAR`], its value replaces the whole
/// library path, as it does under `cargo test`. A dependency's `rustc-env` variables are not
/// set.
///
/// # Errors
///
/// [`Error::LibraryPath`] when a directory contains the path-list separator.
pub fn binary_env(
    binary: &TestBinary,
    package: &Package,
    context: &RunContext<'_>,
) -> Result<Vec<(OsString, OsString)>> {
    let mut vars: Vec<(OsString, OsString)> = Vec::new();
    let own_script = context.build.build_scripts.get(&binary.id.package_id);
    if let Some(script) = own_script {
        vars.push(("OUT_DIR".into(), script.out_dir.clone().into()));
        vars.extend(
            script
                .env
                .iter()
                .map(|(name, value)| (name.into(), value.into())),
        );
    }
    vars.extend(
        package
            .env()
            .iter()
            .map(|(name, value)| (OsString::from(name), value.clone())),
    );
    vars.push(("CARGO".into(), context.toolchain.cargo.clone().into()));
    // A "let chain": the block runs only when the kind spawns package bins *and* the package
    // has some, binding `bins` on the way, like Python's `if cond and (bins := get(...)):`.
    if binary.id.kind.spawns_package_bins()
        && let Some(bins) = context.build.package_bins.get(&binary.id.package_id)
    {
        vars.extend(
            bins.iter()
                .map(|(name, path)| (format!("CARGO_BIN_EXE_{name}").into(), path.into())),
        );
    }
    // `is_some_and` is `own_script is not None and <test>(own_script)`.
    let script_sets_library_path = own_script
        .is_some_and(|script| script.env.iter().any(|(name, _)| name == LIBRARY_PATH_VAR));
    if !script_sets_library_path {
        vars.push((LIBRARY_PATH_VAR.into(), library_path(binary, context)?));
    }
    Ok(vars)
}

/// The dynamic library path `cargo test` gives a binary: the build scripts' library
/// directories inside the binary's artifact directory (sorted, without duplicates), the
/// artifact directory, the executable's directory, the toolchain's target libraries, then the
/// inherited value.
fn library_path(binary: &TestBinary, context: &RunContext<'_>) -> Result<OsString> {
    let artifact_dir = context.workspace.artifact_dir(&binary.executable);
    // A `BTreeSet` is a sorted set, so it orders and deduplicates the way Cargo's does. With no
    // artifact directory (`None`), `iter()` yields nothing and no linked path is kept.
    let linked: BTreeSet<&Path> = artifact_dir
        .iter()
        .flat_map(|dir| {
            context
                .build
                .linked_paths
                .iter()
                .map(PathBuf::as_path)
                // `filter` lends its closure a reference to each item, so `path` is a `&&Path`;
                // method calls see through both references (auto-deref). `move` copies the
                // `dir` reference into the closure, which outlives this call to `flat_map`'s.
                .filter(move |path| path.starts_with(dir))
        })
        .collect();
    let mut dirs: Vec<PathBuf> = linked.into_iter().map(Path::to_path_buf).collect();
    dirs.extend(artifact_dir);
    dirs.extend(binary.executable.parent().map(Path::to_path_buf));
    let toolchain = context.toolchain;
    let triple = compiled_triple(
        context.workspace.build_dir(),
        &binary.executable,
        &toolchain.host,
    );
    dirs.push(toolchain.target_libdir(triple));
    match &context.inherited_library_path {
        Some(inherited) => dirs.extend(env::split_paths(inherited)),
        None => dirs.extend(default_library_path()),
    }
    Ok(env::join_paths(dirs)?)
}

/// The triple a test executable was compiled for, read from where Cargo put it:
/// `<build-dir>/<triple>/<profile>/deps/<name>` under `--target`, else
/// `<build-dir>/<profile>/deps/<name>` for the host. This holds with several `--target` flags,
/// where the arguments alone cannot say which binary is which.
fn compiled_triple<'a>(build_dir: &Path, executable: &'a Path, host: &'a str) -> &'a str {
    let Some(relative) = executable
        .parent()
        .and_then(Path::parent)
        .and_then(|profile_dir| profile_dir.strip_prefix(build_dir).ok())
    else {
        return host;
    };
    let mut components = relative.components();
    match (components.next(), components.next(), components.next()) {
        // Exactly two components: `<triple>/<profile>`. `to_str` is `None` for non-UTF-8.
        (Some(Component::Normal(triple)), Some(_), None) => triple.to_str().unwrap_or(host),
        _ => host,
    }
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
    // The closure builds the message only if starting the command failed.
    let status = command.status().map_err(|source| {
        Error::io(format!(
            "running {}",
            command.get_program().to_string_lossy()
        ))(source)
    })?;
    Ok(RunOutcome {
        status,
        duration: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, HashMap};

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::{BuildScriptOutput, TargetId};
    use crate::workspace::testing::{ID, metadata};

    const KINDS: [TargetKind; 5] = [
        TargetKind::Lib,
        TargetKind::Bin,
        TargetKind::Test,
        TargetKind::Bench,
        TargetKind::Example,
    ];

    /// A dependency's package id: its build script output must not reach `ID`'s binaries.
    const DEPENDENCY: &str = "path+file:///ws/crates/dep#0.1.0";

    /// The workspace fixture with a separate build directory at /cache/build.
    fn workspace() -> Workspace {
        Workspace::from_json(&metadata("0.1.0", Some("/cache/build"))).expect("valid metadata")
    }

    /// A build with one package bin and the given linked paths and build scripts.
    fn build(linked_paths: Vec<PathBuf>, own_script: Option<BuildScriptOutput>) -> Build {
        let dependency_script = BuildScriptOutput {
            out_dir: PathBuf::from("/cache/build/debug/build/dep-1/out"),
            env: vec![("DEP_ENV".to_owned(), "1".to_owned())],
        };
        let mut build_scripts = HashMap::from([(DEPENDENCY.to_owned(), dependency_script)]);
        build_scripts.extend(own_script.map(|script| (ID.to_owned(), script)));
        Build {
            binaries: Vec::new(),
            package_bins: HashMap::from([(
                ID.to_owned(),
                BTreeMap::from([("tool".to_owned(), PathBuf::from("/ws/target/debug/tool"))]),
            )]),
            linked_paths,
            build_scripts,
        }
    }

    /// A test binary in the build directory; `triple_dir` is the `<triple>` directory Cargo
    /// adds under `--target`.
    fn binary(kind: TargetKind, triple_dir: Option<&str>) -> TestBinary {
        let base = triple_dir.map_or(PathBuf::from("/cache/build"), |triple| {
            Path::new("/cache/build").join(triple)
        });
        TestBinary {
            id: TargetId {
                package_id: ID.to_owned(),
                kind,
                name: "t".to_owned(),
            },
            src_path: PathBuf::from("/ws/crates/a/tests/t.rs"),
            executable: base.join("debug/deps/t-0123"),
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
        // `rev()`: a later pair wins, as with `Command::envs`.
        env.iter()
            .rev()
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

    /// How `--target` is given: not at all, as a plain triple, or as a `.json` target
    /// specification (which counts as the host).
    #[derive(Debug, Clone, Copy)]
    enum Compile {
        Host,
        Triple,
        Spec,
    }

    const COMPILES: [Compile; 3] = [Compile::Host, Compile::Triple, Compile::Spec];

    /// Where a drawn linked path lies.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Place {
        ArtifactDir,
        TargetDirElsewhere,
        BuildDir,
        Outside,
    }

    const PLACES: [Place; 4] = [
        Place::ArtifactDir,
        Place::TargetDirElsewhere,
        Place::BuildDir,
        Place::Outside,
    ];

    // Property: CARGO_BIN_EXE_* is set exactly for integration tests and benchmarks; the
    // package's variables and CARGO are always set; OUT_DIR and the rustc-env variables are
    // the package's own build script's, never a dependency's; the library path is the own
    // script's value when it sets one, else the linked paths inside the binary's artifact
    // directory (sorted, deduplicated), the artifact directory, the deps directory, the
    // standard libraries of the triple the binary was built for, then what the variable held
    // before.
    #[hegel::test]
    fn the_environment_is_cargos(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        // The `<triple>` directory Cargo builds the binary under: none for the host. Binaries for
        // different triples can come from one build with several `--target` flags.
        let triple_dir =
            tc.draw(gs::sampled_from(&[None, Some("tt"), Some("uu")]).print_as_debug());
        let inherited = tc.draw(gs::booleans());
        let script_sets_library_path = tc.draw(gs::booleans());
        let artifact_dir = match triple_dir {
            None => PathBuf::from("/ws/target/debug"),
            Some(triple) => Path::new("/ws/target").join(triple).join("debug"),
        };
        let drawn: Vec<(Place, String)> = tc.draw(
            gs::vecs(gs::tuples2(
                gs::sampled_from(&PLACES).print_as_debug(),
                gs::text().alphabet("ab").min_size(1).max_size(2),
            ))
            .max_size(8),
        );
        let place_path = |place: Place, name: &str| match place {
            Place::ArtifactDir => artifact_dir.join("build").join(name).join("out"),
            Place::TargetDirElsewhere => Path::new("/ws/target/elsewhere").join(name),
            Place::BuildDir => Path::new("/cache/build/debug/build").join(name),
            Place::Outside => Path::new("/opt").join(name),
        };
        let linked_paths: Vec<PathBuf> = drawn
            .iter()
            .map(|(place, name)| place_path(*place, name))
            .collect();
        let own_script = tc.draw(gs::booleans()).then(|| {
            let mut env = vec![("OWN_ENV".to_owned(), "yes".to_owned())];
            if script_sets_library_path {
                env.push((LIBRARY_PATH_VAR.to_owned(), "/script/lib".to_owned()));
            }
            BuildScriptOutput {
                out_dir: PathBuf::from("/cache/build/debug/build/a-1/out"),
                env,
            }
        });
        let (workspace, toolchain) = (workspace(), toolchain());
        let build = build(linked_paths, own_script.clone());
        let context = RunContext {
            workspace: &workspace,
            build: &build,
            toolchain: &toolchain,
            inherited_library_path: inherited
                .then(|| env::join_paths(["/x", "/y"]).expect("joinable")),
        };
        let package = workspace.package(ID).expect("listed");

        let vars =
            binary_env(&binary(kind, triple_dir), package, &context).expect("an environment");

        assert_eq!(
            get(&vars, "CARGO_BIN_EXE_tool").is_some(),
            kind.spawns_package_bins()
        );
        assert_eq!(get(&vars, "CARGO"), Some(OsStr::new("/c/cargo")));
        assert_eq!(get(&vars, "CARGO_PKG_NAME"), Some(OsStr::new("a")));
        assert_eq!(
            get(&vars, "OUT_DIR"),
            own_script.as_ref().map(|script| script.out_dir.as_os_str())
        );
        assert_eq!(
            get(&vars, "OWN_ENV"),
            own_script.as_ref().map(|_| OsStr::new("yes"))
        );
        assert_eq!(get(&vars, "DEP_ENV"), None);
        let library_path = get(&vars, LIBRARY_PATH_VAR).expect("set");
        if own_script.is_some() && script_sets_library_path {
            // The package's own script replaces the whole library path, as under Cargo.
            assert_eq!(library_path, OsStr::new("/script/lib"));
            return;
        }
        let library_path: Vec<PathBuf> = env::split_paths(library_path).collect();
        let mut expected: Vec<PathBuf> = drawn
            .iter()
            .filter(|(place, _)| *place == Place::ArtifactDir)
            .map(|(place, name)| place_path(*place, name))
            .collect::<BTreeSet<PathBuf>>()
            .into_iter()
            .collect();
        expected.push(artifact_dir.clone());
        expected.push(
            binary(kind, triple_dir)
                .executable
                .parent()
                .expect("in a directory")
                .to_path_buf(),
        );
        expected.push(
            Path::new("/sys/lib/rustlib")
                .join(triple_dir.unwrap_or("h"))
                .join("lib"),
        );
        if inherited {
            expected.extend(["/x", "/y"].map(PathBuf::from));
        } else {
            expected.extend(default_library_path());
        }
        assert_eq!(library_path, expected);
    }

    // Property: with no artifact directory (an executable outside the build directory), no
    // linked path is kept.
    #[hegel::test]
    fn no_artifact_dir_keeps_no_linked_paths(tc: TestCase) {
        let name = tc.draw(gs::text().alphabet("ab").min_size(1).max_size(2));
        let (workspace, toolchain) = (workspace(), toolchain());
        let build = build(vec![Path::new("/ws/target/debug/build").join(&name)], None);
        let context = RunContext {
            workspace: &workspace,
            build: &build,
            toolchain: &toolchain,
            inherited_library_path: Some(OsString::new()),
        };
        let mut outside = binary(TargetKind::Lib, None);
        outside.executable = Path::new("/elsewhere/deps").join(&name);

        let path = library_path(&outside, &context).expect("a library path");

        let dirs: Vec<PathBuf> = env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect();
        assert_eq!(
            dirs,
            [
                PathBuf::from("/elsewhere/deps"),
                PathBuf::from("/sys/lib/rustlib/h/lib")
            ]
        );
    }

    // Property: the direct command runs the executable from the package root with the test
    // arguments and exactly the environment `binary_env` gives.
    #[hegel::test]
    fn direct_commands_run_from_the_package_root(tc: TestCase) {
        let kind = tc.draw(gs::sampled_from(&KINDS).print_as_debug());
        let own_script = tc.draw(gs::booleans()).then(|| BuildScriptOutput {
            out_dir: PathBuf::from("/cache/build/debug/build/a-1/out"),
            env: vec![("OWN_ENV".to_owned(), "yes".to_owned())],
        });
        let test_args = draw_test_args(&tc);
        let (workspace, toolchain) = (workspace(), toolchain());
        let build = build(Vec::new(), own_script);
        let context = RunContext {
            workspace: &workspace,
            build: &build,
            toolchain: &toolchain,
            inherited_library_path: None,
        };
        let binary = binary(kind, None);

        let command = direct_command(&binary, &context, &test_args).expect("a command");

        assert_eq!(command.get_program(), binary.executable.as_os_str());
        assert_eq!(command.get_current_dir(), Some(Path::new("/ws/crates/a")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            test_args
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
        let package = workspace.package(ID).expect("listed");
        let expected: BTreeMap<OsString, Option<OsString>> = binary_env(&binary, package, &context)
            .expect("an environment")
            .into_iter()
            .map(|(name, value)| (name, Some(value)))
            .collect();
        let envs: BTreeMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        assert_eq!(envs, expected);
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

        let command = via_cargo_command(
            Path::new("cargo"),
            &binary(kind, None),
            &cargo_args,
            &test_args,
        );

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

    // Property: a runner counts exactly when CARGO_TARGET_<TRIPLE>_RUNNER is set for the
    // triple being compiled for (a plain `--target`, else the host), spelled upper-case with
    // `-` and `.` as `_`; a runner for another triple does not count.
    #[hegel::test]
    fn runners_are_found_for_the_compiled_triple(tc: TestCase) {
        let word = || gs::text().alphabet("ab6").min_size(1).max_size(3);
        let draw_triple = || -> (String, String) {
            let words: Vec<String> = tc.draw(gs::vecs(word()).min_size(1).max_size(3));
            let separators: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(2).max_size(2));
            let mut triple = words[0].clone();
            for (index, part) in words.iter().enumerate().skip(1) {
                triple.push(if separators[index - 1] { '-' } else { '.' });
                triple.push_str(part);
            }
            let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
            (triple, upper.join("_"))
        };
        let (host, host_var) = draw_triple();
        let (target, target_var) = draw_triple();
        let how = tc.draw(gs::sampled_from(&COMPILES).print_as_debug());
        let set_for_compiled = tc.draw(gs::booleans());
        let set_for_other = tc.draw(gs::booleans());
        let cargo_args = CargoArgs::new(match how {
            Compile::Host => Vec::new(),
            Compile::Triple => vec!["--target".into(), target.clone().into()],
            Compile::Spec => vec![format!("--target=specs/{target}.json").into()],
        });
        let (compiled_var, other_var) = match how {
            Compile::Host | Compile::Spec => (host_var, target_var),
            Compile::Triple => (target_var, host_var),
        };
        let mut vars: HashMap<String, OsString> = HashMap::new();
        if set_for_other {
            vars.insert(format!("CARGO_TARGET_{other_var}_RUNNER"), "wine".into());
        }
        if set_for_compiled {
            vars.insert(format!("CARGO_TARGET_{compiled_var}_RUNNER"), "qemu".into());
        }
        let toolchain = Toolchain::new(PathBuf::from("/sys"), host, PathBuf::from("cargo"));

        let found = configured_runner_in(&toolchain, &cargo_args, |name| vars.get(name).cloned());

        let same_var = compiled_var == other_var;
        assert_eq!(found, set_for_compiled || (same_var && set_for_other));
    }

    // Property: an absolute cargo is kept, a relative path with several components is made
    // absolute, and a bare name becomes the toolchain's own cargo when it has one.
    #[hegel::test(test_cases = 20)]
    fn cargo_paths_are_resolved(tc: TestCase) {
        let name = tc.draw(gs::text().alphabet("abc").min_size(1).max_size(4));
        let bundled_exists = tc.draw(gs::booleans());
        let sysroot = tempfile::tempdir().expect("a temporary directory");
        let bundled = sysroot
            .path()
            .join("bin")
            .join(format!("cargo{}", env::consts::EXE_SUFFIX));
        if bundled_exists {
            std::fs::create_dir_all(bundled.parent().expect("in bin")).expect("bin created");
            std::fs::write(&bundled, b"").expect("bundled cargo written");
        }
        let absolute = Path::new("/opt").join(&name);
        let relative = Path::new("tools").join(&name);
        let bare = PathBuf::from(&name);

        let resolved = |cargo: &Path| resolve_cargo(cargo, sysroot.path()).expect("resolved");

        assert_eq!(resolved(&absolute), absolute);
        let made_absolute = resolved(&relative);
        assert!(made_absolute.is_absolute(), "{made_absolute:?}");
        assert!(made_absolute.ends_with(&relative), "{made_absolute:?}");
        assert_eq!(
            resolved(&bare),
            if bundled_exists {
                bundled
            } else {
                bare.clone()
            }
        );
    }
}
