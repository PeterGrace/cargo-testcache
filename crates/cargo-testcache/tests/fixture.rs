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
    /// Holds the copy, the run log, the target and build directories; deleted on drop.
    temp: TempDir,
    /// The copied workspace root, `<temp>/ws`.
    root: PathBuf,
    /// Whether Cargo gets a build directory apart from the target directory.
    separate_build_dir: bool,
}

impl Fixture {
    /// Copies the fixture workspace into a new temporary directory, with a separate build
    /// directory.
    fn new() -> Self {
        Self::with_build_dir(true)
    }

    /// Copies the fixture workspace into a new temporary directory.
    ///
    /// `separate_build_dir` gives Cargo a build directory apart from the target directory, so
    /// test executables and build-script output live elsewhere than the `bin` executables.
    fn with_build_dir(separate_build_dir: bool) -> Self {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path().join("ws");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ws");
        copy_tree(&source, &root);
        fs::create_dir(temp.path().join("fail")).expect("create the fail directory");
        Self {
            temp,
            root,
            separate_build_dir,
        }
    }

    /// The file every fixture test appends its binary's name to.
    fn run_log(&self) -> PathBuf {
        self.temp.path().join("runs.log")
    }

    /// `cargo <args>` in the fixture, with a controlled environment.
    fn cargo(&self, args: &[&str], extra_env: &[(&str, &OsStr)]) -> Output {
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_cargo-testcache"))
            .parent()
            .expect("the binary has a directory")
            .to_path_buf();
        let inherited_path = env::var_os("PATH").unwrap_or_default();
        let path =
            env::join_paths(std::iter::once(bin_dir).chain(env::split_paths(&inherited_path)))
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
            .env("FIXTURE_RUN_LOG", self.run_log())
            .env("FIXTURE_FAIL_DIR", self.temp.path().join("fail"))
            .envs(extra_env.iter().copied());
        if self.separate_build_dir {
            command.env("CARGO_BUILD_BUILD_DIR", self.temp.path().join("build"));
        }
        command.output().expect("run cargo")
    }

    /// `cargo testcache <args>`.
    fn testcache(&self, args: &[&str], extra_env: &[(&str, &OsStr)]) -> Output {
        let args: Vec<&str> = std::iter::once("testcache")
            .chain(args.iter().copied())
            .collect();
        self.cargo(&args, extra_env)
    }

    /// `cargo testcache <args>`, which must succeed.
    fn testcache_ok(&self, args: &[&str]) -> Output {
        let output = self.testcache(args, &[]);
        assert!(
            output.status.success(),
            "cargo testcache {args:?} failed:\n{}",
            stderr(&output)
        );
        output
    }

    /// The binaries that ran since the last call.
    fn take_runs(&self) -> BTreeSet<String> {
        let runs = fs::read_to_string(self.run_log()).unwrap_or_default();
        // A missing log only means nothing ran.
        let _ = fs::remove_file(self.run_log());
        runs.lines().map(str::to_owned).collect()
    }

    /// Overwrites a file in the fixture copy.
    fn write(&self, relative: &str, contents: &str) {
        fs::write(self.root.join(relative), contents).expect("write a fixture file");
    }

    /// Replaces the one occurrence of `from` in a fixture file with `to`.
    fn replace(&self, relative: &str, from: &str, to: &str) {
        let path = self.root.join(relative);
        let contents = fs::read_to_string(&path).expect("read a fixture file");
        assert_eq!(
            contents.matches(from).count(),
            1,
            "{relative} has {from:?} once"
        );
        fs::write(&path, contents.replace(from, to)).expect("write a fixture file");
    }

    /// Appends to a fixture file.
    fn append(&self, relative: &str, text: &str) {
        let path = self.root.join(relative);
        let contents = fs::read_to_string(&path).expect("read a fixture file");
        fs::write(&path, contents + text).expect("write a fixture file");
    }

    /// Sets the `STAMP` constant in a fixture source file, which must define exactly one.
    fn set_stamp(&self, relative: &str, stamp: u32) {
        let path = self.root.join(relative);
        let source = fs::read_to_string(&path).expect("read the source");
        let definitions = source
            .lines()
            .filter(|line| line.contains("STAMP: &str = "))
            .count();
        assert_eq!(definitions, 1, "{relative} must define STAMP exactly once");
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

    /// Makes the named test binary fail (`fail == true`) or pass again.
    fn fail(&self, name: &str, fail: bool) {
        let path = self.temp.path().join("fail").join(name);
        if fail {
            fs::write(path, "").expect("create the fail file");
        } else {
            fs::remove_file(path).expect("remove the fail file");
        }
    }
}

/// Copies a directory tree, leaving out any `target` directory and `Cargo.lock` left behind by
/// building the fixture in place.
fn copy_tree(from: &Path, to: &Path) {
    for entry in WalkDir::new(from)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != "target" && entry.file_name() != "Cargo.lock")
    {
        let entry = entry.expect("walk the fixture");
        let target = to.join(entry.path().strip_prefix(from).expect("inside the fixture"));
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).expect("create a directory");
        } else {
            fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

/// A command's stderr as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Both variants of a property, in the drawn order: as given, or `reversed`.
fn in_order<T>(mut variants: [T; 2], reversed: bool) -> [T; 2] {
    if reversed {
        variants.reverse();
    }
    variants
}

/// A set of owned binary names.
fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

// Property (9.1): the first run runs every binary and the second skips every one, with either
// runner. Both runners are checked in every case, each on a fresh fixture, in a drawn order.
#[hegel::test(test_cases = 2)]
fn the_second_run_skips_every_binary(tc: TestCase) {
    let reversed = tc.draw(gs::booleans());
    for via_cargo in in_order([false, true], reversed) {
        let runner: &[&str] = if via_cargo { &["--via-cargo"] } else { &[] };
        let fixture = Fixture::new();

        fixture.testcache_ok(runner);
        assert_eq!(
            fixture.take_runs(),
            names(&ALL),
            "first run, via cargo: {via_cargo}"
        );

        let second = fixture.testcache_ok(runner);
        assert_eq!(
            fixture.take_runs(),
            BTreeSet::new(),
            "second run, via cargo: {via_cargo}"
        );
        assert!(stderr(&second).contains("0 run"), "{}", stderr(&second));
    }
}

// Property (9.2, 9.6): changing one crate re-runs exactly the binaries that depend on it; for
// `beta`, the integration test re-runs because the binary it spawns changed. Every crate is
// changed in every case, in turn on one fixture, so a derandomized run cannot miss one.
#[hegel::test(test_cases = 2)]
fn changing_a_crate_reruns_only_its_dependents(tc: TestCase) {
    let stamp = tc.draw(gs::integers::<u32>().min_value(1));
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    let changes: [(&str, &[&str]); 3] = [
        ("alpha/src/lib.rs", &["alpha", "gamma", "reads_input"]),
        ("gamma/src/lib.rs", &["gamma"]),
        ("beta/src/main.rs", &["betatool", "spawns_tool"]),
    ];
    for (file, expected) in changes {
        fixture.set_stamp(file, stamp);
        let output = fixture.testcache_ok(&["--explain"]);

        assert_eq!(
            fixture.take_runs(),
            names(expected),
            "after changing {file}"
        );
        if file.starts_with("beta") {
            assert!(
                stderr(&output).contains("spawned bin betatool changed"),
                "{}",
                stderr(&output)
            );
        }
    }
}

// Property (9.3): changing a declared input, or a declared variable, re-runs every binary the
// declaration covers. Every kind of change is made in every case, in turn on one fixture.
#[hegel::test(test_cases = 2)]
fn changing_a_declared_input_reruns_what_it_covers(tc: TestCase) {
    let value = tc.draw(gs::integers::<u32>().min_value(1)).to_string();
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    fixture.write("data/value.txt", &value);
    fixture.testcache_ok(&[]);
    assert_eq!(
        fixture.take_runs(),
        names(&ALL),
        "after changing data/value.txt"
    );

    fixture.write("alpha/fixtures/extra.txt", &value);
    fixture.testcache_ok(&[]);
    assert_eq!(
        fixture.take_runs(),
        names(&["alpha", "reads_input"]),
        "after changing alpha/fixtures/extra.txt"
    );

    let output = fixture.testcache(&[], &[("FIXTURE_ENV", OsStr::new(&value))]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fixture.take_runs(),
        names(&ALL),
        "after setting FIXTURE_ENV"
    );
}

/// Where a fixture test puts a Cargo `[env]` setting.
#[derive(Debug, Clone, Copy)]
enum EnvSetting {
    /// In `.cargo/config.toml`.
    InConfig,
    /// In `.cargo/inc.toml`, which `.cargo/config.toml` includes.
    Included,
    /// As a `--config` argument.
    Argument,
}

// Property: an `[env]` table in Cargo's configuration reaches every test binary, as under
// `cargo test`, though the direct runner cannot apply it (the run falls back to Cargo), and
// changing it reruns every binary. Checked with the table in `.cargo/config.toml`, in a file
// that `.cargo/config.toml` includes (only that file changes), and given as `--config`, each
// on a fresh fixture, in a drawn order.
#[hegel::test(test_cases = 2)]
fn cargo_config_env_reaches_the_tests_and_is_keyed(tc: TestCase) {
    let rotation = tc.draw(gs::integers::<usize>().max_value(2));
    let first = tc.draw(gs::integers::<u32>()).to_string();
    let second = format!("{first}-changed");
    let mut settings = [
        EnvSetting::InConfig,
        EnvSetting::Included,
        EnvSetting::Argument,
    ];
    settings.rotate_left(rotation);
    for setting in settings {
        let fixture = Fixture::new();
        let run = |value: &str| {
            let line = format!("FIXTURE_CFG = \"{value}\"");
            let config_arg = format!("env.{line}");
            let table = format!("[env]\n{line}\n");
            fs::create_dir_all(fixture.root.join(".cargo")).expect("create .cargo");
            let args: Vec<&str> = match setting {
                EnvSetting::InConfig => {
                    fixture.write(".cargo/config.toml", &table);
                    Vec::new()
                }
                EnvSetting::Included => {
                    fixture.write(".cargo/config.toml", "include = [\"inc.toml\"]\n");
                    fixture.write(".cargo/inc.toml", &table);
                    Vec::new()
                }
                EnvSetting::Argument => vec!["--config", &config_arg],
            };
            let output = fixture.testcache(&args, &[("FIXTURE_CFG_EXPECTED", OsStr::new(value))]);
            assert!(
                output.status.success(),
                "{setting:?}, value {value}:\n{}",
                stderr(&output)
            );
            fixture.take_runs()
        };

        assert_eq!(run(&first), names(&ALL), "first run, {setting:?}");
        assert_eq!(run(&first), BTreeSet::new(), "unchanged, {setting:?}");
        assert_eq!(run(&second), names(&ALL), "changed, {setting:?}");
        // Some binaries run, so each runs through `cargo test` on its own.
        fixture.write("alpha/fixtures/extra.txt", &second);
        assert_eq!(
            run(&second),
            names(&["alpha", "reads_input"]),
            "one input changed, {setting:?}"
        );
        assert_eq!(run(&second), BTreeSet::new(), "recorded, {setting:?}");
    }
}

// Property: a target runner set in Cargo's configuration (for the host triple, for a `cfg`, or
// by `--config`) is applied to every test binary, which only Cargo can do: the run falls back
// to the via-cargo runner and says so. The runner is `env FIXTURE_CFG=<value>`, which runs the
// binary with a variable the fixture checks. Every form is checked in every case, each on a
// fresh fixture, in a drawn order.
#[hegel::test(test_cases = 2)]
fn a_configured_runner_is_applied_through_cargo(tc: TestCase) {
    let rotation = tc.draw(gs::integers::<usize>().max_value(2));
    let value = tc.draw(gs::integers::<u32>()).to_string();
    let host = host_triple();
    let mut variants = [
        (true, format!("\"{host}\"")),
        (true, "'cfg(all())'".to_owned()),
        (false, "\"cfg(all())\"".to_owned()),
    ];
    variants.rotate_left(rotation);
    for (in_file, target) in variants {
        let fixture = Fixture::new();
        let runner = format!("runner = [\"env\", \"FIXTURE_CFG={value}\"]");
        let config_arg = format!("target.{target}.{runner}");
        let args: Vec<&str> = if in_file {
            fs::create_dir_all(fixture.root.join(".cargo")).expect("create .cargo");
            fixture.write(
                ".cargo/config.toml",
                &format!("[target.{target}]\n{runner}\n"),
            );
            Vec::new()
        } else {
            vec!["--config", &config_arg]
        };
        let run = || {
            let output = fixture.testcache(&args, &[("FIXTURE_CFG_EXPECTED", OsStr::new(&value))]);
            assert!(
                output.status.success(),
                "target {target}, in a file: {in_file}:\n{}",
                stderr(&output)
            );
            (fixture.take_runs(), stderr(&output))
        };

        let (runs, first) = run();
        assert_eq!(runs, names(&ALL), "first run, target {target}");
        assert!(first.contains("using the via-cargo runner"), "{first}");
        assert_eq!(run().0, BTreeSet::new(), "second run, target {target}");
        // Some binaries run, so each runs through `cargo test` on its own.
        fixture.write("alpha/fixtures/extra.txt", &value);
        assert_eq!(
            run().0,
            names(&["alpha", "reads_input"]),
            "one input changed, target {target}"
        );
    }
}

/// The host's target triple, from `rustc`.
fn host_triple() -> String {
    let output = Command::new("rustc")
        .args(["--print", "host-tuple"])
        .output()
        .expect("run rustc");
    assert!(output.status.success(), "rustc --print host-tuple failed");
    String::from_utf8(output.stdout)
        .expect("the triple is UTF-8")
        .trim()
        .to_owned()
}

// Property: under `--via-cargo`, a binary that Cargo builds differently when its target is
// selected alone runs but is not recorded, so it runs again; one Cargo builds the same way is
// recorded. Here gamma's dev-dependency turns on probe's feature `x`, which feature unification
// applies to the whole workspace's build but not to `cargo test -p alpha --test reads_input`
// or `-p beta --test spawns_tool`; gamma's own lib test gets the feature either way. Each of
// the three is changed alone, in turn on one fixture, in a drawn order.
#[hegel::test(test_cases = 2)]
fn via_cargo_records_only_the_binary_it_hashed(tc: TestCase) {
    let rotation = tc.draw(gs::integers::<usize>().max_value(2));
    let stamp = tc.draw(gs::integers::<u32>());
    let fixture = Fixture::new();
    fixture.append("probe/Cargo.toml", "\n[features]\nx = []\n");
    fixture.replace(
        "gamma/Cargo.toml",
        "probe = { path = \"../probe\" }",
        "probe = { path = \"../probe\", features = [\"x\"] }",
    );
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), names(&ALL), "the first run");

    let mut variants = [
        ("alpha/tests/reads_input.rs", "reads_input", false),
        ("beta/tests/spawns_tool.rs", "spawns_tool", false),
        ("gamma/src/lib.rs", "gamma", true),
    ];
    variants.rotate_left(rotation);
    for (file, name, recorded) in variants {
        // A new test changes this one binary and no other.
        fixture.append(
            file,
            &format!("\n#[test]\nfn stamp_{stamp}() {{\n    println!(\"{stamp}\");\n}}\n"),
        );
        let output = fixture.testcache_ok(&["--via-cargo"]);
        assert_eq!(fixture.take_runs(), names(&[name]), "after changing {file}");
        assert_eq!(
            stderr(&output).contains("Not recorded"),
            !recorded,
            "{}",
            stderr(&output)
        );

        let expected = if recorded { names(&[]) } else { names(&[name]) };
        fixture.testcache_ok(&["--via-cargo"]);
        assert_eq!(fixture.take_runs(), expected, "via cargo again, {name}");
        // The direct runner runs the hashed executable, so it records the pass.
        fixture.testcache_ok(&[]);
        assert_eq!(fixture.take_runs(), expected, "directly, {name}");
        fixture.testcache_ok(&[]);
        assert_eq!(fixture.take_runs(), BTreeSet::new(), "recorded, {name}");
    }
}

// Property: a package's `inputs` glob may reach outside the package with `..` and spell its
// directories with `./`, and still covers the files it names: with only alpha's table naming
// `data/`, changing `data/value.txt` reruns alpha's binaries and no other, and so does changing
// alpha's fixture. Every spelling is checked in every case, in turn on one fixture, in a drawn
// order. A glob that matches no file is warned about.
#[hegel::test(test_cases = 2)]
fn package_inputs_may_climb_out_of_the_package(tc: TestCase) {
    let rotation = tc.draw(gs::integers::<usize>().max_value(2));
    let stamp = tc.draw(gs::integers::<u32>());
    let fixture = Fixture::new();
    fixture.replace("Cargo.toml", "inputs = [\"data/**\"]", "inputs = []");
    let mut spellings = [
        ["../data/**", "./fixtures/**"],
        ["./../data/**", "fixtures/**"],
        ["../alpha/../data/**", "fixtures/./**"],
    ];
    spellings.rotate_left(rotation);
    let mut table = "inputs = [\"fixtures/**\"]".to_owned();
    for (index, spelling) in spellings.iter().enumerate() {
        let next = format!("inputs = [\"{}\", \"{}\"]", spelling[0], spelling[1]);
        fixture.replace("alpha/Cargo.toml", &table, &next);
        table = next;
        let settled = fixture.testcache_ok(&[]);
        assert!(
            !stderr(&settled).contains("matched no file"),
            "{spelling:?}: {}",
            stderr(&settled)
        );
        fixture.take_runs();

        for file in ["data/value.txt", "alpha/fixtures/extra.txt"] {
            fixture.write(file, &format!("{stamp}-{index}"));
            fixture.testcache_ok(&[]);
            assert_eq!(
                fixture.take_runs(),
                names(&["alpha", "reads_input"]),
                "{spelling:?}, after changing {file}"
            );
        }
    }
    // A glob that matches nothing is warned about, naming its table.
    fixture.replace("alpha/Cargo.toml", &table, "inputs = [\"nothing/**\"]");
    let output = fixture.testcache_ok(&[]);
    assert!(
        stderr(&output).contains(
            "inputs glob \"nothing/**\" in [package.metadata.testcache] of `alpha` matched no file"
        ),
        "{}",
        stderr(&output)
    );
}

// Property: a `[package.metadata.testcache.targets.<name>]` table narrows what it declares to
// that target. alpha's `reads_input` table declares `reads_input_data/**` and, here, spawns
// beta's `betatool`. Changing that input reruns `reads_input` alone, not alpha's unit tests;
// changing `betatool` reruns `reads_input` too, besides beta's own binaries. `--explain` names
// the level of each. Both changes are made in every case, in turn on one fixture, in a drawn
// order.
#[hegel::test(test_cases = 2)]
fn target_tables_rerun_only_their_target(tc: TestCase) {
    let reversed = tc.draw(gs::booleans());
    let stamp = tc.draw(gs::integers::<u32>().min_value(1));
    let fixture = Fixture::new();
    // The target table is the last one in alpha's manifest, so this line joins it.
    fixture.append("alpha/Cargo.toml", "spawns = [\"betatool\"]\n");
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), names(&ALL), "the first run");
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), BTreeSet::new(), "the second run");

    let changes: [(&str, &[&str], &str); 2] = [
        (
            "alpha/reads_input_data/value.txt",
            &["reads_input"],
            "input alpha/reads_input_data/value.txt changed (target `reads_input`)",
        ),
        (
            "beta/src/main.rs",
            &["betatool", "reads_input", "spawns_tool"],
            "spawned target reads_input spawns betatool changed",
        ),
    ];
    for (file, expected, explained) in in_order(changes, reversed) {
        if file.ends_with(".rs") {
            fixture.set_stamp(file, stamp);
        } else {
            fixture.write(file, &stamp.to_string());
        }
        let output = fixture.testcache_ok(&["--explain"]);

        assert_eq!(
            fixture.take_runs(),
            names(expected),
            "after changing {file}"
        );
        assert!(stderr(&output).contains(explained), "{}", stderr(&output));
    }
}

// Property (9.4): a failing binary is not recorded, so it runs again, and a passing one is, so
// it does not. Every binary starts out failing and is fixed one at a time, in a drawn order:
// each run then runs exactly the binaries that have not passed yet.
#[hegel::test(test_cases = 2)]
fn a_failing_binary_runs_again(tc: TestCase) {
    let rotation = tc.draw(gs::integers::<usize>().max_value(ALL.len() - 1));
    let fixture = Fixture::new();
    for name in ALL {
        fixture.fail(name, true);
    }
    let output = fixture.testcache(&["--no-fail-fast"], &[]);
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    assert_eq!(fixture.take_runs(), names(&ALL));

    let mut order = ALL;
    order.rotate_left(rotation);
    for (index, fixed) in order.iter().enumerate() {
        fixture.fail(fixed, false);
        let output = fixture.testcache(&["--no-fail-fast"], &[]);
        let still_failing = &order[index + 1..];
        let expected_code = if still_failing.is_empty() { 0 } else { 101 };
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{}",
            stderr(&output)
        );
        // The one just fixed runs (and passes); the others still fail and run again.
        assert_eq!(
            fixture.take_runs(),
            names(&order[index..]),
            "after fixing {fixed}"
        );
    }
    fixture.testcache_ok(&[]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
}

// Property (9.5): different arguments after `--` re-run everything, the same ones again skip.
#[hegel::test(test_cases = 2)]
fn changed_test_arguments_rerun(tc: TestCase) {
    let argument = tc.draw(
        gs::sampled_from(&["--test-threads=1", "--nocapture", "--include-ignored"])
            .print_as_debug(),
    );
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    fixture.testcache_ok(&["--", argument]);
    assert_eq!(fixture.take_runs(), names(&ALL));
    fixture.testcache_ok(&["--", argument]);
    assert_eq!(fixture.take_runs(), BTreeSet::new());
}

// Property (9.7): --no-cache runs everything and records it, with and without --explain. Both
// are checked in every case, in a drawn order, on one fixture: each step ends with every
// binary recorded, which is where the next one starts.
#[hegel::test(test_cases = 2)]
fn no_cache_runs_everything(tc: TestCase) {
    let reversed = tc.draw(gs::booleans());
    let fixture = Fixture::new();
    fixture.testcache_ok(&[]);
    fixture.take_runs();

    for explain in in_order([false, true], reversed) {
        let args: &[&str] = if explain {
            &["--no-cache", "--explain"]
        } else {
            &["--no-cache"]
        };
        fixture.testcache_ok(args);
        assert_eq!(fixture.take_runs(), names(&ALL), "{args:?}");
        fixture.testcache_ok(&[]);
        assert_eq!(fixture.take_runs(), BTreeSet::new(), "after {args:?}");
    }
}

// Property (9.8): `status` runs nothing and records nothing, and `clear` forgets every pass.
// Checked before and after a recorded run in every case, each on a fresh fixture, in a drawn
// order.
#[hegel::test(test_cases = 2)]
fn status_changes_nothing_and_clear_forgets(tc: TestCase) {
    let reversed = tc.draw(gs::booleans());
    for ran_before in in_order([false, true], reversed) {
        let fixture = Fixture::new();
        if ran_before {
            fixture.testcache_ok(&[]);
            fixture.take_runs();
        }

        let status = fixture.testcache_ok(&["status"]);
        assert_eq!(
            fixture.take_runs(),
            BTreeSet::new(),
            "ran before: {ran_before}"
        );
        let verb = if ran_before {
            "Would skip"
        } else {
            "Would run"
        };
        assert!(stderr(&status).contains(verb), "{}", stderr(&status));

        fixture.testcache_ok(&[]);
        let expected = if ran_before {
            BTreeSet::new()
        } else {
            names(&ALL)
        };
        assert_eq!(fixture.take_runs(), expected, "ran before: {ran_before}");

        fixture.testcache_ok(&["clear"]);
        fixture.testcache_ok(&[]);
        assert_eq!(
            fixture.take_runs(),
            names(&ALL),
            "after clear, ran before: {ran_before}"
        );
    }
}

// Property (9.9, conformance): every test binary sees the same working directory and the same
// environment under `cargo testcache` (direct runner) as under `cargo test`, with and without a
// separate build directory. `alpha`'s build script adds `OUT_DIR`, a `rustc-env` variable and a
// native library directory, which alpha's binaries must see and gamma's must not.
#[hegel::test(test_cases = 2)]
fn the_direct_runner_matches_cargo_test(tc: TestCase) {
    let cargo_first = tc.draw(gs::booleans());
    // Both layouts in every case: Cargo keeps a build script's library directory on the path
    // only when it lies under the target directory, which a separate build directory rules out.
    for separate_build_dir in [false, true] {
        let dumps = conformance_dumps(separate_build_dir, cargo_first);
        for name in ALL {
            assert_eq!(dumps.read_testcache(name), dumps.read_cargo(name), "{name}");
        }
        // Guard against a vacuous comparison: the build script's output really is there.
        let alpha = dumps.read_cargo("alpha");
        let gamma = dumps.read_cargo("gamma");
        assert!(
            alpha.iter().any(|line| line == "ALPHA_BUILD_ENV=1"),
            "{alpha:#?}"
        );
        assert!(
            alpha.iter().any(|line| line.starts_with("OUT_DIR=")),
            "{alpha:#?}"
        );
        assert!(
            !gamma
                .iter()
                .any(|line| line.starts_with("ALPHA_BUILD_ENV=")),
            "{gamma:#?}"
        );
        let native = Path::new("out").join("native");
        let native = native.to_string_lossy();
        let native_on_path = alpha.iter().any(|line| line.contains(native.as_ref()));
        assert_eq!(native_on_path, !separate_build_dir, "{alpha:#?}");
    }
}

/// The environment dumps of one `cargo test` and one `cargo testcache --no-cache` (direct
/// runner) of the same fixture copy.
struct Dumps {
    /// Keeps the fixture, and so the dumps, alive.
    fixture: Fixture,
}

impl Dumps {
    /// The dump directory for `cargo test` (`under_cargo`) or `cargo testcache`.
    fn dir(&self, under_cargo: bool) -> PathBuf {
        let name = if under_cargo {
            "dump-cargo"
        } else {
            "dump-testcache"
        };
        self.fixture.temp.path().join(name)
    }

    /// `name`'s dump under `cargo test`, one line per entry.
    fn read_cargo(&self, name: &str) -> Vec<String> {
        read_dump(&self.dir(true), name)
    }

    /// `name`'s dump under `cargo testcache`, one line per entry.
    fn read_testcache(&self, name: &str) -> Vec<String> {
        read_dump(&self.dir(false), name)
    }
}

/// Runs `cargo test` and `cargo testcache --no-cache` on a fresh fixture copy, in the order
/// given, each test binary dumping its environment.
fn conformance_dumps(separate_build_dir: bool, cargo_first: bool) -> Dumps {
    let dumps = Dumps {
        fixture: Fixture::with_build_dir(separate_build_dir),
    };
    for under_cargo in [true, false] {
        fs::create_dir(dumps.dir(under_cargo)).expect("create a dump directory");
    }
    let run = |under_cargo: bool| {
        let dir = dumps.dir(under_cargo);
        let extra_env = [("FIXTURE_DUMP_DIR", dir.as_os_str())];
        let output = if under_cargo {
            dumps.fixture.cargo(&["test"], &extra_env)
        } else {
            dumps.fixture.testcache(&["--no-cache"], &extra_env)
        };
        assert!(output.status.success(), "{}", stderr(&output));
    };
    run(cargo_first);
    run(!cargo_first);
    dumps
}

/// Variables left out of the comparison, because the test harness itself causes them.
///
/// - `FIXTURE_DUMP_DIR` differs between the two runs by construction.
/// - `__LLVM_PROFILE_RT_INIT_ONCE` is set in its own process by the LLVM profiling runtime of a
///   coverage-instrumented `cargo-testcache` (`just coverage` in CI), and the test binaries it
///   runs directly inherit it. A normally built `cargo-testcache` never sets it.
const HARNESS_VARIABLES: [&str; 2] = ["FIXTURE_DUMP_DIR", "__LLVM_PROFILE_RT_INIT_ONCE"];

/// A test binary's environment dump, without the [`HARNESS_VARIABLES`].
fn read_dump(dir: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(dir.join(format!("{name}.env")))
        .unwrap_or_else(|error| panic!("{name} dumped its environment: {error}"))
        .lines()
        .filter(|line| {
            let name = line.split_once('=').map_or(*line, |(name, _)| name);
            !HARNESS_VARIABLES.contains(&name)
        })
        .map(str::to_owned)
        .collect()
}
