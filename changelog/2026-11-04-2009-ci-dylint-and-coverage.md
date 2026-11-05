# 2026-11-04 20:09 EST: CI fixes for dylint and the coverage run

## Summary

The first CI run on GitHub (run 41945779556) failed two jobs, neither because of a bug in
cargo-testcache. The macOS fixture tests, fmt/clippy/doc, deny and msrv passed.

## Changed

- `.github/workflows/ci.yml`: the dylint job installs `cargo-dylint@6.3.0` and
  `dylint-link@6.3.0` instead of 6.0.4. Building 6.0.4's `dylint_driver` failed in its build
  script with "`revparse_ext` failed for `master`". 6.3.0 is what local runs use, and it
  loads the lint libraries at their current tags (`v6.0.4`, `v0.1.2`).
- `Cargo.toml`: the `[workspace.metadata.dylint]` comment now says which driver CI uses.
- `crates/cargo-testcache/tests/fixture.rs`: the conformance comparison leaves out
  `__LLVM_PROFILE_RT_INIT_ONCE` as well as `FIXTURE_DUMP_DIR`, through a documented
  `HARNESS_VARIABLES` list. Under `just coverage`, the LLVM profiling runtime in the
  instrumented `cargo-testcache` sets that variable in its own process, and the test binaries it
  runs directly inherit it. It was the only difference from `cargo test`; a normally built
  `cargo-testcache` never sets it.

## Tests

- `just gate` locally: exit 0, 86 tests passed (dylint with cargo-dylint 6.3.0).
- CI on this branch confirms the dylint job and the coverage run.
