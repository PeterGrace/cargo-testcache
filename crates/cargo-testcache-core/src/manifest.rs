//! The `[workspace.metadata.testcache]` and `[package.metadata.testcache]` tables.
//!
//! ```toml
//! [workspace.metadata.testcache]
//! inputs = ["migrations/**"]     # files tests read at runtime
//! env = ["DATABASE_IMAGE"]       # variables whose values are part of every key
//! always_run = ["*/live_*"]      # "package/target" patterns that are never skipped
//! spawns = ["myapp-server"]      # binaries spawned by path, relative to target/<profile>
//! ```
//!
//! A package's table uses the same keys and adds to the workspace's. Its `inputs` are relative
//! to the package's directory; the workspace's to the workspace root.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// The key under `metadata` that holds this tool's table.
const TABLE: &str = "testcache";

/// One `[*.metadata.testcache]` table.
// `deny_unknown_fields` turns a misspelt key into an error instead of a silently ignored one;
// `default` makes every key optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestcacheConfig {
    /// Globs of files that tests read at runtime.
    pub inputs: Vec<String>,
    /// Environment variables whose values are part of the key.
    pub env: Vec<String>,
    /// `package/target` patterns that always run.
    pub always_run: Vec<String>,
    /// Paths, relative to the target profile directory, of binaries tests spawn.
    pub spawns: Vec<String>,
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
            source,
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
                source,
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

    const KEYS: [&str; 4] = ["inputs", "env", "always_run", "spawns"];

    fn draw_list(tc: &TestCase) -> Vec<String> {
        tc.draw(gs::vecs(gs::text().max_size(10)).max_size(3))
    }

    fn draw_name(tc: &TestCase) -> String {
        tc.draw(gs::text().alphabet("abc_-").min_size(1).max_size(8))
    }

    // Property: every key of the table is read as written.
    #[hegel::test]
    fn tables_are_read_as_written(tc: TestCase) {
        let config = TestcacheConfig {
            inputs: draw_list(&tc),
            env: draw_list(&tc),
            always_run: draw_list(&tc),
            spawns: draw_list(&tc),
        };
        let metadata = json!({ "other": 1, "testcache": {
            "inputs": config.inputs, "env": config.env,
            "always_run": config.always_run, "spawns": config.spawns,
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
}
