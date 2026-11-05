# 2026-11-04 21:44 EST: Optional inputs (issue #4)

## Summary

An `inputs` glob that matched no file was warned about on every run. That is right for a typo,
but some inputs only exist in some checkouts, such as a developer's `.env` read by an opt-in
live test, and for those the warning was noise repeated once per declaring package. Every
`[*.metadata.testcache]` table, and every `targets.<name>` table, now takes
`optional_inputs`: globs keyed exactly like `inputs`, except that matching nothing is not
warned about.

## Changed

- `manifest`: `TestcacheConfig::optional_inputs` and `TargetConfig::optional_inputs`, read from
  the workspace, package and target tables; module docs list the new key.
- `inputs`: a public `Globs` (a table's required and optional globs, built `From` a
  `TestcacheConfig` or `TargetConfig`) replaces the bare slices in `InputPatterns::from_parts`
  and `PackageGlobs::globs`. Each compiled glob records a `Declared` with whether it is
  optional, and `warn_unmatched` skips optional globs via the new `unmatched`. A target table
  with only optional globs now counts as declaring inputs.
- `README.md`: documents `optional_inputs` at each level, and that a glob listed in both
  `inputs` and `optional_inputs` counts as required and is still warned about (from review).
- Tests: `only_required_globs_that_match_nothing_are_warned_about` (inputs unit property); the
  manifest properties cover the new key at both table kinds; the fixture test
  `package_inputs_may_climb_out_of_the_package` now checks an unmatched optional glob is silent
  and that the optional file appearing and then disappearing each rerun alpha's binaries.

## Tests

- `just gate`: formatting, clippy with `-D warnings`, dylint, every test (82 unit, 13 fixture,
  others), the Hegel canary and docs all passed.

## Decisions

- A separate `optional_inputs` key rather than a `?` prefix on the glob: `?` already matches
  any one character, so `?foo` is a valid glob today and a prefix would change its meaning.
- No explicit "absent" marker in the key. A key records each input by its path, so a file
  appearing or disappearing already changes it, the same as for a required glob; the fixture
  test checks both directions.
