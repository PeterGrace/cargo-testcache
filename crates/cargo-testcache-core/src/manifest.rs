//! The `[workspace.metadata.testcache]` and `[package.metadata.testcache]` tables.
//!
//! ```toml
//! [workspace.metadata.testcache]
//! inputs = ["migrations/**"]     # files tests read at runtime
//! optional_inputs = [".env"]     # the same, but a glob that matches nothing is not warned about
//! env = ["DATABASE_IMAGE"]       # variables whose values are part of every key
//! always_run = ["*/live_*"]      # "package/target" patterns that are never skipped
//! spawns = ["myapp-server"]      # binaries spawned by path, relative to target/<profile>
//! ```
//!
//! A package's table uses the same keys and adds to the workspace's. Its `inputs` and
//! `optional_inputs` are relative to the package's directory; the workspace's to the workspace
//! root.
//!
//! A package's table may also narrow `inputs`, `optional_inputs` and `spawns` to some of its
//! test targets:
//!
//! ```toml
//! [package.metadata.testcache.targets.migrate]   # integration test `tests/migrate.rs`
//! inputs = ["migrations/**"]                     # relative to the package, as above
//! spawns = ["helper-server"]                     # relative to target/<profile>, as above
//! ```
//!
//! A `targets.<name>` table applies to every test binary of the package whose target is named
//! `<name>`, whatever its kind; `lib` also names the package's library. It holds only `inputs`,
//! `optional_inputs` and `spawns`: `env` and `always_run` stay package-wide. The workspace's
//! table has no `targets`.

use std::collections::BTreeMap;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::Value;

use crate::discover::TargetKind;
use crate::error::{Error, Result};

/// The key under `metadata` that holds this tool's table.
const TABLE: &str = "testcache";

/// The key of a package table that holds its per-target tables.
const TARGETS: &str = "targets";

/// The `targets` key that names the package's library, whatever its crate name.
pub const LIB_TARGET: &str = "lib";

/// Where the workspace's table is, in messages.
pub const WORKSPACE_LOCATION: &str = "[workspace.metadata]";

/// One `[*.metadata.testcache]` table.
// `deny_unknown_fields` turns a misspelt key into an error instead of a silently ignored one;
// `default` makes every key optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestcacheConfig {
    /// Globs of files that tests read at runtime.
    pub inputs: Vec<String>,
    /// Globs like `inputs` for files that may be absent: one that matches nothing is expected.
    pub optional_inputs: Vec<String>,
    /// Environment variables whose values are part of the key.
    pub env: Vec<String>,
    /// `package/target` patterns that always run.
    pub always_run: Vec<String>,
    /// Paths, relative to the target profile directory, of binaries tests spawn.
    pub spawns: Vec<String>,
    /// Per-target `inputs`, `optional_inputs` and `spawns`, by target name; only in a package's
    /// table.
    // A `BTreeMap` iterates in key order, so the targets' spawns enter a key in a fixed order.
    pub targets: BTreeMap<String, TargetConfig>,
}

/// One `[package.metadata.testcache.targets.<name>]` table: what only that target's test
/// binaries read or spawn, on top of the package's and the workspace's.
// `deny_unknown_fields` also refuses `env` and `always_run` here: they stay package-wide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetConfig {
    /// Globs of files the target's tests read, relative to the package directory.
    pub inputs: Vec<String>,
    /// Globs like `inputs` for files that may be absent.
    pub optional_inputs: Vec<String>,
    /// Paths, relative to the target profile directory, of binaries the target's tests spawn.
    pub spawns: Vec<String>,
}

/// Whether a `targets.<name>` key applies to a test binary.
///
/// # Arguments
///
/// * `key` - The key under `targets`.
/// * `kind` - The binary's target kind.
/// * `name` - The binary's target name.
///
/// # Returns
///
/// `true` when the key is the target's name (whatever its kind, so a bin and an integration
/// test of the same name both match), or is `lib` and the binary is the library's.
pub fn target_matches(key: &str, kind: TargetKind, name: &str) -> bool {
    key == name || (key == LIB_TARGET && kind == TargetKind::Lib)
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

    /// Reads the workspace's table, which may not have `targets`.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The `[workspace.metadata]` value, if any.
    ///
    /// # Returns
    ///
    /// The table, or the default when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidManifest`], naming [`WORKSPACE_LOCATION`], when the table has `targets`
    /// (even an empty one), an unknown key or a value of the wrong type.
    pub fn from_workspace_metadata(metadata: Option<&Value>) -> Result<Self> {
        let has_targets = metadata
            .and_then(|metadata| metadata.get(TABLE))
            .is_some_and(|table| table.get(TARGETS).is_some());
        if has_targets {
            return Err(Error::InvalidManifest {
                location: WORKSPACE_LOCATION.to_owned(),
                message: "`targets` is only allowed in a package's [package.metadata.testcache] \
                    table, since target names belong to a package"
                    .to_owned(),
            });
        }
        Self::from_metadata(metadata, WORKSPACE_LOCATION)
    }

    /// The per-target tables that apply to a test binary of this package.
    ///
    /// # Arguments
    ///
    /// * `kind` - The binary's target kind.
    /// * `name` - The binary's target name.
    ///
    /// # Returns
    ///
    /// Each matching `targets` key with its table, in key order (see [`target_matches`]).
    pub fn targets_matching<'a>(
        &'a self,
        kind: TargetKind,
        name: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a TargetConfig)> {
        self.targets
            .iter()
            .filter(move |(key, _)| target_matches(key, kind, name))
            .map(|(key, table)| (key.as_str(), table))
    }
}

/// Compiles one glob, with `*` and `?` stopping at `/` (only `**` crosses directories).
///
/// Globs use `/` separators on every platform, `\` escapes the next character on every
/// platform, and matching is case-sensitive, even on case-insensitive file systems.
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
        .backslash_escape(true)
        .build()
        .map_err(|source| Error::InvalidGlob {
            pattern: pattern.to_owned(),
            reason: source.to_string(),
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
        builder
            .build()
            .map(Self)
            .map_err(|source| Error::InvalidGlob {
                // `build` fails only on whole-set limits, so no single user pattern is to blame.
                pattern: "the always_run patterns together".to_owned(),
                reason: source.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};
    use serde_json::json;

    const KEYS: [&str; 6] = [
        "inputs",
        "optional_inputs",
        "env",
        "always_run",
        "spawns",
        "targets",
    ];

    /// The keys a `targets.<name>` table may have.
    const TARGET_KEYS: [&str; 3] = ["inputs", "optional_inputs", "spawns"];

    fn draw_list(tc: &TestCase) -> Vec<String> {
        tc.draw(gs::vecs(gs::text().max_size(10)).max_size(3))
    }

    fn draw_name(tc: &TestCase) -> String {
        tc.draw(gs::text().alphabet("abc_-").min_size(1).max_size(8))
    }

    /// Draws a `targets` map of a few target tables.
    fn draw_targets(tc: &TestCase) -> BTreeMap<String, TargetConfig> {
        let names = tc.draw(gs::vecs(gs::text().max_size(6)).max_size(3));
        names
            .into_iter()
            .map(|name| {
                let table = TargetConfig {
                    inputs: draw_list(tc),
                    optional_inputs: draw_list(tc),
                    spawns: draw_list(tc),
                };
                (name, table)
            })
            .collect()
    }

    /// A `targets` map as the JSON `cargo metadata` gives for it.
    fn targets_json(targets: &BTreeMap<String, TargetConfig>) -> Value {
        Value::Object(
            targets
                .iter()
                .map(|(name, table)| {
                    let value = json!({
                        "inputs": table.inputs,
                        "optional_inputs": table.optional_inputs,
                        "spawns": table.spawns,
                    });
                    (name.clone(), value)
                })
                .collect(),
        )
    }

    // Property: every key of the table is read as written, `targets` included.
    #[hegel::test]
    fn tables_are_read_as_written(tc: TestCase) {
        let config = TestcacheConfig {
            inputs: draw_list(&tc),
            optional_inputs: draw_list(&tc),
            env: draw_list(&tc),
            always_run: draw_list(&tc),
            spawns: draw_list(&tc),
            targets: draw_targets(&tc),
        };
        let metadata = json!({ "other": 1, "testcache": {
            "inputs": config.inputs, "optional_inputs": config.optional_inputs, "env": config.env,
            "always_run": config.always_run, "spawns": config.spawns,
            "targets": targets_json(&config.targets),
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
            Value::Object(serde_json::Map::from_iter([(
                other_key,
                json!({ "inputs": ["x"] }),
            )]))
        });

        let read = TestcacheConfig::from_metadata(metadata.as_ref(), "package `p`")
            .expect("no table is valid");

        assert_eq!(read, TestcacheConfig::default());
    }

    // Property: an unknown key is an error that names the table it is in.
    #[hegel::test]
    fn unknown_keys_are_refused_with_their_location(tc: TestCase) {
        let location = draw_name(&tc);
        let key = tc.draw(gs::text().filter(|key: &String| !KEYS.contains(&key.as_str())));
        let metadata = json!({ "testcache": { key: ["x"] } });

        let error = TestcacheConfig::from_metadata(Some(&metadata), &location)
            .expect_err("unknown keys are refused");

        assert!(
            matches!(&error, Error::InvalidManifest { location: at, .. } if *at == location),
            "{error:?}"
        );
    }

    // Property: a value of the wrong type is an error that names the table it is in.
    #[hegel::test]
    fn wrong_types_are_refused_with_their_location(tc: TestCase) {
        let location = draw_name(&tc);
        let key = tc.draw(gs::sampled_from(KEYS.to_vec()));
        // The array has at least one number: an empty array is a valid list of strings.
        let value = tc.draw(hegel::one_of!(
            gs::text().map(Value::from),
            gs::integers::<i64>().map(Value::from),
            gs::vecs(gs::integers::<i64>())
                .min_size(1)
                .max_size(3)
                .map(Value::from),
        ));
        let mut table = serde_json::Map::new();
        table.insert(key.to_owned(), value);
        let metadata = json!({ "testcache": table });

        let error = TestcacheConfig::from_metadata(Some(&metadata), &location)
            .expect_err("a wrong type is refused");

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
    // Property: a target table holds only `inputs`, `optional_inputs` and `spawns`; any other
    // key, `env` and `always_run` included, is an error that names the package table it is in.
    #[hegel::test]
    fn target_tables_refuse_other_keys(tc: TestCase) {
        let location = draw_name(&tc);
        let target = tc.draw(gs::text().max_size(6));
        let key = tc.draw(hegel::one_of!(
            gs::sampled_from(vec!["env".to_owned(), "always_run".to_owned()]),
            gs::text().filter(|key: &String| !TARGET_KEYS.contains(&key.as_str())),
        ));
        let mut table = serde_json::Map::new();
        table.insert(key, json!(["x"]));
        let mut targets = serde_json::Map::new();
        targets.insert(target, Value::Object(table));
        let metadata = json!({ "testcache": { "targets": targets } });

        let error = TestcacheConfig::from_metadata(Some(&metadata), &location)
            .expect_err("only inputs, optional_inputs and spawns are allowed in a target table");

        assert!(
            matches!(&error, Error::InvalidManifest { location: at, .. } if *at == location),
            "{error:?}"
        );
    }

    // Property: the workspace's table may not have `targets`, even an empty one, and the error
    // names the workspace table; the same table without `targets` is read.
    #[hegel::test]
    fn the_workspace_table_refuses_targets(tc: TestCase) {
        let targets = draw_targets(&tc);
        let inputs = draw_list(&tc);
        let with_targets =
            json!({ "testcache": { "inputs": inputs, "targets": targets_json(&targets) } });
        let without_targets = json!({ "testcache": { "inputs": inputs } });

        let refused = TestcacheConfig::from_workspace_metadata(Some(&with_targets));
        let read = TestcacheConfig::from_workspace_metadata(Some(&without_targets))
            .expect("a workspace table without targets is valid");

        assert!(
            matches!(
                &refused,
                Err(Error::InvalidManifest { location, .. }) if location == WORKSPACE_LOCATION
            ),
            "{refused:?}"
        );
        assert_eq!(read.inputs, inputs);
        assert!(read.targets.is_empty());
    }

    // Property: a target key applies to every binary with that target name, whatever its kind,
    // and `lib` also applies to the library; no other key applies.
    #[hegel::test]
    fn target_keys_match_by_name_and_lib_matches_the_library(tc: TestCase) {
        let kind = tc.draw(
            gs::sampled_from(&[
                TargetKind::Lib,
                TargetKind::Bin,
                TargetKind::Test,
                TargetKind::Bench,
                TargetKind::Example,
            ])
            .print_as_debug(),
        );
        // `draw_name`'s alphabet cannot spell `lib`, so the name is never the library key.
        let name = draw_name(&tc);
        let other = tc.draw(
            gs::text()
                .max_size(8)
                .filter(|key: &String| *key != name && key != LIB_TARGET),
        );
        let config = TestcacheConfig {
            targets: [name.clone(), other.clone(), LIB_TARGET.to_owned()]
                .into_iter()
                .map(|key| (key, TargetConfig::default()))
                .collect(),
            ..TestcacheConfig::default()
        };

        let matching: Vec<&str> = config
            .targets_matching(kind, &name)
            .map(|(key, _)| key)
            .collect();

        let mut expected = vec![name.as_str()];
        if kind == TargetKind::Lib {
            expected.push(LIB_TARGET);
        }
        expected.sort_unstable();
        assert_eq!(matching, expected, "{kind:?} {name:?}, other {other:?}");
    }
}
