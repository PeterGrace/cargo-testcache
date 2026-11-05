# cargo-testcache

`cargo testcache` runs `cargo test`, but skips every test binary that already passed with the
same executable, the same binaries it spawns, the same declared input files and environment,
the same test arguments and the same Cargo configuration, the way Go's test cache does.
Failures are never cached. The design is in
`docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`.

## Usage

```
cargo install --path crates/cargo-testcache
cargo testcache [OPTIONS] [CARGO TEST ARGS] [-- TEST BINARY ARGS]
cargo testcache status [CARGO TEST ARGS]     # what would run, and why; runs nothing
cargo testcache clear                        # forget every recorded pass
```

A test filter spelled exactly `status` or `clear` must be written after `--`
(`cargo testcache -- status`), since a first argument with that name is the subcommand.

| Option | Effect |
|---|---|
| `--no-cache` | Run everything and record the passes |
| `--explain` | Print why each binary runs: binary, input `x` or env `Y` changed, ... |
| `--via-cargo` | Run each binary through `cargo test -p <pkg> --test <name>` instead of directly |
| `--no-doc` | Skip doc tests (they are never cached) |

On a first, cold run, pass `--no-fail-fast`. Like `cargo test`, a run stops at the first
binary that fails, so the binaries after it are neither run nor recorded; with
`--no-fail-fast` every passing binary is recorded, so the next run does not repeat them.

After a variable is added to `env`, the next run explains `env X newly declared, unset` (or
`env X newly keyed, set`): the binaries run because the key now includes the variable, not
because the environment changed.

`--via-cargo` often cannot record a pass. When only some binaries must run, it builds each
target alone (`-p <pkg> --test <name>`), and Cargo then unifies features differently, so it
often builds a different executable from the one that was hashed (2 of this repository's 4 test
binaries). Such a binary runs but is never recorded, so it runs again every time. Prefer the
direct runner.

Declare what tests read at runtime in the root `Cargo.toml`; a package's own table adds to it,
with `inputs` relative to the package. Globs must stay inside the workspace root: `../shared/**`
in a package is fine, an absolute glob is refused, and a glob that matches no file is warned
about. Put a file that only some checkouts have (a developer's `.env` read by an opt-in test)
in `optional_inputs` instead: it is keyed on the same way, so its appearing or disappearing
reruns the binaries that declare it, but matching nothing is expected and not warned about. A
glob listed in both keys counts as required, so it is still warned about.

Every matched file is hashed on every run, so keep globs narrow. Name source directories one
level deep (`sidecar/src/**`, not `sidecar/**`) and leave dependency directories such as
`node_modules` out, declaring their lockfile instead: it changes whenever they do. A glob that
matches more than 1,000 files is warned about, with its count.

```toml
[workspace.metadata.testcache]
inputs = ["migrations/**", "tests/fixtures/**"]  # files read at runtime
optional_inputs = [".env"]                       # the same, but they may be absent
env = ["DATABASE_IMAGE"]                         # variables whose values matter
always_run = ["*/live_*"]                        # package/target patterns never skipped
spawns = ["myapp-server"]                        # binaries spawned by path, under target/<profile>
```

A package can narrow `inputs`, `optional_inputs` and `spawns` to some of its test targets, so a
fixture one test reads, or a helper only some tests spawn, reruns only those binaries. A binary's
key covers the workspace's and its package's declarations plus its own target's:

```toml
[package.metadata.testcache]
inputs = ["shared/**"]                         # every target of the package

[package.metadata.testcache.targets.migrate]   # integration test `tests/migrate.rs`
inputs = ["migrations/**"]

[package.metadata.testcache.targets.lib]       # the lib's unit tests
inputs = ["../../scripts/fixture.json"]
spawns = ["helper-server"]
```

- `targets.<name>` applies to every test binary of the package whose target is named `<name>`,
  whatever its kind, so a bin and an integration test with the same name both match. `lib`
  also matches the library's unit tests, whatever the crate is called.
- A target table holds only `inputs` and `optional_inputs` (relative to the package, under the
  same rules as the package's) and `spawns` (under `target/<profile>`). `env` and `always_run`
  stay package-level; any other key is an error.
- A `targets.<name>` that names none of the package's lib, bin, test, bench or example
  targets, and is not `lib` for a package with a library, is an error, as is `targets` in
  `[workspace.metadata.testcache]`. Only workspace members are checked.
- `--explain` says which levels declare a changed input, and which target a target table's
  spawned binary belongs to:
  ``input migrations/1.sql changed (target `migrate`)``,
  ``spawned binary helper-server changed (target `lib`)``.

Any change to a `.cargo/config.toml` (in the working directory, its ancestors or `$CARGO_HOME`),
to a file it `include`s, or to a `--config` argument reruns every binary. When the
configuration has an `[env]` table or a target runner, binaries run through `cargo test`, since
only Cargo applies them. That is the via-cargo runner, with the limit above: you get it without
passing `--via-cargo`, and binaries that Cargo builds differently on their own are never
recorded. Applying `[env]` in the direct runner, so that only runners need Cargo, is future
work.

The record lives in `<target-dir>/testcache/passes.jsonl`; `cargo clean` removes it too. Keep a
plain `cargo test` in CI: the cache is for local iteration.

## Development

Install [`just`](https://github.com/casey/just), then run `just` to list the tasks. The common
ones:

| Command | What it does |
|---|---|
| `just run --help` | Build and run the binary; arguments pass through |
| `just gate` | Everything to check before committing: format, lints, tests, docs |
| `just fmt` | Format the workspace |
| `just deny` | Audit dependencies (needs `cargo-deny`) |
| `just coverage` | Test coverage summary (needs `cargo-llvm-cov`) |
| `just release-prepare patch` | Bump, commit and tag a release (needs `cargo-release`) |
| `just release-ship` | Publish the prepared release |

Configuration is read from environment variables (or a `.env` file); `.env.example` lists them
with their defaults.

## Layout

- `crates/cargo-testcache`: the binary: command line, logging, exit status.
- `crates/cargo-testcache-core`: the application logic, as a library.
- `changelog/`: one entry per change, `YYYY-MM-DD-HHMM-<slug>.md`.
- `docs/`: design specs, plans and reference material.
