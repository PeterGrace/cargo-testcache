# 2026-11-05 10:30 EST: Stat cache review follow-ups (issue #9)

## Summary

PR #10 was merged before its review comments were addressed. This entry handles the three
comments. `status` now writing `hashes.json` is documented, and the two property tests that
need Unix stat fields are marked `#[cfg(unix)]`. The suggestion to prune the cache to the paths
digested in each run is declined; Decisions explains why.

## Changed

- `README.md`: the `status` usage line and the stat-cache paragraph say that `status` saves the
  digests it computes, though it records no passes.
- `docs/superpowers/specs/2026-11-04-cargo-testcache-design.md`: the `status` entry says the
  same.
- `crates/cargo-testcache-core/src/hashes.rs`:
  `a_settled_digest_is_reused_while_the_metadata_matches` and
  `changed_metadata_or_no_reuse_hashes_again` are `#[cfg(unix)]`, because off Unix nothing is
  remembered. The `expect` message now states its Unix assumption.
- `crates/cargo-testcache/tests/fixture.rs`: the comment on the `status` property now says
  "records no passes" instead of "records nothing".

## Tests

- `just fmt`: no changes. `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `just test`: all passed (2 + 15 fixture + 90 core + 2 doc tests), the two now-`cfg(unix)`
  properties among them on Linux.
- `just dylint` (including `hyp_hegel`): clean.

## Decisions

- Entries are not pruned to the paths digested in the current run. Runs over different subsets
  (`-p alpha`, then the whole workspace) would evict each other's entries and hash those files
  again, which is the cost the cache exists to avoid. Entries for files that no longer exist
  are dropped on save. The rest are bounded by the executables still in the target directory,
  and `cargo clean` removes them all.
