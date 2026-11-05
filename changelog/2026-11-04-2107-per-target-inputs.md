# 2026-11-04 21:07 EST: Per-target inputs and spawns (issue #3)

## Summary

`inputs` and `spawns` applied to a whole package, so a fixture read by one test target reran
every binary of the package. In one real workspace of 114 binaries, a fixture read by 3
binaries reran 72, and a helper binary that only some tests spawn reran 89. A package's table
can now hold `[package.metadata.testcache.targets.<name>]` tables with their own `inputs` and
`spawns`. A binary's key covers the workspace's and its package's declarations plus those of
the target tables that name it, and `--explain` says which level declares a changed input.

## Changed

- `manifest`: `TestcacheConfig::targets`, a map of `TargetConfig` tables that hold only
  `inputs` and `spawns` (`deny_unknown_fields`, so `env`, `always_run` and misspelt keys are
  refused). `from_workspace_metadata` refuses `targets` in the workspace table, even an empty
  one. `target_matches` and `targets_matching` give the matching rule: the key equals the
  target name whatever its kind, or is `lib` (`LIB_TARGET`) and the binary is the library's.
- `workspace`: `RawPackage` reads each package's `targets` from `cargo metadata`. A member's
  `targets.<name>` that names none of its lib, bin, test, bench or example targets, and is not
  `lib` for a package with a library, is an `Error::InvalidManifest` listing the package's
  targets. Dependencies are not checked. `discover::TargetKind::from_cargo` is now
  `pub(crate)` so these kinds are read the same way.
- `inputs`: a third owner, `Owner::Target`, indexes a list of (package index, target key).
  `PackageGlobs::targets` carries a package's target tables. Target globs are rooted at the
  package directory with the same `root_glob` rules, and are warned about as
  ``package `p` target `t` ``. `InputIndex::for_binary` replaces `for_package`: it returns the
  workspace's, the package's and the matching targets' files, each file once.
  `InputIndex::declaring_levels` returns the `InputLevel`s whose globs match a path for a
  binary now, matched against the globs so a removed file is covered too. The walk, its
  pruning and the parallel hashing are unchanged.
- `app`: `spawned_paths` adds the matching targets' spawns as `target <name> spawns <path>`.
  Workspace and package spawns stay `spawns <path>`. Each planned binary that runs gets an
  explanation, built from its reason and the input index while planning, which `--explain`,
  the run-everything shortcut and `status` print.
- `plan`: `RunReason::describe` words a reason with an optional note after each change. Its
  `Display` is the same text without notes. `Change` is unchanged, so plan stays free of
  configuration.
- `key`: `KEY_FORMAT_VERSION` 2 to 3, since spawn names now carry their level.
- Fixture: `alpha` has `[package.metadata.testcache.targets.reads_input]` declaring
  `reads_input_data/**`, which `tests/reads_input.rs` now reads.
- README configuration section and design sections 4.2 (key parts 2 and 3), 5 (the target
  tables and their rules) and 6 (`--explain`).

## Tests

- New properties:
  - manifest: target tables are read as written; another key in a target table (`env` and
    `always_run` included) is refused, naming the package table; `targets` in the workspace
    table is refused; the matching rule.
  - workspace: a target name that matches nothing is refused, and a real one accepted; `lib`
    is accepted only for a package with a library, whatever its library kind; build scripts
    are not targets; dependencies' target tables are not checked.
  - inputs: a target-level file reaches only the binaries its table names, of every kind
    sharing the name, and `lib` reaches the library; workspace and package files reach every
    binary of the package; another package's target of the same name does not see them; a
    package outside the root with only target inputs makes them unavailable; the levels
    declaring a path, including a missing file's.
  - plan: `describe` puts each note after its own change.
  - app: explain names the declaring levels for a path; spawn names show their level.
  - fixture: `target_tables_rerun_only_their_target`. Changing alpha's target input reruns
    `reads_input` alone, and `--explain` says ``(target `reads_input`)``. With
    `spawns = ["betatool"]` added to that table, changing `betatool` reruns `reads_input`,
    `betatool` and `spawns_tool`, but not alpha's unit tests.
- Mutation check: making target owners apply to every binary of their package fails the new
  inputs, app and fixture tests.
- `just fmt`, `cargo clippy --all-targets -- -D warnings`, `just test` and `just gate`: all
  exit 0. That is 81 core unit tests and 13 fixture tests, with the Hegel canary at 100 cases.
  The fixture tests also pass with `CI=true HEGEL_DATABASE=disabled`.
- Real tool on a copy of the fixture: first run 6 run, second run 6 skipped. After changing
  `alpha/reads_input_data/value.txt`, `--explain` printed ``Running tests/reads_input.rs
  (alpha/reads_input): input alpha/reads_input_data/value.txt changed (target `reads_input`)``
  and `1 run (1 changed), 5 skipped`. A misspelt `targets.reads_inptu` was refused.

## Decisions

- An owner's target is an index into a list of (package index, target key), not the name
  itself, so `Owner` stays `Copy` and small.
- The explanation is computed while planning and stored with each binary that runs. The input
  index exists only during planning, and `status` shows the same text.
- Build-script targets do not count as targets a table may name: they have no test binary.
- A package outside the workspace root that declares inputs at either level makes all of its
  binaries' inputs unavailable (they run), as before for package-level inputs.

## Not done

- `env` and `always_run` per target, which the issue left package-level.
