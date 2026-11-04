//! Cargo's own configuration: the `.cargo/config.toml` files and `--config` arguments that
//! Cargo reads for this invocation.
//!
//! Cargo configuration can change what `cargo test` gives a test binary without changing the
//! binary: an `[env]` table sets variables in its environment, and other settings (a runner,
//! for one) change how it runs. This module does not interpret those settings. It hashes the
//! whole configuration into one key part, so any change reruns every binary (coarse, but never
//! a wrong skip), and it notices `[env]` and target runners (`target.<triple>.runner`,
//! `target.'cfg(..)'.runner`), which only the via-cargo runner can apply.
//!
//! The files are `.cargo/config.toml` and `.cargo/config` in the current directory and each of
//! its ancestors, then `config.toml` and `config` in `$CARGO_HOME` (default `~/.cargo`), as in
//! the Cargo reference's "Hierarchical structure". A `--config` value is a `KEY=VALUE` TOML
//! fragment or, when it names an existing file, that file. A top-level `include` in any of
//! them names more files, relative to the including file (a fragment's, to the working
//! directory); they are followed recursively and count like any other configuration file.

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

/// The configuration table of per-target settings, `[target.<triple>]` and
/// `[target.'cfg(..)']`.
const TARGET_TABLE: &str = "target";

/// The per-target key naming a program that runs test binaries (a valgrind or qemu wrapper).
const RUNNER_KEY: &str = "runner";

/// The top-level key that names more configuration files to read.
const INCLUDE_KEY: &str = "include";

/// What a configuration text sets that only Cargo can apply to the binaries it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Declares {
    /// An `[env]` table.
    env: bool,
    /// A runner for some target, whichever triple or `cfg` it is for.
    runner: bool,
}

impl Declares {
    /// Both, for a text that cannot be read or parsed: nothing can be ruled out.
    const EVERYTHING: Self = Self {
        env: true,
        runner: true,
    };

    /// Adds what another text declares.
    fn add(&mut self, other: Self) {
        self.env |= other.env;
        self.runner |= other.runner;
    }
}

/// What this tool needs to know of Cargo's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoConfig {
    /// The digest of every file and `--config` value, or why one could not be read.
    digest: PartResult<Digest>,
    /// What any of them sets (or may set) that only Cargo applies.
    declares: Declares,
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
    /// it counts as setting `[env]` and a runner.
    pub fn load(cargo_args: &CargoArgs) -> Self {
        let cwd = match env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => {
                return Self {
                    digest: Err(format!("cannot find the working directory: {error}")),
                    declares: Declares::EVERYTHING,
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
    /// and counts as setting `[env]` and a runner, the safe direction for all three.
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
            collector.file(&file, Origin::Searched);
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
        self.declares.env
    }

    /// Whether the configuration sets a target runner (`target.<triple>.runner` or
    /// `target.'cfg(..)'.runner`), which the direct runner cannot apply. Every target counts,
    /// not only the one being built: this errs towards running through Cargo.
    ///
    /// # Returns
    ///
    /// `true` when a file or `--config` value sets one, does not parse, or cannot be read.
    pub fn sets_runner(&self) -> bool {
        self.declares.runner
    }
}

/// How a configuration file came to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// One of the places Cargo looks (or a `--config` file): missing means no configuration.
    Searched,
    /// Named by an `include`: missing is an error unless the include is optional.
    Included {
        /// `optional = true` in the include's table.
        optional: bool,
    },
}

/// One `include` entry: a path, relative to the including file, and whether it may be missing.
type Include = (String, bool);

/// Gathers the fields of the digest and whether `[env]` was seen.
#[derive(Debug, Default)]
struct Collector {
    /// Length-prefixed by [`Digest::of_fields`]; each entry starts with a tag field, so a file
    /// and a `--config` value can never be mistaken for each other.
    fields: Vec<Vec<u8>>,
    declares: Declares,
    /// The first file that could not be read, or include that could not be followed.
    unreadable: Option<String>,
    /// The files being read, outermost first, to stop an include cycle.
    chain: Vec<PathBuf>,
}

impl Collector {
    /// Gives up on knowing the configuration: the key is unavailable and everything counts as
    /// declared, the safe direction for both.
    fn give_up(&mut self, why: impl FnOnce() -> String) {
        self.declares = Declares::EVERYTHING;
        self.unreadable.get_or_insert_with(why);
    }

    /// Adds a configuration file, and the files it includes.
    fn file(&mut self, path: &Path, origin: Origin) {
        let contents = match fs::read(path) {
            Ok(contents) => contents,
            // A missing file, or a `.cargo` that is not a directory, is no configuration,
            // unless an include requires it.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                match origin {
                    Origin::Searched => {}
                    // Hashed as absent, so creating it later changes the key.
                    Origin::Included { optional: true } => {
                        self.fields.push(b"absent".to_vec());
                        self.fields
                            .push(path.as_os_str().as_encoded_bytes().to_vec());
                    }
                    Origin::Included { optional: false } => {
                        self.give_up(|| format!("cannot read included {}: {error}", path.display()))
                    }
                }
                return;
            }
            Err(error) => {
                self.give_up(|| format!("cannot read {}: {error}", path.display()));
                return;
            }
        };
        tracing::debug!(path = %path.display(), "reading Cargo configuration");
        let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if self.chain.contains(&identity) {
            self.give_up(|| format!("{} includes itself", path.display()));
            return;
        }
        self.fields.push(b"file".to_vec());
        self.fields
            .push(path.as_os_str().as_encoded_bytes().to_vec());
        let table = parse(&contents);
        self.fields.push(contents);
        let Some(table) = table else {
            self.declares = Declares::EVERYTHING;
            return;
        };
        self.declares.add(declarations_in(&table));
        self.chain.push(identity);
        // An include is relative to the including file's directory.
        self.includes(path.parent().unwrap_or(Path::new("")), &table, path);
        self.chain.pop();
    }

    /// Follows a parsed configuration's `include`, if it has one.
    ///
    /// * `base` - The directory include paths are relative to.
    /// * `table` - The configuration.
    /// * `source` - Where it came from, for messages.
    fn includes(&mut self, base: &Path, table: &toml::Table, source: &Path) {
        let Some(value) = table.get(INCLUDE_KEY) else {
            return;
        };
        let Some(entries) = include_entries(value) else {
            self.give_up(|| format!("unrecognised include in {}", source.display()));
            return;
        };
        for (path, optional) in entries {
            self.file(&base.join(path), Origin::Included { optional });
        }
    }

    /// Adds a `--config` value: the file it names when one exists (as Cargo decides), else
    /// the value itself as a TOML fragment, whose includes are relative to `cwd`.
    fn cli_value(&mut self, cwd: &Path, value: &OsStr) {
        self.fields.push(CONFIG_FLAG.as_bytes().to_vec());
        self.fields.push(value.as_encoded_bytes().to_vec());
        let as_path = cwd.join(value);
        if !value.is_empty() && as_path.exists() {
            self.file(&as_path, Origin::Searched);
            return;
        }
        match parse(value.as_encoded_bytes()) {
            Some(table) => {
                self.declares.add(declarations_in(&table));
                self.includes(cwd, &table, Path::new(CONFIG_FLAG));
            }
            None => self.declares = Declares::EVERYTHING,
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
            declares: self.declares,
        }
    }
}

/// Parses a configuration text.
///
/// # Returns
///
/// The table, or `None` when the text is not UTF-8 or not TOML.
fn parse(contents: &[u8]) -> Option<toml::Table> {
    std::str::from_utf8(contents)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
}

/// The entries of an `include` value, in every form Cargo accepts: a path, an array of paths,
/// or an array of tables with a `path` and an optional `optional`.
///
/// # Returns
///
/// Each path and whether it is optional, in order; `None` for any other shape.
fn include_entries(value: &toml::Value) -> Option<Vec<Include>> {
    let entry = |item: &toml::Value| -> Option<Include> {
        match item {
            toml::Value::String(path) => Some((path.clone(), false)),
            toml::Value::Table(table) => {
                let path = table.get("path")?.as_str()?.to_owned();
                let optional = match table.get("optional") {
                    None => false,
                    Some(optional) => optional.as_bool()?,
                };
                // A key other than these two is a shape this tool does not know.
                let known = table.keys().all(|key| key == "path" || key == "optional");
                known.then_some((path, optional))
            }
            _ => None,
        }
    };
    match value {
        toml::Value::String(path) => Some(vec![(path.clone(), false)]),
        // Collecting `Option`s into one `Option` gives `None` if any entry is unrecognised.
        toml::Value::Array(items) => items.iter().map(entry).collect(),
        _ => None,
    }
}

/// What a TOML text sets that only Cargo applies, as [`declarations_in`] finds it.
///
/// # Returns
///
/// What it sets; everything when it is not UTF-8 or does not parse, since then nothing can be
/// ruled out.
#[cfg(test)]
fn declarations(contents: &[u8]) -> Declares {
    parse(contents).map_or(Declares::EVERYTHING, |table| declarations_in(&table))
}

/// What a parsed configuration sets that only Cargo applies: a top-level `env` key (an `[env]`
/// table, or `env.NAME = ...` as a `--config` value writes it), and a `runner` under any table
/// of `target` (`[target.<triple>]` or `[target.'cfg(..)']`, or `target.<x>.runner = ...`). The
/// files it includes are looked at separately.
///
/// # Returns
///
/// What it sets.
fn declarations_in(table: &toml::Table) -> Declares {
    // `get(..)` is `None` for a missing key; `as_table` is `None` for a value that is not a
    // table. `is_some_and` is `x is not None and <test>(x)`.
    let runner = table
        .get(TARGET_TABLE)
        .and_then(toml::Value::as_table)
        .is_some_and(|targets| {
            targets.values().any(|target| {
                target
                    .as_table()
                    .is_some_and(|settings| settings.contains_key(RUNNER_KEY))
            })
        });
    Declares {
        env: table.contains_key(ENV_TABLE),
        runner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    /// Configuration texts, and whether each declares `[env]` and a runner as far as this
    /// module is concerned (an unparsable text counts as declaring both).
    const TEXTS: [(&str, bool, bool); 12] = [
        ("[env]\nX = \"1\"\n", true, false),
        ("env.X = \"1\"", true, false),
        ("env = { X = \"1\" }", true, false),
        ("[build]\njobs = 4\n", false, false),
        ("build.jobs = 4", false, false),
        ("[target.x]\nenv = 1\n", false, false),
        (
            "[target.x86_64-unknown-linux-gnu]\nrunner = \"valgrind\"\n",
            false,
            true,
        ),
        (
            "[target.'cfg(unix)']\nrunner = [\"qemu\", \"-x\"]\n",
            false,
            true,
        ),
        ("target.\"cfg(all())\".runner = \"env\"", false, true),
        (
            "runner = \"top-level, not a target's\"\ntarget = 1",
            false,
            false,
        ),
        ("", false, false),
        ("this is [not toml", true, true),
    ];

    /// Where a drawn file goes: the working directory's `.cargo`, its parent's, or the Cargo
    /// home.
    const PLACES: [&str; 3] = ["ws/sub/.cargo", "ws/.cargo", "home"];

    // Property: a text declares `[env]` exactly when it has a top-level `env` key, and a
    // runner exactly when some table under `target` has a `runner` key; a text that does not
    // parse declares both.
    #[hegel::test]
    fn env_and_runners_are_found_where_cargo_reads_them(tc: TestCase) {
        let (text, env, runner) = tc.draw(gs::sampled_from(TEXTS.to_vec()));

        assert_eq!(
            declarations(text.as_bytes()),
            Declares { env, runner },
            "{text:?}"
        );
        assert_eq!(declarations(&[0xff, 0xfe]), Declares::EVERYTHING);
    }

    // Property: the configuration of drawn files and `--config` values is available, sets
    // `[env]` or a runner exactly when one of them declares it, and its digest changes when any
    // one file or value changes.
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
        let outside = CargoConfig::read(&cwd, None, []);
        for (place, toml_extension, (text, env, runner)) in &placed {
            let dir = temp.path().join(place);
            fs::create_dir_all(&dir).expect("create the directory");
            let name = if *toml_extension {
                "config.toml"
            } else {
                "config"
            };
            fs::write(dir.join(name), text).expect("write the file");
            on_disk.insert(dir.join(name), (*env, *runner));
        }
        let cli_values: Vec<&OsStr> = cli.iter().map(|(text, ..)| OsStr::new(*text)).collect();

        let config = CargoConfig::read(&cwd, Some(&home), cli_values.iter().copied());

        let expected_env = outside.sets_env()
            || on_disk.values().any(|(env, _)| *env)
            || cli.iter().any(|(_, env, _)| *env);
        let expected_runner = outside.sets_runner()
            || on_disk.values().any(|(_, runner)| *runner)
            || cli.iter().any(|(.., runner)| *runner);
        assert_eq!(config.sets_env(), expected_env);
        assert_eq!(config.sets_runner(), expected_runner);
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
    // exists but cannot be read makes the digest unavailable and counts as `[env]` and a
    // runner.
    #[hegel::test(test_cases = 10)]
    fn config_files_given_by_path_are_read(tc: TestCase) {
        let (text, env, runner) = tc.draw(gs::sampled_from(TEXTS.to_vec()));
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("extra.toml"), text).expect("write the file");
        let outside = CargoConfig::read(temp.path(), None, []);

        let config = CargoConfig::read(temp.path(), None, [OsStr::new("extra.toml")]);

        assert_eq!(config.sets_env(), env || outside.sets_env());
        assert_eq!(config.sets_runner(), runner || outside.sets_runner());
        // A directory where a file is expected exists but cannot be read as one.
        fs::create_dir_all(temp.path().join(".cargo/config.toml")).expect("create a directory");
        let unreadable = CargoConfig::read(temp.path(), None, [OsStr::new("extra.toml")]);
        assert!(unreadable.digest().is_err());
        assert!(unreadable.sets_env());
        assert!(unreadable.sets_runner());
    }

    /// One drawn `include` entry: a path and its `optional` key (absent, true or false), written
    /// as a bare string (when it has no `optional`) or as a table.
    fn include_text(entries: &[(String, Option<bool>, bool)], single: bool) -> String {
        let written: Vec<String> = entries
            .iter()
            .map(|(path, optional, as_table)| match (optional, as_table) {
                (None, false) => format!("\"{path}\""),
                (None, true) => format!("{{ path = \"{path}\" }}"),
                (Some(optional), _) => format!("{{ path = \"{path}\", optional = {optional} }}"),
            })
            .collect();
        match (single, written.as_slice()) {
            (true, [only]) if !only.starts_with('{') => format!("include = {only}"),
            _ => format!("include = [{}]", written.join(", ")),
        }
    }

    // Property: every form `include` takes is read as its paths and their `optional`, in
    // order, and any other shape is unrecognised.
    #[hegel::test]
    fn include_forms_are_read_in_order(tc: TestCase) {
        let entries: Vec<(String, Option<bool>, bool)> = tc.draw(
            gs::vecs(gs::tuples!(
                gs::text().alphabet("ab./").min_size(1).max_size(5),
                gs::optional(gs::booleans()),
                gs::booleans()
            ))
            .max_size(3),
        );
        let single = tc.draw(gs::booleans());
        let bad = tc.draw(gs::sampled_from(vec![
            "include = 1",
            "include = [1]",
            "include = [{ optional = true }]",
            "include = [{ path = \"a\", optional = \"yes\" }]",
            "include = [{ path = \"a\", other = 1 }]",
            "include = { path = \"a\" }",
        ]));
        let table = parse(include_text(&entries, single).as_bytes()).expect("valid TOML");
        let bad_table = parse(bad.as_bytes()).expect("valid TOML");

        let read = include_entries(&table[INCLUDE_KEY]);

        let expected: Vec<Include> = entries
            .iter()
            .map(|(path, optional, _)| (path.clone(), optional.unwrap_or(false)))
            .collect();
        assert_eq!(read, Some(expected));
        assert_eq!(include_entries(&bad_table[INCLUDE_KEY]), None, "{bad}");
    }

    // Property: an included file counts like any other configuration file, however it is
    // included and however deep: what it declares is found, and changing it changes the digest.
    // A missing optional include is hashed as absent, so creating it changes the digest; a
    // missing required include, an unrecognised `include` or a cycle makes the key unavailable
    // and counts as declaring everything.
    #[hegel::test(test_cases = 30)]
    fn included_files_count_like_any_other(tc: TestCase) {
        let (text, env, runner) = tc.draw(gs::sampled_from(TEXTS.to_vec()));
        let as_table = tc.draw(gs::booleans());
        let nested = tc.draw(gs::booleans());
        let temp = tempfile::tempdir().expect("temporary directory");
        let dot_cargo = temp.path().join(".cargo");
        fs::create_dir_all(dot_cargo.join("more")).expect("create the directories");
        let outside = CargoConfig::read(temp.path(), None, []);
        // `inc.toml` is relative to `.cargo`; a nested include of `more/deep.toml` is relative
        // to `inc.toml`'s directory.
        let leaf = if nested {
            fs::write(dot_cargo.join("inc.toml"), "include = \"more/deep.toml\"")
                .expect("write inc.toml");
            dot_cargo.join("more/deep.toml")
        } else {
            dot_cargo.join("inc.toml")
        };
        fs::write(&leaf, text).expect("write the included file");
        let include = include_text(&[("inc.toml".to_owned(), None, as_table)], true);
        fs::write(dot_cargo.join("config.toml"), &include).expect("write config.toml");
        let read = || CargoConfig::read(temp.path(), None, []);

        let config = read();

        assert_eq!(config.sets_env(), env || outside.sets_env(), "{text:?}");
        assert_eq!(
            config.sets_runner(),
            runner || outside.sets_runner(),
            "{text:?}"
        );
        let digest = config.digest().expect("every file is readable");
        fs::write(&leaf, "build.jobs = 99").expect("change the included file");
        assert_ne!(
            read().digest(),
            Ok(digest),
            "after changing the included file"
        );

        fs::write(
            dot_cargo.join("config.toml"),
            "include = [{ path = \"later.toml\", optional = true }]",
        )
        .expect("write config.toml");
        let without = read().digest().expect("a missing optional include is fine");
        fs::write(dot_cargo.join("later.toml"), "build.jobs = 1").expect("create later.toml");
        assert_ne!(
            read().digest(),
            Ok(without),
            "after creating the optional include"
        );

        for broken in [
            "include = \"missing.toml\"",
            "include = 3",
            "include = \"config.toml\"",
        ] {
            fs::write(dot_cargo.join("config.toml"), broken).expect("write config.toml");
            let config = read();
            assert!(config.digest().is_err(), "{broken}");
            assert!(config.sets_env() && config.sets_runner(), "{broken}");
        }
    }
}
