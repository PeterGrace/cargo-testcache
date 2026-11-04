//! The record of passing test binaries: `<target-dir>/testcache/passes.jsonl`.
//!
//! One JSON object per line, appended only after a binary exits 0. An advisory lock on
//! `<target-dir>/testcache/lock` serializes runs that share a target directory, as Cargo's own
//! build lock does. Opening the store compacts the file to the newest line per target, so it
//! stays one line per test binary.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::discover::TargetId;
use crate::error::{Error, Result};
use crate::key::{Digest, KeyParts};

/// The records file, inside the store directory.
const PASSES_FILE: &str = "passes.jsonl";
/// The lock file, inside the store directory.
const LOCK_FILE: &str = "lock";

/// One recorded pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassRecord {
    /// The test binary's target.
    pub target: TargetId,
    /// [`KeyParts::key`] of `parts`.
    pub key: Digest,
    /// What the key was computed from, for `--explain`.
    pub parts: KeyParts,
    /// When it passed, in seconds since the Unix epoch.
    pub passed_at: u64,
    /// How long it ran, in milliseconds.
    pub duration_ms: u64,
}

/// The open store. Holds the lock until dropped.
#[derive(Debug)]
pub struct Store {
    records: HashMap<TargetId, PassRecord>,
    passes: File,
    /// Never read: holding the open file is what holds the lock. The leading `_` tells the
    /// compiler that is intended.
    _lock: File,
}

impl Store {
    /// Opens the store in `dir`, creating it, waiting for the lock, and compacting the records.
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory, `<target-dir>/testcache`.
    ///
    /// # Returns
    ///
    /// The store, holding the lock.
    ///
    /// # Errors
    ///
    /// When the directory or files cannot be created, read, locked or rewritten. Lines that do
    /// not parse are dropped with a warning, not an error.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).map_err(Error::io(format!("creating {}", dir.display())))?;
        let lock = lock(dir)?;
        let path = dir.join(PASSES_FILE);
        let (records, lines) = read(&path)?;
        if lines != records.len() {
            rewrite(&path, &records)?;
        }
        let passes = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(Error::io(format!("opening {}", path.display())))?;
        Ok(Self {
            records,
            passes,
            _lock: lock,
        })
    }

    /// The newest recorded pass of a target.
    ///
    /// # Arguments
    ///
    /// * `target` - The target.
    ///
    /// # Returns
    ///
    /// Its record, if it ever passed.
    pub fn lookup(&self, target: &TargetId) -> Option<&PassRecord> {
        self.records.get(target)
    }

    /// Appends a pass.
    ///
    /// # Arguments
    ///
    /// * `record` - The pass.
    ///
    /// # Errors
    ///
    /// When the line cannot be written.
    pub fn record(&mut self, record: PassRecord) -> Result<()> {
        let mut line = serde_json::to_vec(&record).map_err(|source| Error::Json {
            what: "a pass record".to_owned(),
            source,
        })?;
        line.push(b'\n');
        // One `write_all` on a file opened for appending; the lock keeps other runs out.
        self.passes
            .write_all(&line)
            .map_err(Error::io("appending to the store"))?;
        self.records.insert(record.target.clone(), record);
        Ok(())
    }

    /// Forgets every recorded pass.
    ///
    /// # Arguments
    ///
    /// * `dir` - The store directory.
    ///
    /// # Returns
    ///
    /// Whether there was anything to forget.
    ///
    /// # Errors
    ///
    /// When the lock cannot be taken or the file cannot be removed.
    pub fn clear(dir: &Path) -> Result<bool> {
        if !dir.is_dir() {
            return Ok(false);
        }
        let _lock = lock(dir)?;
        // Only the records go: the lock file stays, because removing a lock file other runs may
        // be waiting on would let two runs hold "the" lock on different inodes.
        let path = dir.join(PASSES_FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io {
                action: format!("removing {}", path.display()),
                source,
            }),
        }
    }
}

/// Takes the store's lock, waiting (with a message) if another run holds it.
fn lock(dir: &Path) -> Result<File> {
    let path = dir.join(LOCK_FILE);
    // Rust's standard library opens every file with close-on-exec (O_CLOEXEC) on Unix, so a
    // test binary, or a daemon such as sccache that it starts, never inherits the lock.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(Error::io(format!("opening {}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            tracing::info!(lock = %path.display(), "waiting for another cargo-testcache run");
            file.lock()
                .map_err(Error::io(format!("locking {}", path.display())))?;
        }
        Err(TryLockError::Error(source)) => {
            return Err(Error::Io {
                action: format!("locking {}", path.display()),
                source,
            });
        }
    }
    Ok(file)
}

/// Reads every record, keeping the last per target.
///
/// # Returns
///
/// The records, and how many lines the file had (more lines than records means it needs
/// compacting).
fn read(path: &Path) -> Result<(HashMap<TargetId, PassRecord>, usize)> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((HashMap::new(), 0)),
        Err(source) => {
            return Err(Error::Io {
                action: format!("reading {}", path.display()),
                source,
            });
        }
    };
    let mut records = HashMap::new();
    let mut lines = 0;
    // `split` yields raw bytes, so a line that is not UTF-8 is dropped like any bad line
    // instead of failing the whole read.
    for line in BufReader::new(file).split(b'\n') {
        let line = line.map_err(Error::io(format!("reading {}", path.display())))?;
        if line.is_empty() {
            continue;
        }
        lines += 1;
        match serde_json::from_slice::<PassRecord>(&line) {
            Ok(record) => {
                records.insert(record.target.clone(), record);
            }
            Err(error) => tracing::warn!(%error, "dropping an unreadable line from the store"),
        }
    }
    Ok((records, lines))
}

/// Replaces the file with one line per record, through a temporary file and a rename.
///
/// The rename is atomic, so a process crash never leaves a half-written file. There is no
/// `fsync`, so a power loss can at worst leave an unreadable file; that costs only reruns,
/// because lines that do not parse are dropped on the next open.
fn rewrite(path: &Path, records: &HashMap<TargetId, PassRecord>) -> Result<()> {
    let mut sorted: Vec<&PassRecord> = records.values().collect();
    sorted.sort_by(|a, b| a.target.cmp(&b.target));
    let mut contents = Vec::new();
    for record in sorted {
        serde_json::to_writer(&mut contents, record).map_err(|source| Error::Json {
            what: "a pass record".to_owned(),
            source,
        })?;
        contents.push(b'\n');
    }
    let temporary = path.with_extension("jsonl.tmp");
    fs::write(&temporary, contents)
        .map_err(Error::io(format!("writing {}", temporary.display())))?;
    fs::rename(&temporary, path).map_err(Error::io(format!("replacing {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    use crate::discover::TargetKind;
    use crate::key::testing::draw_parts;

    const NAMES: [&str; 3] = ["a", "b", "c"];

    /// Held by tests that spawn a process, and by the test that checks the lock is released.
    /// Between `fork` and `exec` a child briefly shares every open file of this process,
    /// including other tests' lock files, and a `flock` lasts until the last copy closes. Without
    /// this, a spawn in one test can make another test's released lock look still held.
    static SPAWN_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn target(name: &str) -> TargetId {
        TargetId {
            package_id: "p".to_owned(),
            kind: TargetKind::Test,
            name: name.to_owned(),
        }
    }

    fn draw_record(tc: &TestCase, name: &str) -> PassRecord {
        let parts = draw_parts(tc);
        PassRecord {
            target: target(name),
            key: parts.key(),
            parts,
            passed_at: tc.draw(gs::integers::<u64>()),
            duration_ms: tc.draw(gs::integers::<u64>()),
        }
    }

    fn draw_records(tc: &TestCase) -> Vec<PassRecord> {
        let names = tc.draw(gs::vecs(gs::sampled_from(&NAMES).print_as_debug()).max_size(6));
        names
            .into_iter()
            .map(|name| draw_record(tc, name))
            .collect()
    }

    /// The newest record per target, as `lookup` should see them.
    fn newest(records: &[PassRecord]) -> HashMap<TargetId, PassRecord> {
        records
            .iter()
            .map(|record| (record.target.clone(), record.clone()))
            .collect()
    }

    fn line_count(dir: &Path) -> usize {
        fs::read(dir.join(PASSES_FILE)).map_or(0, |bytes| {
            bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .count()
        })
    }

    // Property (design 9): opening compacts the file to the newest line per target, and drops
    // lines it cannot read.
    #[hegel::test(test_cases = 50)]
    fn opening_keeps_the_newest_line_per_target(tc: TestCase) {
        let records = draw_records(&tc);
        let garbage = tc.draw(gs::booleans());
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut contents = Vec::new();
        for record in &records {
            serde_json::to_writer(&mut contents, record).expect("serialize");
            contents.push(b'\n');
            if garbage {
                contents.extend_from_slice(b"{not json\xff\n");
            }
        }
        fs::write(dir.path().join(PASSES_FILE), contents).expect("write the file");

        let store = Store::open(dir.path()).expect("open");

        let expected = newest(&records);
        for name in NAMES {
            assert_eq!(
                store.lookup(&target(name)),
                expected.get(&target(name)),
                "{name}"
            );
        }
        drop(store);
        assert_eq!(line_count(dir.path()), expected.len());
    }

    // Property: recorded passes survive reopening, and `clear` forgets them all.
    #[hegel::test(test_cases = 50)]
    fn records_persist_until_cleared(tc: TestCase) {
        let records = draw_records(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");

        let mut store = Store::open(dir.path()).expect("open");
        for record in &records {
            store.record(record.clone()).expect("record");
        }
        drop(store);
        let reopened = Store::open(dir.path()).expect("reopen");
        let expected = newest(&records);
        for name in NAMES {
            assert_eq!(
                reopened.lookup(&target(name)),
                expected.get(&target(name)),
                "{name}"
            );
        }
        drop(reopened);

        assert!(Store::clear(dir.path()).expect("clear"));
        let cleared = Store::open(dir.path()).expect("open after clear");
        assert!(
            NAMES
                .iter()
                .all(|name| cleared.lookup(&target(name)).is_none())
        );
    }

    // Property (design 9): concurrent writers under the lock never lose or interleave lines,
    // even when every open compacts a file with duplicates and the writers share targets.
    #[hegel::test(test_cases = 10)]
    fn concurrent_writers_never_interleave(tc: TestCase) {
        let seed = draw_records(&tc);
        let per_thread = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let batches: Vec<Vec<PassRecord>> = (0..2)
            .map(|_| {
                (0..per_thread)
                    .map(|_| {
                        let name = tc.draw(gs::sampled_from(&NAMES).print_as_debug());
                        draw_record(&tc, name)
                    })
                    .collect()
            })
            .collect();
        let dir = tempfile::tempdir().expect("temporary directory");
        // Every seed line twice, so the first open compacts, and so does any later open that
        // follows an append to a target already in the file.
        let mut contents = Vec::new();
        for record in seed.iter().chain(&seed) {
            serde_json::to_writer(&mut contents, record).expect("serialize");
            contents.push(b'\n');
        }
        fs::write(dir.path().join(PASSES_FILE), contents).expect("write the seed");

        // `thread::scope` lets the threads borrow `batches`, and joins them all at the end of
        // the block. `move` copies the two references into each thread.
        let path = dir.path();
        std::thread::scope(|scope| {
            for batch in &batches {
                scope.spawn(move || {
                    let mut store = Store::open(path).expect("open");
                    for record in batch {
                        store.record(record.clone()).expect("record");
                    }
                });
            }
        });

        // Which thread wrote last is not deterministic, so each target's final record may be
        // the last write of either thread; a target neither wrote keeps its newest seed line.
        let store = Store::open(dir.path()).expect("reopen");
        let seeded = newest(&seed);
        let mut targets: Vec<TargetId> = seeded.keys().cloned().collect();
        for name in NAMES {
            let target = target(name);
            let candidates: Vec<&PassRecord> = batches
                .iter()
                .filter_map(|batch| batch.iter().rev().find(|record| record.target == target))
                .collect();
            if candidates.is_empty() {
                assert_eq!(store.lookup(&target), seeded.get(&target), "{name}");
                continue;
            }
            let found = store.lookup(&target).expect("a written target is recorded");
            assert!(
                candidates.contains(&found),
                "{name}: {found:?} is no thread's last write"
            );
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        drop(store);
        assert_eq!(line_count(dir.path()), targets.len());
    }

    // Property: while a store is open its lock is held, and dropping it releases the lock.
    #[hegel::test(test_cases = 20)]
    fn the_lock_is_held_while_the_store_is_open(tc: TestCase) {
        let _guard = SPAWN_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let records = draw_records(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut store = Store::open(dir.path()).expect("open");
        for record in records {
            store.record(record).expect("record");
        }
        let probe = || {
            OpenOptions::new()
                .write(true)
                .open(dir.path().join(LOCK_FILE))
                .expect("open lock")
        };

        let held = probe().try_lock();
        assert!(matches!(held, Err(TryLockError::WouldBlock)), "{held:?}");
        drop(store);
        let released = probe().try_lock();
        assert!(released.is_ok(), "{released:?}");
    }

    // Property (design 9): a process spawned while the store is open does not inherit the lock.
    #[cfg(target_os = "linux")]
    #[hegel::test(test_cases = 5)]
    fn spawned_processes_do_not_inherit_the_lock(tc: TestCase) {
        let records = draw_records(&tc);
        let dir = tempfile::tempdir().expect("temporary directory");
        let mut store = Store::open(dir.path()).expect("open");
        for record in records {
            store.record(record).expect("record");
        }

        let guard = SPAWN_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                r#"for fd in /proc/self/fd/*; do readlink "$fd"; done"#,
            ])
            .output()
            .expect("run sh");
        drop(guard);

        // Compare canonical paths: `readlink` reports resolved paths, so a symlinked temporary
        // directory would otherwise make the assertion pass vacuously.
        let lock_path = fs::canonicalize(dir.path().join(LOCK_FILE)).expect("canonical lock path");
        let open_files = String::from_utf8_lossy(&output.stdout);
        assert!(
            !open_files.lines().any(|line| Path::new(line) == lock_path),
            "the child holds {}: {open_files}",
            lock_path.display()
        );
        drop(store);
    }
}
