# Agent Guidelines for Rust Code Quality

This document provides guidelines for maintaining high-quality Rust code. These rules MUST be followed by all AI coding agents and contributors.

## Your Core Principles

All code you write MUST be fully optimized.

"Fully optimized" includes:

- maximizing algorithmic big-O efficiency for memory and runtime
- using parallelization and SIMD where appropriate
- following proper style conventions for Rust (e.g. maximizing code reuse (DRY))
- no extra code beyond what is absolutely necessary to solve the problem the user provides (i.e. no technical debt)

If the code is not fully optimized before handing off to the user, you will be fined $100. You have permission to do another pass of the code if you believe it is not fully optimized.

YOU ARE A RABID DOCUMENTARIAN.  You will create markdown docs for every new iteration of code.  You will create these docs under changelog/ and will date-and-timestamp the markdown files.

Changelog entries **MUST** use this format, and nothing else:

- File name: `changelog/YYYY-MM-DD-HHMM-<slug>.md`, local time from the clock (`date +%Y-%m-%d-%H%M`), with a short kebab-case slug.
- First line: `# YYYY-MM-DD HH:MM TZ: <Title> (issue #N)`, leaving out the issue when there is none.
- Then `## Summary` (what and why, in a few sentences), `## Changed` (one bullet per change, naming files or items), and `## Tests` (what was run, and what it showed). Add `## Decisions` or `## Not done` when there is something to say.

## Project Layout

- A Cargo workspace: `crates/cargo-testcache` is a thin binary (arguments, logging, signals); the application's logic lives in `crates/cargo-testcache-core` and other library crates under `crates/`, so tests and other crates can call it without a process.
- Shared dependency versions, lints and metadata live in the root `Cargo.toml` (`[workspace.dependencies]`, `[workspace.lints]`); member crates opt in with `<key>.workspace = true` and `[lints] workspace = true`.
- Configuration comes from defaults, then `CARGO_TESTCACHE_*` environment variables (and `.env`), then command-line flags. Document every variable in `.env.example`.
- Logs go to stderr, so stdout stays clean for data. Set the filter with `RUST_LOG`.
- `just` runs every routine task (`just` lists them); CI calls the same recipes.
- Design specs go in `docs/superpowers/specs/YYYY-MM-DD-<slug>-design.md` and implementation plans in `docs/superpowers/plans/YYYY-MM-DD-<slug>.md`.

## Preferred Tools

- Use `cargo` for project management, building, and dependency management.
- Use `indicatif` to track long-running operations with progress bars. The message should be contextually sensitive.
- Use `serde` with `serde_json` for JSON serialization/deserialization.
- Use `ratatui` and `crossterm` for terminal applications/TUIs.
- Use `axum` for creating any web servers or HTTP APIs, with async handlers returning `Result<Response, AppError>`, layered extractors, and `tower` middleware. Full conventions auto-load for `**/src/routes/**`, `**/src/handlers/**`, `**/src/api/**`, and `**/src/main.rs` from `.claude/rules/web-server.md`.
- When reporting errors to the console, use `tracing::error!` or `log::error!` instead of `println!`.
- Web-based front ends (WASM/`dioxus`) **MUST** do all deep computation in Rust and use Pico CSS + vanilla JS only (never React/jQuery), with adaptive light/dark theming. Full rules auto-load for `**/*.html`, `**/*.css`, `web/**`, and `**/src/app.rs`/`**/src/components/**` from `.claude/rules/wasm-frontend.md`.
- Tabular data processing **MUST** use `polars` (never other dataframe libraries), never print row-count and schema together, and never ingest more than 10 rows at a time. Full rules auto-load for `**/src/data/**`, `**/src/etl/**`, `**/*.csv`, `**/*.parquet` from `.claude/rules/data-processing.md`.
- PyO3/`maturin` Python bindings **MUST** rebuild with `maturin` after Rust changes, use `uv` (never system Python) for `.venv`, and follow strict Python typing/mypy rules. Full rules auto-load for `**/*.py`, `pyproject.toml`, `**/src/python/**` from `.claude/rules/pyo3-python.md`.

## Code Style and Formatting

- **MUST** use meaningful, descriptive variable and function names
- **MUST** follow Rust API Guidelines and idiomatic Rust conventions
- **MUST** use 4 spaces for indentation (never tabs)
- **NEVER** use emoji, or unicode that emulates emoji (e.g. ✓, ✗). The only exception is when writing tests and testing the impact of multibyte characters.
- Use snake_case for functions/variables/modules, PascalCase for types/traits, SCREAMING_SNAKE_CASE for constants
- Limit line length to 100 characters (rustfmt default)
- Assume the user is a Python expert, but a Rust novice. Include additional code comments around Rust-specific nuances that a Python developer may not recognize.

## Documentation

- **MUST** include doc comments for all public functions, structs, enums, and methods
- **MUST** document function parameters, return values, and errors
- Keep comments up-to-date with code changes
- Include examples in doc comments for complex functions

A full worked example (Arguments/Returns/Errors/Examples sections on a `calculate_total` function) is in [docs/reference/doc-comment-example.md](../docs/reference/doc-comment-example.md).

## Type System

- **MUST** leverage Rust's type system to prevent bugs at compile time
- **NEVER** use `.unwrap()` in library code; use `.expect()` only for invariant violations with a descriptive message
- **MUST** use meaningful custom error types with `thiserror`
- Use newtypes to distinguish semantically different values of the same underlying type
- Prefer `Option<T>` over sentinel values

## Error Handling

- **NEVER** use `.unwrap()` in production code paths
- **MUST** use `Result<T, E>` for fallible operations
- **MUST** use `thiserror` for defining error types and `anyhow` for application-level errors
- **MUST** propagate errors with `?` operator where appropriate
- Provide meaningful error messages with context using `.context()` from `anyhow`

## Function Design

- **MUST** keep functions focused on a single responsibility
- **MUST** prefer borrowing (`&T`, `&mut T`) over ownership when possible
- Limit function parameters to 5 or fewer; use a config struct for more
- Return early to reduce nesting
- Use iterators and combinators over explicit loops where clearer

## Struct and Enum Design

- **MUST** keep types focused on a single responsibility
- **MUST** derive common traits: `Debug`, `Clone`, `PartialEq` where appropriate
- Use `#[derive(Default)]` when a sensible default exists
- Prefer composition over inheritance-like patterns
- Use builder pattern for complex struct construction
- Make fields private by default; provide accessor methods when needed

## Testing

- **MUST** write unit tests for all new functions and types
- **MUST** mock external dependencies (APIs, databases, file systems)
- **MUST** write tests as Hegel property tests (`#[hegel::test]`, from the `hegeltest` crate): state a property that holds for every input and draw the inputs, rather than checking a few hand-picked examples. `just dylint` enforces this with the `hyp_hegel` lint.
- Async property tests put `#[tokio::test]` before `#[hegel::test]`; tests that start processes or touch process-wide state set a small `test_cases`.
- Follow the Arrange-Act-Assert pattern
- Do not commit commented-out tests
- Put tests that run the binary in `crates/cargo-testcache/tests/`, and tests of the library's public API in the library crate's `tests/`
- Use `#[cfg(test)]` modules for test code

## Imports and Dependencies

- **MUST** avoid wildcard imports (`use module::*`) except for preludes, test modules (`use super::*`), and prelude re-exports
- **MUST** document dependencies in `Cargo.toml` with version constraints
- Use `cargo` for dependency management
- Organize imports: standard library, external crates, local modules
- Use `rustfmt` to automate import formatting

## Rust Best Practices

- **NEVER** use `unsafe` unless absolutely necessary; document safety invariants when used
- **MUST** call `.clone()` explicitly on non-`Copy` types; avoid hidden clones in closures and iterators
- **MUST** use pattern matching exhaustively; avoid catch-all `_` patterns when possible
- **MUST** use `format!` macro for string formatting
- Use iterators and iterator adapters over manual loops
- Use `enumerate()` instead of manual counter variables
- Prefer `if let` and `while let` for single-pattern matching

## Memory and Performance

- **MUST** avoid unnecessary allocations; prefer `&str` over `String` when possible
- **MUST** use `Cow<'_, str>` when ownership is conditionally needed
- Use `Vec::with_capacity()` when the size is known
- Prefer stack allocation over heap when appropriate
- Use `Arc` and `Rc` judiciously; prefer borrowing

## Concurrency

- **MUST** use `Send` and `Sync` bounds appropriately
- **MUST** prefer `tokio` for async runtime in async applications
- **MUST** use `rayon` for CPU-bound parallelism
- Avoid `Mutex` when `RwLock` or lock-free alternatives are appropriate
- Use channels (`mpsc`, `crossbeam`) for message passing

## Security

- **NEVER** store secrets, API keys, or passwords in code. Only store them in `.env`.
  - Ensure `.env` is declared in `.gitignore`.
- **MUST** use environment variables for sensitive configuration via `dotenvy` or `std::env`
- **NEVER** log sensitive information (passwords, tokens, PII)
- Use `secrecy` crate for sensitive data types

## Version Control

- **MUST** write clear, descriptive commit messages
- **NEVER** commit commented-out code; delete it
- **NEVER** commit debug `println!` statements or `dbg!` macros
- **NEVER** commit credentials or sensitive data

## Tools

- **MUST** use `rustfmt` for code formatting
- **MUST** use `clippy` for linting and follow its suggestions
- **MUST** ensure code compiles with no warnings (use `-D warnings` flag in CI, not `#![deny(warnings)]` in source)
- Silence a lint for one item with `#[expect(lint, reason = "...")]`, never `#[allow]`: an `expect` that no longer fires is itself a warning, so stale exemptions get removed
- Use `cargo` for building, testing, and dependency management
- Use `cargo test` for running tests
- Use `cargo doc` for generating documentation
- **NEVER** build with `cargo build --features python`: this will always fail. Instead, **ALWAYS** use `maturin`.

## Before Committing

- [ ] `just gate` passes: formatting, clippy (`--workspace --all-targets -D warnings`), dylint, every test, the Hegel canary, and docs with warnings as errors
- [ ] If dependencies changed, `just deny` passes
- [ ] If the project creates a Python package and Rust code is touched, rebuild the Python package (`source .venv/bin/activate && maturin develop --release --features python`)
- [ ] If the project creates a WASM package and Rust code is touched, rebuild the WASM package (`wasm-pack build --target web --out-dir web/pkg`)
- [ ] All public items have doc comments
- [ ] No commented-out code or debug statements
- [ ] No hardcoded credentials

---

**Remember:** Prioritize clarity and maintainability over cleverness.
