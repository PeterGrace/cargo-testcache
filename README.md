# cargo-testcache

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

- `crates/cargo-testcache`: the binary: arguments, logging, signal handling.
- `crates/cargo-testcache-core`: the application logic, as a library.
- `changelog/`: one entry per change, `YYYY-MM-DD-HHMM-<slug>.md`.
- `docs/`: design specs, plans and reference material.
