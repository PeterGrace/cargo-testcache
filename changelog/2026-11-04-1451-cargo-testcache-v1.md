# 2026-11-04 14:51 EST: cargo-testcache v1

## Summary

Implemented the design in `docs/superpowers/specs/2026-11-04-cargo-testcache-design.md` following
`docs/superpowers/plans/2026-11-04-cargo-testcache.md`: `cargo testcache` builds with
`cargo test --no-run`, keys every test binary on its executable, the binaries it spawns, its
declared inputs and environment and the test arguments, and skips binaries whose key matches a
recorded pass. Every one of the plan's 15 tasks was implemented by a subagent and reviewed for
spec compliance and code quality, with fixes and re-reviews until both reviews passed.

## Changed

- `crates/cargo-testcache-core`: new modules `key`, `args`, `manifest`, `workspace`, `discover`,
  `inputs`, `store`, `plan`, `exec`, `report`, `app`; `config` and `error` reworked; the
  template's async `app` removed.
- `crates/cargo-testcache/src/main.rs`: the command line (`status`, `clear`, `--no-cache`,
  `--explain`, `--via-cargo`, `--no-doc`); `shutdown.rs` and `tests/startup.rs` removed.
- `crates/cargo-testcache/tests/fixture.rs` and `tests/fixtures/ws`: end-to-end and conformance
  tests.
- `Cargo.toml`: dependencies for hashing, globbing, walking, progress and parallelism; tokio,
  console-subscriber and nix removed; MSRV 1.89; the layout comment no longer mentions signals.
- `justfile`, `.github/workflows/ci.yml`: tokio-console recipes removed; `fixture-macos` job added
  (the fixture tests on `macos-latest`).
- `deny.toml`: the stale `tokio-console` mention removed.
- `README.md`: usage, options, the `[workspace.metadata.testcache]` table, where the record
  lives, and that a filter named `status` or `clear` goes after `--`; the layout line no longer
  mentions signal handling.
- `.env.example`: checked; it lists only `RUST_LOG` and `CARGO_TESTCACHE_LOG_FORMAT`, with no
  shutdown timeout.
- `.claude/CLAUDE.md`: the binary is described as "command line, logging, exit status".
- The design: status line set to implemented; section 6 corrected (the progress bar is on stderr,
  is plain lines when stderr is not a terminal, and is cleared while a binary runs; Cargo-style
  labels; a positional filter equals `-- filter`); section 7 gained the "sources edited between
  the build and a via-cargo run" row.

## Tests

- `just fmt && just gate`: exit 0 in 19.9 s wall time (incremental). Formatting (workspace and
  fixture workspace), clippy with `-D warnings`, dylint, and `cargo test --workspace`: 72 tests
  passed, 0 failed (60 unit tests in `cargo-testcache-core`, 2 in the binary, 8 fixture tests,
  1 Hegel canary, 1 doc test). The Hegel canary generated 100 cases. `cargo doc` with
  `-D warnings` passed.
- Fixture tests: 8 passed in 6.41 s (test binary already built).
- `just deny`: exit 0, "advisories ok, bans ok, licenses ok, sources ok"; one
  `warning[duplicate]` for two versions of `syn`.
- `just msrv`: exit 0; `cargo +1.89 check --workspace --all-targets --locked` finished.
- The tool on this repository: the first `cargo run -q -p cargo-testcache -- testcache` ended
  with `testcache: 4 binaries, 4 run (4 never passed), 0 skipped`, the second with
  `testcache: 4 binaries, 0 run, 4 skipped`. `testcache clear` then removed the record.

## Decisions

From the plan:

1. No `cargo_metadata` crate: own serde structs for `cargo metadata` and build messages, because
   that crate turns `rust_version = "1.89"` into `1.89.0`, which tests would see.
2. Direct execution is the default; `--via-cargo` is the fallback. The conformance test runs in
   CI on Linux and macOS.
3. Test output is inherited, not piped, so records hold no passed/ignored counts.
4. No tokio and no signal handlers: Ctrl-C reaches the test binary through the process group,
   and only exit status 0 is recorded.
5. `.env` is read, never loaded: only its `CARGO_TESTCACHE_*` entries are used.
6. Package `inputs` globs are relative to the package root, workspace globs to the workspace
   root; both add up.
7. Unreadable directories are skipped while matching inputs; an unreadable matched file makes
   the key unavailable, so the binary runs.
8. Environment values are hashed in the record, never stored.
9. `config` holds the environment settings; the `[*.metadata.testcache]` tables live in
   `manifest`.
10. `--no-run` and `--doc` pass straight through to `cargo test`; `--message-format` is refused.
11. Binaries run in (package name, target kind, target name) order.
12. Plain records have public fields; types with behaviour keep private fields and accessors.

Taken during implementation:

- workspace: only workspace members' `[package.metadata.testcache]` tables are read;
  dependencies get the default config (full metadata is still loaded so `-p <dependency>`
  works).
- inputs:
  - the root, target and build dirs are canonicalized, and the target/build dirs are skipped by
    identity (dev, ino) on Unix;
  - symlinked files and directories are followed;
  - `\` in package dirs is escaped;
  - the walk is pruned to the globs' literal directory prefixes;
  - a matched path that is not UTF-8 makes the key unavailable;
  - every `.git` entry is skipped.
- exec (conformance with `cargo test`, found by comparing env dumps of a probe workspace with
  build scripts):
  - `OUT_DIR` and the package's own build-script `rustc-env` are set;
  - build-script link-search paths are kept only under the binary's artifact dir, sorted and
    de-duplicated, first;
  - a build script's own `rustc-env` for the library-path variable replaces the computed path;
  - the target libdir is chosen per binary from its executable's location (so several
    `--target`s work);
  - `rustc --print` runs in the process cwd (rustup resolves the toolchain there);
  - `CARGO_TARGET_<TRIPLE>_RUNNER` forces the via-cargo runner.
- args:
  - flags are matched on bytes;
  - a positional `cargo test` TESTNAME filter is moved to the test args
    (`cargo testcache foo` == `-- foo`), so it is part of the key. This fixed a wrong skip: a
    filtered via-cargo pass used to be recorded as a full pass;
  - the value-taking options are taken from cargo 1.99's `cargo test --help`.
- discover: a message that is valid JSON but unreadable logs a warning (a test binary must not
  vanish silently); lines are read as bytes.
- app:
  - `status` refuses `--doc`/`--no-run` and never runs cargo;
  - a fail-fast stop reports ", N not run";
  - `status` says "would run";
  - `--explain` also explains the run-once-through-cargo shortcut;
  - `--via-cargo` never needs rustc.
- cli:
  - clap's `help` subcommand is disabled (use `--help`);
  - a misplaced subcommand name or `help` taken as a test filter logs a warning;
  - an invalid `--log-format` exits 2 (clap's usage error).
- report: examples get Cargo's `unittests` label; the progress bar has a steady tick and names
  the running binary.
- store:
  - the lock is std's `File::lock` (stable since Rust 1.89, hence the MSRV), not `fs4`: one
    dependency fewer, and std opens files close-on-exec, as the design requires;
  - lock-contention properties; a spawn mutex in tests, because `flock` is briefly shared
    across fork/exec.
- workspace: a package's `always_run` patterns are not scoped to that package; they join one
  workspace-wide set matched against `package/target`, unlike `inputs`.
- fixture tests:
  - every variant runs on every case (CI derandomizes Hegel, and random draws missed variants);
  - the conformance test runs with and without a separate build dir and with an `alpha` build
    script, and found no difference from `cargo test`;
  - mutation testing showed the fixture tests catch a wrong skip, keys ignoring inputs, args or
    spawned bins, a missing `OUT_DIR`, recorded failures, and the wrong working directory;
  - the fixture is formatted by `just fmt`/`fmt-check`.

Fixed after the final branch review (changelog/2026-11-04-1526-review-fixes.md):

- Cargo's configuration is a key part, and an `[env]` table makes the run use Cargo;
- a via-cargo pass is recorded only when Cargo builds the hashed executable for that target;
- input globs resolve `.` and `..`, and globs that can never match are refused or warned
  about;
- a pass is recorded only if the executable and spawned binaries are unchanged after the run.

## Not done

- Per-test selection, doc-test caching, `--jobs`, a nextest runner, the determinism check and a
  runtime-reads lint: design section 10 and the "future work" notes.
- libtest's passed/ignored counts in records (Decision 3).
- Windows is untested: no CI job runs there.
- The macOS fixture job has not run yet; it runs on the next push.
- Store size: every record repeats the workspace-wide `inputs` digests, so 120 binaries with
  1000 input files make `passes.jsonl` about 13 MB, read and parsed on every open (about
  0.1-0.3 s, estimated). Possible fix: store each distinct inputs map once, keyed by digest, or
  keep `parts` in a side file read only by `--explain`.
- The store's lock is fatal on filesystems without flock support (some network mounts); falling
  back to running uncached is an open design question.
- Runners configured in `.cargo/config.toml` (`target.<triple>.runner`) are not detected; only
  `CARGO_TARGET_<TRIPLE>_RUNNER` switches to the via-cargo runner. A `--target` given as a
  `.json` spec, `CARGO_BUILD_TARGET` and `build.target` count as the host for that check.
- `build_scripts` is keyed by package id, last message wins: if one build runs a package's build
  script twice (host and target, or different features), a test binary could get the other
  run's `OUT_DIR`. Untested.
- The nested-cargo case (an inherited library path that already starts with the computed one) is
  not special-cased as Cargo may do; unverified.
