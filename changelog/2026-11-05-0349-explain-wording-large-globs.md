# 2026-11-05 03:49 EST: Explain wording, oversized globs and a first-run tip (issue #5)

## Summary

Issue #5 came from adopting the tool in a workspace with about 115 test binaries. `--explain`
said `spawned spawns <name> changed`, because it printed a spawned binary's key name as it is.
It also said `env X changed` for a variable that had just been added to `env`, even when the
variable was unset before and after. A `sidecar/**` glob silently hashed 6,180 `node_modules`
files on every run. And nothing told users that a cold run stops at the first failure. Now the
explain text names the binary and says how a variable's key entry differs. A glob that matches
more than 1,000 files is warned about. The README covers both the glob advice and the
`--no-fail-fast` tip.

## Changed

- `plan`: `Change::Env` is now `{ name, change: EnvChange }`. `EnvChange::between` compares a
  variable's old and new key entries and gives `Value`, `Added { set }` or `Removed`. The
  wording is `env X changed`, `env X newly declared, unset`, `env X newly keyed, set` or
  `env X no longer keyed`. `Change::Spawned` is worded by `spawned_parts`:
  `spawned binary x changed`, plus `` (target `t`)`` for a `targets.<name>` spawn.
- `app`: `explain` matches the new `Change::Env` shape.
- `inputs`: the walk counts the files each glob matches instead of recording only whether it
  matched any. A public `LARGE_GLOB_FILES` (1000) and a new `oversized` give the globs over
  that count, required or optional. `warn_about_counts` (formerly `warn_unmatched`) warns
  about those globs as well as about globs that match nothing. The warning names the glob, its
  table and the count, and suggests naming source directories and declaring the lockfile.
- `README.md`: a `--no-fail-fast` tip for a first cold run, the new `env` wording, advice to
  keep globs one level deep and leave dependency directories out, the 1,000-file warning, and
  a target-level spawn example of `--explain`.
- Tests: `env_changes_say_how_the_entry_differs` and `spawned_binaries_are_explained_by_name`
  (plan); the plan properties that check one changed part now work out the expected
  `EnvChange` from the two keys. `globs_matching_nothing_or_too_much_are_warned_about`
  (inputs) replaces the unmatched-only property. The fixture test
  `a_glob_matching_too_many_files_is_warned_about` checks that 1,001 files draw the warning
  and 1,000 do not. Two fixture assertions now expect the new spawned wording.

## Tests

- `just gate`: formatting, clippy with `-D warnings`, dylint, every test (84 unit, 14 fixture,
  2 binary, doctests), the Hegel canary and docs. All pass.
- Manual: in a copy of the fixture workspace, adding `BRAND_NEW_VAR` (unset) to `env` and
  running `--explain` printed `env BRAND_NEW_VAR newly declared, unset` for all six binaries.

## Decisions

- The wording for an added variable that is unset says "newly declared", because only a
  declared variable can be keyed while unset. A newly added variable that is set says "newly
  keyed", because it can also be a `RUST_TEST_TIME_*` variable that was just set and was never
  declared.
- The threshold is a constant, not a setting. The issue asked only for a warning, and a glob
  that big is almost never meant.
- The oversized warning also covers `optional_inputs`, since their files are hashed the same
  way.

## Not done

- Inputs and spawned binaries that appear or disappear still read as `changed`. The issue
  asked only about variables.
