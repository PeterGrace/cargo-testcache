# 2026-11-04 12:52 EST: Implementation plan for cargo-testcache v1

## Summary

Wrote the implementation plan for the design in
`docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`: 15 test-first tasks, each with its
code and property tests, from stripping the template's async scaffolding to the fixture-workspace
and conformance tests and a macOS CI job. Facts the plan relies on were checked against cargo
1.99.1 first.

## Changed

- `docs/superpowers/plans/2026-11-04-cargo-testcache.md`: the plan.

## Tests

No code changed. A scratch workspace confirmed what `cargo test` gives a test binary: the
package root as working directory, `CARGO_BIN_EXE_*` only for integration tests, and the library
path order. It also confirmed that `cargo metadata` reports `build_directory`, and that
`cargo_metadata` rewrites `rust-version = "1.89"` as `1.89.0`.

## Decisions

The plan lists 12, among them: its own serde structs instead of `cargo_metadata`, direct execution
as the default runner (gated by the conformance test on Linux and macOS), inherited test output
(no libtest counts in records), no tokio, and `.env` read but never loaded into the environment.
