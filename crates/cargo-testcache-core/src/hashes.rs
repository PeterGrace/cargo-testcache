//! Remembered file digests: `<target-dir>/testcache/hashes.json`.
//!
//! Hashing every test executable on every run reads gigabytes from disk on a large workspace,
//! even when nothing changed. Like ccache, sccache or git's index, this cache keeps each hashed
//! file's metadata (size, modification and change times to the nanosecond, inode and device)
//! next to its SHA-256, and reuses the digest while the metadata is unchanged. A warm run then
//! reads only metadata, and a run after a one-crate change hashes only the relinked binaries.
//!
//! **Racy timestamps.** A file written in the same timestamp tick as it was hashed could be
//! written again within that tick without its metadata changing. Following git's "racy git"
//! rule, a digest is remembered only when the file's modification and change times are more
//! than [`RACY_WINDOW`] before the moment its metadata was read; otherwise the file is hashed
//! again next time.
//!
//! **Residual risk.** A file rewritten in place, keeping its size, with its modification time
//! set back (or a clock that jumps backwards) and the same change time, inode and device,
//! would keep a stale digest. The change time cannot be set from user space, and linkers write
//! a new file and rename it (a new inode), so a relink is always seen. A network filesystem
//! whose clock is more than [`RACY_WINDOW`] behind this machine's could defeat the racy rule;
//! `--no-stat-cache` (or `CARGO_TESTCACHE_FULL_HASH=1`) hashes every file in full.
//!
//! The file is read and written only while the store's lock is held, so runs sharing a target
//! directory never interleave their writes.

use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::key::Digest;

/// The cache file, inside the store directory.
const HASHES_FILE: &str = "hashes.json";

/// Bumped whenever [`FileStat`] or the file's layout changes; a file of another version is
/// ignored and rewritten.
const FORMAT_VERSION: u32 = 1;

/// How recent a file's modification or change time may be, relative to when its metadata was
/// read, before its digest is not remembered. Two seconds covers the coarsest common timestamp
/// granularity (FAT's 2 s; HFS+ and ext3 use 1 s, the kernel's coarse clock a few ms).
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// The metadata a remembered digest is valid for.
// Times are kept as seconds and nanoseconds, exactly as `stat` returns them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    len: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    inode: u64,
    device: u64,
}

impl FileStat {
    /// The stat fields of a file's metadata.
    ///
    /// # Arguments
    ///
    /// * `metadata` - What `fs::metadata` returned for the file.
    ///
    /// # Returns
    ///
    /// The fields, or `None` off Unix, where inode and change time are not available, so no
    /// digest is ever remembered there.
    #[cfg(unix)]
    pub fn of(metadata: &Metadata) -> Option<Self> {
        // `MetadataExt` adds the raw `stat` fields to `Metadata`; importing a trait is what
        // makes its methods callable, unlike Python where methods come with the class.
        use std::os::unix::fs::MetadataExt as _;
        Some(Self {
            len: metadata.len(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime_sec: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
            inode: metadata.ino(),
            device: metadata.dev(),
        })
    }

    /// The stat fields of a file's metadata: never available off Unix.
    #[cfg(not(unix))]
    pub fn of(_metadata: &Metadata) -> Option<Self> {
        None
    }

    /// Whether the file's times are far enough before `observed` that a later write would
    /// change them (git's racy rule).
    ///
    /// # Arguments
    ///
    /// * `observed` - When the metadata was read (taken just before reading it).
    ///
    /// # Returns
    ///
    /// `true` when both the modification and change times are more than [`RACY_WINDOW`]
    /// before `observed`; `false` for a recent or future time, or a clock before 1970.
    fn settled_at(&self, observed: SystemTime) -> bool {
        let Ok(since_epoch) = observed.duration_since(UNIX_EPOCH) else {
            return false;
        };
        // `i128` nanoseconds hold any `i64` seconds without overflow.
        let nanos = |sec: i64, nsec: i64| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
        let newest =
            nanos(self.mtime_sec, self.mtime_nsec).max(nanos(self.ctime_sec, self.ctime_nsec));
        // `as_nanos` is a `u128`; a time that does not fit an `i128` is centuries away.
        let observed = i128::try_from(since_epoch.as_nanos()).unwrap_or(i128::MAX);
        let window = i128::try_from(RACY_WINDOW.as_nanos()).unwrap_or(i128::MAX);
        observed.saturating_sub(newest) > window
    }
}

/// A remembered digest and the metadata it is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    stat: FileStat,
    digest: Digest,
}

/// The file's contents.
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    entries: Vec<Entry>,
}

/// A file's digest, and the entry to remember for it if it was hashed and its times have
/// settled. Pass it to [`HashCache::remember`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    digest: Digest,
    fresh: Option<(FileStat, Digest)>,
}

/// The remembered digests, by absolute path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HashCache {
    entries: HashMap<PathBuf, (FileStat, Digest)>,
    /// `false` under `--no-stat-cache`: every file is hashed, and the entries are refreshed.
    reuse: bool,
    /// Whether an entry was added or replaced since loading, so the file needs writing.
    changed: bool,
}

impl HashCache {
    /// Reads the remembered digests from the store directory.
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory, `<target-dir>/testcache`; its lock must be held.
    /// * `reuse` - Whether remembered digests are used; `false` hashes every file.
    ///
    /// # Returns
    ///
    /// The cache. A missing file gives an empty one; an unreadable file, or one of another
    /// format version, is warned about (unless only the version differs) and ignored, since
    /// it only costs hashing.
    pub fn load(dir: &Path, reuse: bool) -> Self {
        let path = dir.join(HASHES_FILE);
        let stored = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Stored>(&bytes)
                .map_err(|error| tracing::warn!(%error, "ignoring an unreadable {HASHES_FILE}"))
                .ok(),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(%error, "cannot read {}", path.display());
                None
            }
        };
        let entries = stored
            .filter(|stored| stored.version == FORMAT_VERSION)
            .map(|stored| {
                stored
                    .entries
                    .into_iter()
                    .map(|entry| (entry.path, (entry.stat, entry.digest)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            entries,
            reuse,
            changed: false,
        }
    }

    /// A file's digest: the remembered one while its metadata is unchanged, else its hash.
    ///
    /// Takes `&self`, so many threads can call it at once; [`HashCache::remember`] records
    /// the result afterwards.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// The digest, with the entry to remember when the file was hashed.
    ///
    /// # Errors
    ///
    /// Any error reading the file's metadata or contents.
    pub fn digest(&self, path: &Path) -> io::Result<Hashed> {
        // Taken before the metadata is read, so it is no later than the moment it describes.
        self.digest_observed(path, SystemTime::now())
    }

    /// [`HashCache::digest`], with the time the metadata is taken to be read at.
    fn digest_observed(&self, path: &Path, observed: SystemTime) -> io::Result<Hashed> {
        let stat = FileStat::of(&fs::metadata(path)?);
        if self.reuse
            && let Some(stat) = stat
            && let Some(&(remembered, digest)) = self.entries.get(path)
            && remembered == stat
        {
            return Ok(Hashed {
                digest,
                fresh: None,
            });
        }
        let digest = Digest::of_file(path)?;
        Ok(Hashed {
            digest,
            fresh: stat
                .filter(|stat| stat.settled_at(observed))
                .map(|stat| (stat, digest)),
        })
    }

    /// Remembers a freshly hashed file's digest, if its times had settled.
    ///
    /// # Arguments
    ///
    /// * `path` - The file that was passed to [`HashCache::digest`].
    /// * `hashed` - What that returned.
    ///
    /// # Returns
    ///
    /// The digest.
    pub fn remember(&mut self, path: &Path, hashed: Hashed) -> Digest {
        if let Some(fresh) = hashed.fresh
            && self.entries.get(path) != Some(&fresh)
        {
            self.entries.insert(path.to_path_buf(), fresh);
            self.changed = true;
        }
        hashed.digest
    }

    /// Writes the cache back, if anything was remembered since it was loaded, leaving out
    /// files that no longer exist and paths that are not UTF-8 (which JSON cannot hold).
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory; its lock must be held.
    ///
    /// # Errors
    ///
    /// When the file cannot be written. It is replaced through a temporary file and a rename,
    /// so a crash never leaves it half-written.
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        if !self.changed {
            return Ok(());
        }
        let mut entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|(path, _)| path.to_str().is_some() && path.exists())
            .map(|(path, &(stat, digest))| Entry {
                path: path.clone(),
                stat,
                digest,
            })
            .collect();
        // Sorted, so the file is the same for the same entries.
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let stored = Stored {
            version: FORMAT_VERSION,
            entries,
        };
        let contents = serde_json::to_vec(&stored).map_err(|source| Error::Json {
            what: "the remembered digests".to_owned(),
            source,
        })?;
        let path = dir.join(HASHES_FILE);
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, contents)
            .map_err(Error::io(format!("writing {}", temporary.display())))?;
        fs::rename(&temporary, &path)
            .map_err(Error::io(format!("replacing {}", path.display())))?;
        self.changed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators as gs;

    use crate::key::testing::draw_digest;

    /// Well after any file a test writes now: its times have settled by then.
    fn later() -> SystemTime {
        SystemTime::now() + RACY_WINDOW * 10
    }

    /// Writes `contents` to `name` in `dir`, returning the path.
    fn write(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write a test file");
        path
    }

    // Property: a file's times are settled exactly when the newer of its modification and
    // change times is more than the racy window before the observation.
    #[hegel::test]
    fn times_settle_only_outside_the_racy_window(tc: TestCase) {
        const BASE_SECS: u64 = 1_700_000_000;
        const NANOS: i64 = 1_000_000_000;
        // Offsets from BASE_SECS, in nanoseconds.
        let time = || {
            tc.draw(
                gs::integers::<i64>()
                    .min_value(-5 * NANOS)
                    .max_value(5 * NANOS),
            )
        };
        let (mtime, ctime) = (time(), time());
        let observed = tc.draw(gs::integers::<i64>().min_value(0).max_value(10 * NANOS));
        // `div_euclid`/`rem_euclid` split like Python's `divmod`, so nanoseconds stay positive.
        let base = i64::try_from(BASE_SECS).expect("fits");
        let stat = FileStat {
            len: 0,
            mtime_sec: base + mtime.div_euclid(NANOS),
            mtime_nsec: mtime.rem_euclid(NANOS),
            ctime_sec: base + ctime.div_euclid(NANOS),
            ctime_nsec: ctime.rem_euclid(NANOS),
            inode: 1,
            device: 1,
        };
        let at = UNIX_EPOCH
            + Duration::from_secs(BASE_SECS)
            + Duration::from_nanos(observed.unsigned_abs());

        let settled = stat.settled_at(at);

        let window = i64::try_from(RACY_WINDOW.as_nanos()).expect("fits");
        assert_eq!(
            settled,
            observed - mtime.max(ctime) > window,
            "{stat:?} at {observed}"
        );
    }

    // Property: a file whose times have settled is remembered with its true digest, and that
    // digest is reused, without reading the file, while its metadata is unchanged.
    #[hegel::test]
    fn a_settled_digest_is_reused_while_the_metadata_matches(tc: TestCase) {
        let contents = tc.draw(gs::binary().max_size(64));
        let planted = draw_digest(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = write(dir.path(), "bin", &contents);
        let mut cache = HashCache::load(dir.path(), true);

        let hashed = cache.digest_observed(&path, later()).expect("readable");
        let digest = cache.remember(&path, hashed);
        let (stat, _) = cache.entries[&path];
        // A planted digest proves the file is not read while its metadata matches.
        cache.entries.insert(path.clone(), (stat, planted));
        let reused = cache.digest_observed(&path, later()).expect("readable");

        assert_eq!(digest, Digest::of_bytes(&contents));
        assert_eq!(
            reused,
            Hashed {
                digest: planted,
                fresh: None
            }
        );
    }

    // Property: a file whose metadata differs from the remembered entry, or any file when
    // reuse is off, is hashed again and gets its true digest.
    #[hegel::test]
    fn changed_metadata_or_no_reuse_hashes_again(tc: TestCase) {
        let contents = tc.draw(gs::binary().max_size(64));
        let reuse = tc.draw(gs::booleans());
        let planted = draw_digest(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = write(dir.path(), "bin", &contents);
        let mut cache = HashCache::load(dir.path(), reuse);
        let stat = FileStat::of(&fs::metadata(&path).expect("metadata")).expect("unix");
        let differs = tc.draw(gs::booleans());
        let remembered = if differs {
            FileStat {
                len: stat.len + 1,
                ..stat
            }
        } else {
            stat
        };
        cache.entries.insert(path.clone(), (remembered, planted));

        let hashed = cache.digest_observed(&path, later()).expect("readable");

        let expected = if reuse && !differs {
            planted
        } else {
            Digest::of_bytes(&contents)
        };
        assert_eq!(hashed.digest, expected, "reuse {reuse}, differs {differs}");
    }

    // Property: a file whose times are within the racy window of the observation is hashed
    // but not remembered, so it is hashed again next time.
    #[hegel::test]
    fn a_racy_file_is_not_remembered(tc: TestCase) {
        let contents = tc.draw(gs::binary().max_size(64));
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = write(dir.path(), "bin", &contents);
        let mut cache = HashCache::load(dir.path(), true);

        // Just written: its change time is now.
        let hashed = cache.digest(&path).expect("readable");
        let digest = cache.remember(&path, hashed);

        assert_eq!(digest, Digest::of_bytes(&contents));
        assert!(cache.entries.is_empty());
        assert!(!cache.changed);
    }

    // Property: saved entries load back unchanged, except those whose file is gone; an
    // unreadable file or one of another version loads as empty.
    #[hegel::test]
    fn entries_survive_a_save_and_load(tc: TestCase) {
        let names =
            tc.draw(gs::vecs(gs::text().alphabet("abc").min_size(1).max_size(3)).max_size(4));
        let removed = tc.draw(gs::booleans());
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut cache = HashCache::load(dir.path(), true);
        for name in &names {
            let path = write(dir.path(), name, name.as_bytes());
            let hashed = cache.digest_observed(&path, later()).expect("readable");
            cache.remember(&path, hashed);
        }
        let mut expected = cache.entries.clone();
        if removed && let Some(name) = names.first() {
            let path = dir.path().join(name);
            fs::remove_file(&path).expect("remove a test file");
            expected.remove(&path);
        }

        cache.save(dir.path()).expect("writable");
        let loaded = HashCache::load(dir.path(), true);

        assert_eq!(loaded.entries, expected);
        assert!(!loaded.changed);
        let corrupt = tc.draw(gs::booleans());
        let replacement = if corrupt {
            "not json".to_owned()
        } else {
            format!("{{\"version\":{},\"entries\":[]}}", FORMAT_VERSION + 1)
        };
        fs::write(dir.path().join(HASHES_FILE), replacement).expect("overwrite");
        assert!(HashCache::load(dir.path(), true).entries.is_empty());
    }
}
