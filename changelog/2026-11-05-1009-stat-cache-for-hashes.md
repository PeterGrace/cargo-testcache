# 2026-11-05 10:09 EST: Remember executable digests by file metadata (issue #9)

## Summary

Every run hashed every test executable and every binary those tests spawn, and hashed them
again before recording a pass. On a workspace with about 115 debug test binaries of about
380 MB each, that read 40 to 45 GB per run, even when every binary was skipped. Now each digest
is remembered in `<target-dir>/testcache/hashes.json` with the file's size, modification and
change times (to the nanosecond), inode and device, the way ccache, sccache and git's index do.
The digest is reused while all of those match, so a warm run reads only metadata, and a run
after a one-crate change hashes only the relinked binaries. `--no-stat-cache` or
`CARGO_TESTCACHE_FULL_HASH=1` turns the reuse off.

## Changed

- `crates/cargo-testcache-core/src/hashes.rs` (new): `FileStat` (the stat fields, Unix only),
  `HashCache` (`load`, `digest`, `remember`, `save`) and `RACY_WINDOW` (2 s). `digest` takes
  `&self`, so rayon's threads share the cache; `remember` stores what they hashed afterwards.
  A digest is remembered only when the file's modification and change times are more than
  `RACY_WINDOW` before the moment its metadata was read (git's racy rule). `save` writes only
  when something changed, through a temporary file and a rename, and drops entries whose file
  is gone. An unreadable file, or one of another format version, is ignored.
- `app.rs`: `run` loads the cache under the store's lock and saves it after planning (so the
  hashing survives an interrupted run) and again after the run. `hash_files` and `record_pass`
  go through the cache, so the re-check before recording a pass compares stat fields first and
  hashes only a file whose metadata changed. A failed save is a warning, not an error.
  `Options` gains `full_hash`.
- `config.rs`: `CARGO_TESTCACHE_FULL_HASH` (`FULL_HASH_VAR`), read as `1`/`true` or `0`/`false`
  in any letter case, with `Config::full_hash` and `Config::with_full_hash`. `error.rs`:
  `Error::InvalidSwitch` for any other value.
- `main.rs`: `--no-stat-cache`, which turns full hashing on over the environment's setting;
  `dispatch` now takes the configuration.
- `README.md`, `.env.example` and the design spec (key part 1 and the limits table) describe the
  cache, the racy rule and the residual risk.
- `crates/cargo-testcache/Cargo.toml`: `serde_json` as a dev-dependency, for the fixture test
  that edits `hashes.json`.

## Tests

- New property tests in `hashes.rs`: the racy rule against drawn times; a settled file is
  remembered with its true digest and a planted digest is then returned without reading the
  file; changed metadata, or reuse turned off, hashes again; a just-written file is not
  remembered; save and load round-trip, minus deleted files, and a corrupt or other-version
  file loads empty. In `config.rs`: the switch's accepted values, the error for others, and
  `with_full_hash`. `main.rs`'s option-recognition test now covers `--no-stat-cache`.
- New fixture test `remembered_digests_replace_hashing_unless_turned_off`: after a run past the
  racy window, a digest planted in `hashes.json` for `gamma` makes only `gamma` rerun (`binary
  changed`), while `--no-stat-cache` or `CARGO_TESTCACHE_FULL_HASH=1` (drawn) ignores it.
  Turning reuse off in `HashCache::digest` makes this test fail, as it should.
- `just gate` passed (format, clippy, dylint, every test, the Hegel canary, docs).
- On this repository (5 executables, 453 MB): a warm run's process read 24 MB (`rchar`), against
  500 MB with `--no-stat-cache`; both skipped all 4 binaries.

## Decisions

- The variable is `CARGO_TESTCACHE_FULL_HASH`, not the issue's `TESTCACHE_FULL_HASH`, because
  the configuration only reads the `CARGO_TESTCACHE_` prefix.
- The racy window is a fixed 2 s rather than a measured per-filesystem granularity. Its cost is
  that a binary linked just before a run is hashed again on the next run, once.
- Files hashed under `--no-stat-cache` are still remembered: the opt-out refuses stored digests,
  and refreshing them with true ones does no harm.
- Off Unix, `FileStat::of` returns `None` and every file is hashed, as before.

## Not done

- No `fsync` of `hashes.json`: a power loss can at worst leave an unreadable file, which is
  ignored and costs only hashing.
