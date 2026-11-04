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
    /// Builds a package from its metadata.
    fn new(raw: RawPackage, config: TestcacheConfig) -> Self {
        let root = raw
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Self {
            env: package_env(&raw, &root),
            name: raw.name,
            root,
            config,
        }
    }

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
        let output = command
            .output()
            .map_err(Error::io("running cargo metadata"))?;
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
            build_dir: raw
                .build_directory
                .unwrap_or_else(|| raw.target_directory.clone()),
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
        self.packages
            .iter()
            .map(|(id, package)| (id.as_str(), package))
    }

    /// The environment variable names whose values are part of a package's keys.
    pub fn env_names<'a>(&'a self, package: &'a Package) -> impl Iterator<Item = &'a str> {
        self.config
            .env
            .iter()
            .chain(&package.config.env)
            .map(String::as_str)
    }

    /// The `spawns` paths that apply to a package.
    pub fn spawns<'a>(&'a self, package: &'a Package) -> impl Iterator<Item = &'a str> {
        self.config
            .spawns
            .iter()
            .chain(&package.config.spawns)
            .map(String::as_str)
    }

    /// Every `always_run` pattern, the workspace's and every package's.
    ///
    /// A package's patterns are not scoped to that package: they join one workspace-wide set
    /// and match `package/target` for every package. This differs from `inputs`, whose globs
    /// are relative to the package that declares them.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGlob`] when a pattern does not compile.
    pub fn always_run(&self) -> Result<AlwaysRun> {
        let packages = self
            .packages
            .values()
            .flat_map(|package| &package.config.always_run);
        AlwaysRun::new(
            self.config
                .always_run
                .iter()
                .chain(packages)
                .map(String::as_str),
        )
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

/// The variables `cargo test` sets for a package's test binaries (checked against Cargo 1.99).
fn package_env(raw: &RawPackage, root: &Path) -> Vec<(&'static str, OsString)> {
    let without_build = raw
        .version
        .split_once('+')
        .map_or(raw.version.as_str(), |(v, _)| v);
    let (numbers, pre) = without_build.split_once('-').unwrap_or((without_build, ""));
    let mut numbers = numbers.splitn(3, '.');
    // A closure that captures `numbers` mutably; each call takes the next component.
    let mut next_number = || OsString::from(numbers.next().unwrap_or_default());
    let text = |value: &Option<String>| OsString::from(value.as_deref().unwrap_or_default());
    vec![
        ("CARGO_MANIFEST_DIR", root.as_os_str().to_owned()),
        (
            "CARGO_MANIFEST_PATH",
            raw.manifest_path.as_os_str().to_owned(),
        ),
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
    use hegel::generators as gs;

    fn env_value<'a>(package: &'a Package, name: &str) -> &'a OsStr {
        package
            .env()
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_os_str())
            .unwrap_or_else(|| panic!("{name} is set"))
    }

    // Draws a dot-separated list of non-empty identifiers over `alphabet`, which is what a
    // semver pre-release or build suffix is; Cargo rejects empty identifiers such as `.` or `1.`.
    fn identifiers(tc: &TestCase, alphabet: &str) -> Option<String> {
        let parts = tc.draw(gs::optional(
            gs::vecs(gs::text().alphabet(alphabet).min_size(1).max_size(3))
                .min_size(1)
                .max_size(3),
        ));
        parts.map(|parts| parts.join("."))
    }

    // Property: CARGO_PKG_VERSION_* split the version as Cargo does: the pre-release is what
    // follows `-`, and build metadata after `+` is dropped from every part but the whole.
    #[hegel::test]
    fn version_parts_are_split_as_cargo_does(tc: TestCase) {
        let [major, minor, patch] = [0, 1, 2].map(|_| tc.draw(gs::integers::<u32>()));
        // `1` is the only digit, so no numeric identifier has a leading zero, which semver forbids.
        let pre = identifiers(&tc, "abc1");
        let build = identifiers(&tc, "xyz9");
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
        assert_eq!(
            env_value(package, "CARGO_PKG_VERSION_MAJOR"),
            major.to_string().as_str()
        );
        assert_eq!(
            env_value(package, "CARGO_PKG_VERSION_MINOR"),
            minor.to_string().as_str()
        );
        assert_eq!(
            env_value(package, "CARGO_PKG_VERSION_PATCH"),
            patch.to_string().as_str()
        );
        assert_eq!(
            env_value(package, "CARGO_PKG_VERSION_PRE"),
            pre.unwrap_or_default().as_str()
        );
        assert_eq!(
            env_value(package, "CARGO_PKG_AUTHORS"),
            "A <a@example.com>:B"
        );
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
        let profile = tc.draw(
            gs::vecs(gs::text().alphabet("abc-").min_size(1).max_size(5))
                .min_size(1)
                .max_size(2),
        );
        let build_dir = if separate {
            "/cache/build"
        } else {
            "/ws/target"
        };
        let workspace = Workspace::from_json(&metadata("0.1.0", separate.then_some(build_dir)))
            .expect("valid metadata");
        let relative: PathBuf = profile.iter().collect();
        let executable = Path::new(build_dir)
            .join(&relative)
            .join("deps")
            .join("t-0123");

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
