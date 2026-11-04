//! Cache keys: a digest of a test binary and of every runtime input it declares.
//!
//! [`KeyParts`] holds one digest per input named in section 4.2 of the design, and
//! [`KeyParts::key`] folds them into the single [`Digest`] the store compares. Keeping the parts,
//! not only the key, is what lets `--explain` name the input that changed.
//!
//! Every value is encoded with a length prefix before it is hashed, so the parts `("ab", "c")`
//! and `("a", "bc")` can never produce the same key.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
// `as _` imports the trait only for its methods (`Sha256::new`, `update`, `finalize`), without
// taking the name `Digest`, which this module uses for its own type.
use sha2::{Digest as _, Sha256};

/// Bumped whenever [`KeyParts::key`] changes how it encodes the parts, so a key computed one
/// way is never compared with one computed another way.
pub const KEY_FORMAT_VERSION: u32 = 2;

/// Environment variables that are part of every key, besides those the configuration names.
pub const ALWAYS_KEYED_ENV: [&str; 2] = ["RUST_TEST_THREADS", "RUST_MIN_STACK"];

/// Every variable whose name starts with this is part of every key (`RUST_TEST_TIME_UNIT` and
/// its siblings, which turn libtest's time limits on).
pub const ALWAYS_KEYED_ENV_PREFIX: &str = "RUST_TEST_TIME_";

/// A key part, or why it cannot be computed (an unreadable file, an executable outside the
/// build directory). The error is a message for the run reason rather than an
/// [`Error`](crate::Error): a part that cannot be computed makes its binary run, it does not
/// fail the run.
pub type PartResult<T> = std::result::Result<T, String>;

/// How much of a file is read per hash update. Test binaries run to hundreds of megabytes.
const READ_CHUNK: usize = 256 * 1024;

/// A SHA-256 digest, serialized as 64 lowercase hex digits.
// `#[serde(transparent)]` serializes the struct as its one field; `hex::serde` turns the bytes
// into a hex string and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(#[serde(with = "hex::serde")] [u8; 32]);

impl Digest {
    /// Hashes a byte string.
    ///
    /// # Arguments
    ///
    /// * `bytes` - What to hash.
    ///
    /// # Returns
    ///
    /// The SHA-256 of `bytes`.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hashes an OS string, such as an environment variable's value, by its platform bytes.
    ///
    /// # Arguments
    ///
    /// * `value` - What to hash.
    ///
    /// # Returns
    ///
    /// The SHA-256 of the value's bytes.
    pub fn of_os_str(value: &OsStr) -> Self {
        Self::of_bytes(value.as_encoded_bytes())
    }

    /// Hashes a sequence of byte strings, each length-prefixed, after their count.
    ///
    /// # Arguments
    ///
    /// * `fields` - The byte strings, in order. `ExactSizeIterator` means the iterator knows its
    ///   length up front, like a Python sequence rather than a generator.
    ///
    /// # Returns
    ///
    /// A digest that differs whenever the sequence differs, including in where one field ends
    /// and the next begins.
    pub fn of_fields<'a>(fields: impl ExactSizeIterator<Item = &'a [u8]>) -> Self {
        let mut encoder = Encoder::default();
        encoder.fields(fields);
        encoder.finish()
    }

    /// Hashes a file's contents, reading it in chunks.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// The SHA-256 of the file's contents.
    ///
    /// # Errors
    ///
    /// Any error opening or reading the file.
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        // On the heap, to keep 256 KiB off the stack.
        let mut chunk = vec![0_u8; READ_CHUNK];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => hasher.update(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self(hasher.finalize().into()))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

/// Everything a test binary's key is computed from, one field per part of design section 4.2.
///
/// The maps are `BTreeMap`s (sorted by key, like a `dict` that always iterates in sorted order),
/// so the order in which files or variables were found never changes the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyParts {
    /// This tool's version and [`KEY_FORMAT_VERSION`] (part 6).
    pub tool: String,
    /// The test executable (part 1).
    pub binary: Digest,
    /// Binaries the test may spawn: `bin <name>` for `CARGO_BIN_EXE_<name>`, `spawns <path>` for
    /// the configured `spawns` (part 2).
    pub spawned: BTreeMap<String, Digest>,
    /// Declared input files, by workspace-relative path (part 3).
    pub inputs: BTreeMap<String, Digest>,
    /// Declared environment variables: a digest of the value, or `None` when unset (part 4).
    /// Values are hashed so that a secret never reaches the store.
    pub env: BTreeMap<String, Option<Digest>>,
    /// The arguments after `--` (part 5).
    pub args: Digest,
    /// Cargo's configuration files and `--config` values, which can change what a test binary
    /// sees (`[env]`, a runner) without changing the binary (part 7).
    pub cargo_config: Digest,
}

impl KeyParts {
    /// The value of [`KeyParts::tool`] for this build of the tool.
    ///
    /// # Returns
    ///
    /// The crate version and [`KEY_FORMAT_VERSION`], such as `0.1.0+key1`.
    pub fn tool_id() -> String {
        // `env!` reads a variable at compile time; Cargo sets `CARGO_PKG_VERSION` while building.
        format!("{}+key{KEY_FORMAT_VERSION}", env!("CARGO_PKG_VERSION"))
    }

    /// The digest of the arguments after `--`, for [`KeyParts::args`].
    ///
    /// # Arguments
    ///
    /// * `args` - The test binary arguments, in order.
    ///
    /// # Returns
    ///
    /// Their digest; order matters.
    pub fn args_digest(args: &[OsString]) -> Digest {
        Digest::of_fields(args.iter().map(|arg| arg.as_encoded_bytes()))
    }

    /// Folds the parts into the key the store compares.
    ///
    /// # Returns
    ///
    /// A digest of every part, each length-prefixed, in a fixed order.
    pub fn key(&self) -> Digest {
        let mut encoder = Encoder::default();
        encoder.field(self.tool.as_bytes());
        encoder.field(&self.binary.0);
        encoder.digest_map(&self.spawned);
        encoder.digest_map(&self.inputs);
        encoder.count(self.env.len());
        for (name, value) in &self.env {
            encoder.field(name.as_bytes());
            // An unset variable is an empty field; a set one is 32 bytes, so they never collide.
            encoder.field(value.as_ref().map_or(&[][..], |digest| &digest.0[..]));
        }
        encoder.field(&self.args.0);
        encoder.field(&self.cargo_config.0);
        encoder.finish()
    }
}

/// Builds [`KeyParts::env`] from the configured variable names and the environment.
///
/// # Arguments
///
/// * `names` - The variables the configuration declares. [`ALWAYS_KEYED_ENV`] and every
///   variable starting with [`ALWAYS_KEYED_ENV_PREFIX`] are added.
/// * `environment` - A snapshot of the process environment.
///
/// # Returns
///
/// One entry per variable: the digest of its value, or `None` when it is unset.
pub fn env_part<'a>(
    names: impl IntoIterator<Item = &'a str>,
    environment: &BTreeMap<OsString, OsString>,
) -> BTreeMap<String, Option<Digest>> {
    let declared = names.into_iter().chain(ALWAYS_KEYED_ENV).map(|name| {
        let value = environment
            .get(OsStr::new(name))
            .map(|value| Digest::of_os_str(value));
        (name.to_owned(), value)
    });
    // A RUST_TEST_TIME_* variable whose name is not UTF-8 is ignored: libtest reads these by
    // UTF-8 name only.
    let prefixed = environment.iter().filter_map(|(name, value)| {
        let name = name.to_str()?;
        name.starts_with(ALWAYS_KEYED_ENV_PREFIX)
            .then(|| (name.to_owned(), Some(Digest::of_os_str(value))))
    });
    declared.chain(prefixed).collect()
}

/// Feeds length-prefixed values into SHA-256.
#[derive(Default)]
struct Encoder(Sha256);

impl Encoder {
    /// Adds a count (or a length) as 8 little-endian bytes.
    fn count(&mut self, count: usize) {
        self.0.update((count as u64).to_le_bytes());
    }

    /// Adds one byte string, after its length.
    fn field(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.0.update(bytes);
    }

    /// Adds a sequence of byte strings, after their count.
    fn fields<'a>(&mut self, fields: impl ExactSizeIterator<Item = &'a [u8]>) {
        self.count(fields.len());
        for field in fields {
            self.field(field);
        }
    }

    /// Adds a name-to-digest map, after its length.
    fn digest_map(&mut self, map: &BTreeMap<String, Digest>) {
        self.count(map.len());
        for (name, digest) in map {
            self.field(name.as_bytes());
            self.field(&digest.0);
        }
    }

    /// Finishes the hash.
    fn finish(self) -> Digest {
        Digest(self.0.finalize().into())
    }
}

/// Generators shared with other modules' tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    /// Draws an arbitrary digest.
    pub(crate) fn draw_digest(tc: &TestCase) -> Digest {
        Digest::of_bytes(&tc.draw(gs::binary().max_size(16)))
    }

    /// Draws a map of short names to digests.
    pub(crate) fn draw_digest_map(tc: &TestCase) -> BTreeMap<String, Digest> {
        let names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(4));
        names
            .into_iter()
            .map(|name| (name, draw_digest(tc)))
            .collect()
    }

    /// Draws arbitrary key parts.
    pub(crate) fn draw_parts(tc: &TestCase) -> KeyParts {
        let env_names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(4));
        KeyParts {
            tool: tc.draw(gs::text().max_size(8)),
            binary: draw_digest(tc),
            spawned: draw_digest_map(tc),
            inputs: draw_digest_map(tc),
            env: env_names
                .into_iter()
                .map(|name| (name, tc.draw(gs::booleans()).then(|| draw_digest(tc))))
                .collect(),
            args: draw_digest(tc),
            cargo_config: draw_digest(tc),
        }
    }

    /// How many parts [`mutate_one_part`] can change.
    pub(crate) const PART_COUNT: u8 = 7;

    /// Changes exactly one part of `parts`, chosen by `which` (below [`PART_COUNT`]).
    pub(crate) fn mutate_one_part(parts: &mut KeyParts, which: u8, name: String) {
        let rehash = |digest: &mut Digest| *digest = Digest::of_bytes(&digest.0);
        match which {
            0 => parts.tool.push('+'),
            1 => rehash(&mut parts.binary),
            2 => rehash(parts.spawned.entry(name).or_insert(parts.binary)),
            3 => rehash(parts.inputs.entry(name).or_insert(parts.binary)),
            4 => {
                let flipped = match parts.env.get(&name) {
                    Some(Some(_)) => None,
                    Some(None) => Some(parts.binary),
                    None => None,
                };
                parts.env.insert(name, flipped);
            }
            5 => rehash(&mut parts.args),
            _ => rehash(&mut parts.cargo_config),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{PART_COUNT, draw_digest, draw_parts, mutate_one_part};
    use super::*;

    use std::collections::BTreeSet;
    use std::io::Write;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    // Property (design 9): the key is a function of its parts, and survives the store's JSON.
    #[hegel::test]
    fn the_key_is_a_function_of_its_parts(tc: TestCase) {
        let parts = draw_parts(&tc);

        let reloaded: KeyParts =
            serde_json::from_str(&serde_json::to_string(&parts).expect("parts serialize"))
                .expect("parts deserialize");

        assert_eq!(parts.key(), parts.clone().key());
        assert_eq!(reloaded, parts);
        assert_eq!(reloaded.key(), parts.key());
    }

    // Property (design 9): changing any one part changes the key.
    #[hegel::test]
    fn changing_any_one_part_changes_the_key(tc: TestCase) {
        let parts = draw_parts(&tc);
        let which = tc.draw(gs::integers::<u8>().max_value(PART_COUNT - 1));
        let name = tc.draw(gs::text().max_size(8));
        let mut changed = parts.clone();

        mutate_one_part(&mut changed, which, name);

        assert_ne!(parts.key(), changed.key());
    }

    // Property (design 9): the order inputs were found in does not change the key.
    #[hegel::test]
    fn input_order_does_not_change_the_key(tc: TestCase) {
        let mut parts = draw_parts(&tc);
        let names = tc.draw(gs::vecs(gs::text().max_size(8)).max_size(6));
        // Collected into a map first: with a repeated name the later entry would win, and
        // reordering would then change which one that is, which is not what is under test.
        let unique: BTreeMap<String, Digest> = names
            .into_iter()
            .map(|name| (name, draw_digest(&tc)))
            .collect();
        let entries: Vec<(String, Digest)> = unique.into_iter().collect();
        let rotation = tc.draw(gs::integers::<usize>().max_value(entries.len()));
        let mut reordered = entries.clone();
        reordered.rotate_left(rotation);
        reordered.reverse();

        parts.inputs = entries.into_iter().collect();
        let mut other = parts.clone();
        other.inputs = reordered.into_iter().collect();

        assert_eq!(parts.key(), other.key());
    }

    // Property (design 9): length prefixes keep ("ab", "c") and ("a", "bc") apart.
    #[hegel::test]
    fn length_prefixes_separate_fields(tc: TestCase) {
        let bytes = tc.draw(gs::binary().min_size(1).max_size(32));
        let first = tc.draw(gs::integers::<usize>().max_value(bytes.len()));
        let offset = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        // Any split point other than `first`.
        let second = (first + 1 + offset) % (bytes.len() + 1);

        let split_at = |at: usize| Digest::of_fields([&bytes[..at], &bytes[at..]].into_iter());

        assert_ne!(split_at(first), split_at(second));
    }

    // Property: hashing a file gives the digest of its bytes, across chunk boundaries.
    #[hegel::test(test_cases = 30)]
    fn a_file_hashes_like_its_bytes(tc: TestCase) {
        let unit = tc.draw(gs::binary().min_size(1).max_size(64));
        let repeats = tc.draw(gs::integers::<usize>().max_value(READ_CHUNK / 16));
        let contents = unit.repeat(repeats);
        let mut file = tempfile::NamedTempFile::new().expect("create a temporary file");
        file.write_all(&contents).expect("write it");

        let digest = Digest::of_file(file.path()).expect("hash it");

        assert_eq!(digest, Digest::of_bytes(&contents));
    }

    // Property: env_part has one entry per declared or always-keyed variable, plus every
    // RUST_TEST_TIME_* variable, and its value is present exactly when the variable is set.
    #[hegel::test]
    fn env_part_covers_declared_and_always_keyed_variables(tc: TestCase) {
        let pool = [
            "A",
            "B",
            "RUST_TEST_THREADS",
            "RUST_TEST_TIME_UNIT",
            "RUST_MIN_STACK",
        ];
        let declared = tc.draw(gs::vecs(gs::sampled_from(&pool).print_as_debug()).max_size(3));
        let set = tc.draw(gs::vecs(gs::sampled_from(&pool).print_as_debug()).max_size(5));
        let environment: BTreeMap<OsString, OsString> = set
            .iter()
            .map(|name| (name.into(), "value".into()))
            .collect();

        let part = env_part(declared.iter().copied(), &environment);

        for name in declared.iter().chain(&ALWAYS_KEYED_ENV) {
            let is_set = set.contains(name);
            assert_eq!(part.get(*name).map(Option::is_some), Some(is_set), "{name}");
        }
        let expected_names: BTreeSet<&str> = declared
            .iter()
            .copied()
            .chain(ALWAYS_KEYED_ENV)
            .chain(
                set.iter()
                    .copied()
                    .filter(|name| name.starts_with(ALWAYS_KEYED_ENV_PREFIX)),
            )
            .collect();
        let actual_names: BTreeSet<&str> = part.keys().map(String::as_str).collect();
        assert_eq!(actual_names, expected_names);
        let value_digest = Digest::of_os_str(OsStr::new("value"));
        for digest in part.values().flatten() {
            assert_eq!(*digest, value_digest);
        }
    }

    // Property: the same digest under the same name in a different part gives a different key.
    #[hegel::test]
    fn moving_an_entry_between_parts_changes_the_key(tc: TestCase) {
        let parts = draw_parts(&tc);
        let name = tc.draw(gs::text().max_size(8));
        let digest = draw_digest(&tc);
        let mut in_spawned = parts.clone();
        in_spawned.spawned.insert(name.clone(), digest);
        let mut in_inputs = parts.clone();
        in_inputs.inputs.insert(name.clone(), digest);
        let mut in_env = parts;
        in_env.inputs.remove(&name);
        in_env.env.insert(name.clone(), Some(digest));
        in_inputs.spawned.remove(&name);
        in_spawned.inputs.remove(&name);
        in_spawned.env.remove(&name);
        in_inputs.env.remove(&name);

        assert_ne!(in_spawned.key(), in_inputs.key());
        assert_ne!(in_inputs.key(), in_env.key());
    }

    // Property: a digest is 64 hex digits in JSON and reads back unchanged.
    #[hegel::test]
    fn digests_round_trip_through_json(tc: TestCase) {
        let digest = draw_digest(&tc);

        let json = serde_json::to_string(&digest).expect("serialize");

        assert_eq!(json, format!("\"{digest}\""));
        assert_eq!(digest.to_string().len(), 64);
        assert_eq!(
            serde_json::from_str::<Digest>(&json).expect("deserialize"),
            digest
        );
    }
}
