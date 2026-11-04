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

Declare what tests read at runtime in the root `Cargo.toml`; a package's own table adds to it,
with `inputs` relative to the package. Globs must stay inside the workspace root: `../shared/**`
in a package is fine, an absolute glob is refused, and a glob that matches no file is warned
about.

```toml
[workspace.metadata.testcache]
inputs = ["migrations/**", "tests/fixtures/**"]  # files read at runtime
env = ["DATABASE_IMAGE"]                         # variables whose values matter
always_run = ["*/live_*"]                        # package/target patterns never skipped
spawns = ["myapp-server"]                        # binaries spawned by path, under target/<profile>
```

Any change to a `.cargo/config.toml` (in the working directory, its ancestors or `$CARGO_HOME`)
or to a `--config` argument reruns every binary. When the configuration has an `[env]` table,
binaries run through `cargo test`, since only Cargo applies it.

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
