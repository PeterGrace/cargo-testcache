//! Cargo's own configuration: the `.cargo/config.toml` files and `--config` arguments that
//! Cargo reads for this invocation.
//!
//! Cargo configuration can change what `cargo test` gives a test binary without changing the
//! binary: an `[env]` table sets variables in its environment, and other settings (a runner,
//! for one) change how it runs. This module does not interpret those settings. It hashes the
//! whole configuration into one key part, so any change reruns every binary (coarse, but never
//! a wrong skip), and it notices `[env]`, which only the via-cargo runner can apply.
//!
//! The files are `.cargo/config.toml` and `.cargo/config` in the current directory and each of
//! its ancestors, then `config.toml` and `config` in `$CARGO_HOME` (default `~/.cargo`), as in
//! the Cargo reference's "Hierarchical structure". A `--config` value is a `KEY=VALUE` TOML
//! fragment or, when it names an existing file, that file.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::args::CargoArgs;
use crate::key::{Digest, PartResult};

/// The `--config` option of Cargo.
const CONFIG_FLAG: &str = "--config";

/// The configuration table that sets environment variables for the processes Cargo runs.
const ENV_TABLE: &str = "env";

/// What this tool needs to know of Cargo's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoConfig {
    /// The digest of every file and `--config` value, or why one could not be read.
    digest: PartResult<Digest>,
    /// Whether any of them has (or may have) an `[env]` table.
    sets_env: bool,
}

impl CargoConfig {
    /// Reads the configuration Cargo would read when started from this process's working
    /// directory, with this process's `$CARGO_HOME` and the user's `--config` arguments.
    ///
    /// # Arguments
    ///
    /// * `cargo_args` - The user's cargo arguments, for `--config`.
    ///
    /// # Returns
    ///
    /// The configuration. When the working directory is unknown, its digest is unavailable and
    /// it counts as setting `[env]`.
    pub fn load(cargo_args: &CargoArgs) -> Self {
        let cwd = match env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                return Self {
                    digest: Err(format!("cannot find the working directory: {error}")),
                    sets_env: true,
                };
            }
        };
        // Cargo resolves a relative `$CARGO_HOME` against the working directory.
        let cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| env::home_dir().map(|home| home.join(".cargo")))
            .map(|home| cwd.join(home));
        Self::read(&cwd, cargo_home.as_deref(), cargo_args.values(CONFIG_FLAG))
    }

    /// Reads the configuration for a given working directory, Cargo home and `--config`
    /// values.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The directory Cargo starts in; it and every ancestor are searched.
    /// * `cargo_home` - `$CARGO_HOME`, if known.
    /// * `cli_values` - The `--config` values, in order.
    ///
    /// # Returns
    ///
    /// The configuration. A file that exists but cannot be read makes the digest unavailable
    /// and counts as setting `[env]`, the safe direction for both.
    pub fn read<'a>(
        cwd: &Path,
        cargo_home: Option<&Path>,
        cli_values: impl IntoIterator<Item = &'a OsStr>,
    ) -> Self {
        let mut collector = Collector::default();
        let files = cwd
            .ancestors()
            .map(|dir| dir.join(".cargo"))
            .chain(cargo_home.map(Path::to_path_buf))
            .flat_map(|dir| [dir.join("config.toml"), dir.join("config")]);
        for file in files {
            collector.file(&file);
        }
        for value in cli_values {
            collector.cli_value(cwd, value);
        }
        collector.finish()
    }

    /// The configuration's key part.
    ///
    /// # Returns
    ///
    /// Its digest, or why a file could not be read.
    pub fn digest(&self) -> PartResult<Digest> {
        self.digest.clone()
    }

    /// Whether the configuration has an `[env]` table, which the direct runner cannot apply.
    ///
    /// # Returns
    ///
    /// `true` when a file or `--config` value has one, does not parse, or cannot be read.
    pub fn sets_env(&self) -> bool {
        self.sets_env
    }
}

/// Gathers the fields of the digest and whether `[env]` was seen.
#[derive(Debug, Default)]
struct Collector {
    /// Length-prefixed by [`Digest::of_fields`]; each entry starts with a tag field, so a file
    /// and a `--config` value can never be mistaken for each other.
    fields: Vec<Vec<u8>>,
    sets_env: bool,
    /// The first file that could not be read.
    unreadable: Option<String>,
}

impl Collector {
    /// Adds a configuration file if it exists.
    fn file(&mut self, path: &Path) {
        match fs::read(path) {
            Ok(contents) => {
                tracing::debug!(path = %path.display(), "reading Cargo configuration");
                self.sets_env |= declares_env(&contents);
                self.fields.push(b"file".to_vec());
                self.fields
                    .push(path.as_os_str().as_encoded_bytes().to_vec());
                self.fields.push(contents);
            }
            // A missing file, or a `.cargo` that is not a directory, is no configuration.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => {
                self.sets_env = true;
                self.unreadable
                    .get_or_insert_with(|| format!("cannot read {}: {error}", path.display()));
            }
        }
    }

    /// Adds a `--config` value: the file it names when one exists (as Cargo decides), else
    /// the value itself as a TOML fragment.
    fn cli_value(&mut self, cwd: &Path, value: &OsStr) {
        self.fields.push(CONFIG_FLAG.as_bytes().to_vec());
        self.fields.push(value.as_encoded_bytes().to_vec());
        let as_path = cwd.join(value);
        if !value.is_empty() && as_path.exists() {
            self.file(&as_path);
        } else {
            self.sets_env |= declares_env(value.as_encoded_bytes());
        }
    }

    /// The configuration gathered.
    fn finish(self) -> CargoConfig {
        let digest = match self.unreadable {
            Some(why) => Err(why),
            None => Ok(Digest::of_fields(self.fields.iter().map(Vec::as_slice))),
        };
        CargoConfig {
            digest,
            sets_env: self.sets_env,
        }
    }
}

/// Whether TOML text has a top-level `env` key: an `[env]` table, or `env.NAME = ...` as a
/// `--config` value writes it.
///
/// # Arguments
///
/// * `contents` - The text's bytes.
///
/// # Returns
///
/// `true` when it has one, or when it is not UTF-8 or does not parse, since then nothing can be
/// ruled out.
fn declares_env(contents: &[u8]) -> bool {
    std::str::from_utf8(contents)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        // `is_none_or` is `table is None or <test>(table)`.
        .is_none_or(|table| table.contains_key(ENV_TABLE))
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    /// Configuration texts, and whether each declares `[env]` as far as this module is
    /// concerned (an unparsable text counts as declaring it).
    const TEXTS: [(&str, bool); 8] = [
        ("[env]\nX = \"1\"\n", true),
        ("env.X = \"1\"", true),
        ("env = { X = \"1\" }", true),
        ("[build]\njobs = 4\n", false),
        ("build.jobs = 4", false),
        ("[target.x]\nenv = 1\n", false),
        ("", false),
        ("this is [not toml", true),
    ];

    /// Where a drawn file goes: the working directory's `.cargo`, its parent's, or the Cargo
    /// home.
    const PLACES: [&str; 3] = ["ws/sub/.cargo", "ws/.cargo", "home"];

    // Property: a text declares `[env]` exactly when it has a top-level `env` key or does not
    // parse.
    #[hegel::test]
    fn env_is_found_at_the_top_level_only(tc: TestCase) {
        let (text, expected) = tc.draw(gs::sampled_from(TEXTS.to_vec()));

        assert_eq!(declares_env(text.as_bytes()), expected, "{text:?}");
        assert!(declares_env(&[0xff, 0xfe]), "bytes that are not UTF-8");
    }

    // Property: the configuration of drawn files and `--config` values is available, sets
    // `[env]` exactly when one of them declares it, and its digest changes when any one file
    // or value changes.
    #[hegel::test(test_cases = 30)]
    fn every_file_and_value_counts(tc: TestCase) {
        let placed = tc.draw(
            gs::vecs(gs::tuples!(
                gs::sampled_from(PLACES.to_vec()),
                gs::booleans(),
                gs::sampled_from(TEXTS.to_vec())
            ))
            .max_size(3),
        );
        let cli = tc.draw(gs::vecs(gs::sampled_from(TEXTS.to_vec())).max_size(2));
        let temp = tempfile::tempdir().expect("temporary directory");
        let cwd = temp.path().join("ws/sub");
        let home = temp.path().join("home");
        // Later placements at the same path overwrite earlier ones, so the expectation is
        // computed from the files as they end up on disk.
        let mut on_disk = std::collections::BTreeMap::new();
        fs::create_dir_all(&cwd).expect("create the working directory");
        // The temporary directory's own ancestors may hold configuration of their own.
        let outside = CargoConfig::read(&cwd, None, []).sets_env();
        for (place, toml_extension, (text, declares)) in &placed {
            let dir = temp.path().join(place);
            fs::create_dir_all(&dir).expect("create the directory");
            let name = if *toml_extension {
                "config.toml"
            } else {
                "config"
            };
            fs::write(dir.join(name), text).expect("write the file");
            on_disk.insert(dir.join(name), *declares);
        }
        let cli_values: Vec<&OsStr> = cli.iter().map(|(text, _)| OsStr::new(*text)).collect();

        let config = CargoConfig::read(&cwd, Some(&home), cli_values.iter().copied());

        let expected_env = outside
            || on_disk.values().any(|declares| *declares)
            || cli.iter().any(|(_, declares)| *declares);
        assert_eq!(config.sets_env(), expected_env);
        let digest = config.digest().expect("every file is readable");
        if let Some(file) = on_disk.keys().next() {
            fs::write(file, "build.jobs = 99").expect("change a file");
            let changed = CargoConfig::read(&cwd, Some(&home), cli_values.iter().copied());
            assert_ne!(
                changed.digest(),
                Ok(digest),
                "after changing {}",
                file.display()
            );
        }
        let mut more_values = cli_values.clone();
        more_values.push(OsStr::new("build.jobs = 1"));
        let with_value = CargoConfig::read(&cwd, Some(&home), more_values.iter().copied());
        assert_ne!(with_value.digest(), config.digest());
    }

    // Property: a `--config` value naming an existing file reads that file, and a file that
    // exists but cannot be read makes the digest unavailable and counts as `[env]`.
    #[hegel::test(test_cases = 10)]
    fn config_files_given_by_path_are_read(tc: TestCase) {
        let (text, declares) = tc.draw(gs::sampled_from(TEXTS.to_vec()));
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("extra.toml"), text).expect("write the file");

        let outside = CargoConfig::read(temp.path(), None, []).sets_env();

        let config = CargoConfig::read(temp.path(), None, [OsStr::new("extra.toml")]);

        assert_eq!(config.sets_env(), declares || outside);
        // A directory where a file is expected exists but cannot be read as one.
        fs::create_dir_all(temp.path().join(".cargo/config.toml")).expect("create a directory");
        let unreadable = CargoConfig::read(temp.path(), None, [OsStr::new("extra.toml")]);
        assert!(unreadable.digest().is_err());
        assert!(unreadable.sets_env());
    }
}
