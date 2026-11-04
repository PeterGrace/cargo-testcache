# cargo-testcache: skip test binaries that already passed

Status: draft, 2026-11-04.

## 1. Problem

In a large Cargo workspace, `cargo test` runs every test binary every time, even when Cargo has
just told us that nothing about most of them changed. Cargo already avoids rebuilding a test
binary whose inputs are unchanged; it does not avoid *running* it.

The motivating case is a workspace of about 120 test binaries and 1000 tests, many backed by
Postgres in Docker and by spawned helper processes. One full run takes 8 to 19 minutes. A change
goes through the full suite four to eight times before it merges, even when most of those runs
follow a docs-only fix or a merge that touched an unrelated crate:

- after the change;
- after an audit's fixes;
- after merging the main branch;
- after review fixes;
- after merging main again.

Most of that time re-runs binaries that are byte-for-byte the ones that passed minutes earlier.

## 2. Goal

`cargo testcache` behaves like `cargo test`, except that it does not run a test binary when:

1. the binary file is identical to one that passed before, and
2. every runtime input that binary declares, or that the tool knows about, is identical too.

Only passes are cached. A failure, a timeout or a crash is never cached, so the next run always
retries it.

### Non-goals

- **Per-test selection inside a binary.** The unit of caching is the test binary, as in Go.
  Running only the tests in a binary that touch changed code is what coverage-based tools do,
  and they are approximate (section 3).
- **Sharing a cache between machines or checkouts.** Test binaries embed absolute paths
  (`CARGO_MANIFEST_DIR`, `file!()`), so the same source in two checkouts gives different
  binaries. The cache lives in the target directory.
- **Replacing CI's full run.** CI should keep running the plain `cargo test` (or this tool with
  `--no-cache`). The cache is for local iteration.
- **A test runner.** Running the binaries, their output and their exit status work as
  `cargo test` does. This is not nextest.

## 3. Prior art

| Tool | How it selects | Why it is not this |
|---|---|---|
| Go's `go test` | Caches a passing result keyed on the test binary and the files and environment variables it read; `-count=1` disables it ([golang/go#23799](https://golang.org/issue/23799)) | Built into Go; the model for this tool |
| Bazel, Buck2 | Cache passing test results keyed on all declared inputs ([Bazel test caching](https://bazel.virtuslab.com/book/1~2~1/)) | Means leaving Cargo as the build system |
| [cargo-affected](https://docs.rs/crate/cargo-affected/latest) | LLVM coverage: re-runs tests whose covered lines overlap the git diff | Very early, not on crates.io. It misses inputs that aren't Rust source (environment, filesystem, network, SQL), which are exactly what Docker- and process-backed tests depend on |
| [cargo-difftests](https://lib.rs/crates/cargo-difftests-core) | LLVM coverage profiles per test | Needs nightly Rust and coverage-instrumented runs, which are slower |
| cargo-nextest | No result cache; its filtersets (`rdeps(crate)`) can select the tests of the crates that depend on a changed crate | Selects by crate graph, not by what actually changed; needs the change set from outside |
| Hand-written CI scripts | Several projects describe a per-binary hash cache in CI issues (for example [zajca/pohunek#1219](https://github.com/zajca/pohunek/issues/1219), [alphaonedev/ai-memory-mcp#6384](https://github.com/alphaonedev/ai-memory-mcp/issues/6384)) | Not packaged; tied to their CI |

As of 2026-11-04 no published Cargo tool does Go-style binary-level caching. The name
`cargo-testcache` is free on crates.io.

## 4. How it works

### 4.1 Build and discover

```
cargo test --no-run --message-format=json-render-diagnostics <the user's cargo arguments>
```

This builds exactly what `cargo test` would build, with the same features, packages and
targets. It emits one `compiler-artifact` message per built target. A test binary is a message
whose `profile.test` is true and whose `executable` is set. That message also carries:

- the package id;
- the target (name, kind: `lib`, `bin`, `test`, `bench`, `example`);
- the source path.

The tool parses the messages with the `cargo_metadata` crate's `Message` type. Diagnostics are
rendered to the terminal as `cargo test` would. A build failure exits with Cargo's status, and
no test binary runs.

The same build also yields the package's `bin` executables. Those are what `CARGO_BIN_EXE_<name>`
points integration tests at (section 4.2).

### 4.2 The key

Each test binary's cache key is a SHA-256 over these parts, in a fixed order with length
prefixes:

1. **The binary:** the SHA-256 of the executable file. Cargo relinks only when an input
   changed, so an unchanged binary is the strongest single signal.
2. **Spawned binaries:** for an integration test or a benchmark, the hashes of every `bin`
   executable of its own package. Cargo exposes those to the test as `CARGO_BIN_EXE_<name>`
   (Cargo reference, "Environment variables Cargo sets for `cargo test`"). The test may spawn
   them, and a change to one changes nothing in the test binary itself.
3. **Declared inputs:** the contents of every file matched by the configured input globs
   (section 5), sorted by path, path included.
4. **Declared environment:** the values (or "unset") of the configured environment variable
   names. A fixed set is always included: `RUST_TEST_THREADS`, `RUST_MIN_STACK` and
   `RUST_TEST_TIME_*`.
5. **Arguments:** the arguments after `--`, such as a name filter, `--ignored` or
   `--include-ignored`. A pass with a filter says nothing about the tests the filter excluded.
6. **The tool:** cargo-testcache's version and the key-format version, so an upgrade never
   reuses a key computed differently.

Two inputs need no key part:

- **The toolchain and target triple:** a change to either rebuilds the binary, so it is already
  in part 1.
- **The source code:** compiled into the binary, so part 1 covers it. Files a test reads with
  `include_str!` or `include_bytes!` are compiled in too.

Files read at runtime are not compiled in. They must be declared, or they are a blind spot
(section 7).

### 4.3 Run

For each discovered test binary, in Cargo's order:

- **Key recorded as a pass:** report it as skipped, with the time of the recorded pass.
- **Otherwise:** run it the way `cargo test` does (section 4.4). If it exits 0, record the key.
  Otherwise record nothing.

**Failures:** fail-fast and `--no-fail-fast` behave as in `cargo test`. The exit status is
non-zero if any binary that ran failed.

**Doc tests** cannot be cached this way, because rustdoc compiles them at test time. Version 1
runs them through `cargo test --doc` with the same package and feature arguments, unless
`--no-doc` is given or the user's own arguments exclude them. Caching doc tests by a hash of the
package's sources is future work.

**Parallelism:** binaries run one at a time, as `cargo test` runs them; each binary runs its own
tests in parallel. A `--jobs N` option to run several binaries at once is future work. Tests
that share external resources (a database, Docker networks) often assume one binary at a time.

### 4.4 Running a binary as cargo test would

`cargo test` runs a test binary with:

- the package's `CARGO_*` variables in its environment (`CARGO_MANIFEST_DIR`, `CARGO_PKG_*`,
  `CARGO_CRATE_NAME` and the rest; the Cargo reference says these apply "for running binaries
  with `cargo run` and `cargo test` as well");
- `CARGO_BIN_EXE_<name>` for integration tests and benchmarks;
- the dynamic library path extended with `target/<profile>`, `target/<profile>/deps`, any
  build-script `rustc-link-search` paths inside the target directory, and the sysroot library
  path (`LD_LIBRARY_PATH` on Unix, `DYLD_FALLBACK_LIBRARY_PATH` on macOS, `PATH` on Windows);
- the package root as its working directory. Cargo's reference doesn't document this, so it has
  to be confirmed by experiment.

The tool rebuilds that environment from `cargo metadata` and the artifact messages, and executes
the binary directly.

**The risk:** any difference from Cargo's environment shows up as a test that passes under
`cargo test` but fails here, or the reverse. Two things contain it:

- **A conformance test:** a fixture workspace whose tests print their environment and working
  directory. The test diffs what `cargo test` gives them against what `cargo testcache` gives
  them.
- **A fallback runner, `--via-cargo`:** it runs each uncached target through Cargo itself with
  `cargo test -p <pkg> --test <name>` (or `--lib`, `--bin <name>`) and the user's arguments.
  Cargo finds everything fresh, so each invocation adds only Cargo's freshness check, about a
  second.

  When no binary is cached at all, this mode runs one plain `cargo test` instead.

### 4.5 The cache

- **Location:** one JSON-lines file at `<target-dir>/testcache/passes.jsonl`. Each line holds a
  key, the package and target, the time of the pass, the duration, and the passed and ignored
  counts parsed from libtest's summary when present.
- **Writes:** append-only, and only after the binary exits 0.
- **Concurrency:** an advisory lock (`fs4` or `fd-lock`) serializes concurrent runs in one target
  directory. When the lock is taken, it is opened with close-on-exec so a spawned daemon such as
  sccache can't inherit it. A daemon that inherits the lock holds it forever and blocks every
  later run.
- **Pruning:** at startup the file is compacted to the newest line per (package, target), so it
  stays one line per binary.
- **Clearing:** `cargo clean` removes the cache along with everything else, because it lives in
  the target directory. `cargo testcache clear` removes it alone.

## 5. Configuration

Configuration lives in `[workspace.metadata.testcache]` in the root `Cargo.toml`, with
per-package overrides in `[package.metadata.testcache]`:

```toml
[workspace.metadata.testcache]
# Files tests read at runtime. Globs are relative to the workspace root and apply to every test
# binary unless a package narrows them.
inputs = ["migrations/**", "tests/fixtures/**", "sidecar/dist/**"]
# Environment variables whose values become part of every key.
env = ["DATABASE_IMAGE", "MYAPP_FAULT"]
# Test binaries that are always run: (package, target) patterns.
always_run = ["*/live_*"]
# Other binaries a test may spawn by path, outside CARGO_BIN_EXE_* (paths relative to the
# target profile directory).
spawns = ["myapp-server"]
```

Package overrides use the same keys and add to the workspace's values.

## 6. Command line

```
cargo testcache [OPTIONS] [CARGO TEST ARGS] [-- TEST BINARY ARGS]
cargo testcache status [CARGO TEST ARGS]
cargo testcache clear
```

- `--no-cache`: run everything and record the passes (a full refresh).
- `--explain`: print, for every binary, why it runs (never passed; binary changed; input
  `migrations/0007.sql` changed; env `X` changed; arguments changed) or why it is skipped.
- `--via-cargo`: section 4.4's fallback runner.
- `--no-doc`: skip doc tests.
- `status`: builds as `run` does, and lists what would run and what would be skipped, without
  running anything.

The output ends with a line such as:

```
testcache: 117 binaries, 9 run (7 changed, 2 never passed), 108 skipped; 1 failed
```

It goes through `indicatif` progress output while binaries run, and plain lines when stdout is
not a terminal.

## 7. Correctness and its limits

The tool skips a binary only when its key matches a recorded pass, and a wrong skip needs a
changed input that the key does not cover. The known uncovered inputs are:

| Input | Covered by | When it goes wrong |
|---|---|---|
| Files read at runtime | `inputs` globs | An undeclared fixture changes. `--explain` and a "runtime reads" lint (future work: compare `strace`/`fs_usage` traces against the globs) help find them |
| External services (database image versions, a mock server image) | `env` names, if the version is in a variable | The service changes behind an unchanged variable. Use `always_run` or `--no-cache` |
| Binaries spawned by path | `spawns` | An unlisted helper binary changes |
| Randomized tests (hegel, proptest, quickcheck) | Nothing: a pass is cached like any other | The cache makes them explore less. Every rebuild of the binary explores again; `--no-cache` explores again on demand |
| Time, network, machine state | Nothing | Same as any test that depends on them |

What the tool does when it is unsure:

- **Any key part it can't compute** (an unreadable input file, a missing executable): the binary
  runs. A missed skip costs time; a wrong skip hides a failure.
- **A failure:** never cached. A flaky test that fails gets retried on the next run.

## 8. Architecture

The template's two-crate layout:

- **`cargo-testcache-core` (library):**
  - `discover`: run the `--no-run` build, parse the artifact messages, and produce the test
    binaries with their package, target and executable.
  - `key`: compute a binary's key from its parts. Pure; every part is an input value.
  - `config`: read the workspace and package metadata.
  - `store`: the JSON-lines cache, its lock, lookup, record and compaction.
  - `exec`: build a binary's environment and working directory, run it, stream its output,
    return its status and libtest summary; plus the `--via-cargo` runner.
  - `plan`: decide run or skip, with the reason, for each binary.
- **`cargo-testcache` (binary):**
  - the `clap` command line, written to work as a cargo subcommand: `cargo testcache` passes
    `testcache` as the first argument;
  - output and the exit status.

**Dependencies:**
- `cargo_metadata` for metadata and messages;
- `sha2` for hashing;
- `globset` and `walkdir` for inputs;
- `serde` and `serde_json` for the store;
- `fs4` for the lock;
- `clap`, `indicatif`, `thiserror` and `anyhow`.

## 9. Testing

- **Unit and hegel tests (`key`):**
  - the key is a function of its parts;
  - it changes when any one part changes;
  - reordering inputs found by a glob does not change it, because they are sorted;
  - the length prefixes keep `("ab","c")` and `("a","bc")` apart.
- **Unit and hegel tests (`plan`):** a binary is skipped iff its key is recorded, and only a
  zero exit status records it.
- **`store` tests:**
  - compaction keeps the newest line per target;
  - concurrent appenders under the lock never interleave lines;
  - a lock held by a spawned child closes on exec.
- **Fixture-workspace integration tests** (`tests/fixtures/ws`), each run through the built
  binary:
  1. the first run runs every binary, and the second skips every one;
  2. touching one crate's source re-runs only the binaries that depend on it;
  3. changing a declared input re-runs every binary its glob covers;
  4. a failing test is not recorded and runs again next time;
  5. changing the arguments after `--` re-runs;
  6. an integration test that spawns its package's binary re-runs when only the binary changes;
  7. `--no-cache` runs everything;
  8. `status` changes nothing;
  9. the conformance test of section 4.4: environment and working directory are identical to
     `cargo test`'s, on Linux and macOS in CI.

## 10. Open questions

1. **Direct execution or `--via-cargo` as the default.** Direct execution is faster. Through
   Cargo is correct by construction but adds about a second per run binary. Recommendation:
   direct by default once the conformance test passes on every CI platform. Until then, ship
   `--via-cargo` as the default.
2. **nextest as a runner.** With `--runner nextest`, run the uncached binaries through
   `cargo nextest run` with a filterset that names them, for nextest's per-test processes and
   output. Worth doing after version 1 if the workspace moves to nextest.
3. **Doc-test caching.** Key doc tests on the hash of the package's `src/**` plus its
   dependencies' binaries. Deferred.
4. **A determinism check.** Rebuild once, and refuse to cache if an unchanged build produces a
   different binary hash (some build scripts embed timestamps). That would surface as "every
   binary always runs", which is safe but useless, so a one-time warning is enough.
