# Common development tasks. Run `just` to list them.
#
# CI calls these recipes rather than spelling out its own commands, so a local run and CI
# check exactly the same things. `just gate` is the pre-commit checklist in .claude/CLAUDE.md.

# Recipe arguments reach the shell as "$@", so `just run --help` passes `--help` through.
set positional-arguments

# List the recipes.
default:
    @just --list

# Format the workspace in place, and the fixture workspace the integration tests copy (its own
# workspace, which `--all` does not reach).
fmt:
    cargo fmt --all
    cargo fmt --all --manifest-path crates/cargo-testcache/tests/fixtures/ws/Cargo.toml

# Fail if anything is not formatted, the fixture workspace included.
fmt-check:
    cargo fmt --all --check
    cargo fmt --all --check --manifest-path crates/cargo-testcache/tests/fixtures/ws/Cargo.toml

# Lint every crate and target (tests, examples) with warnings as errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run the dylint lint libraries pinned in `[workspace.metadata.dylint]` (Cargo.toml).
dylint:
    #!/usr/bin/env bash
    set -euo pipefail
    # `cargo dylint` exits 0 when no library loads, so check they all did first.
    listed="$(cargo dylint list --all)"
    for lib in supplementary hyp_hegel; do
        if ! grep -q "^${lib}@" <<<"${listed}"; then
            echo "dylint library '${lib}' did not load; the run would pass vacuously" >&2
            echo "${listed}" >&2
            exit 1
        fi
    done
    # Lint levels are named, not `-D warnings`: the libraries build on their own nightly, and a
    # new rustc warning there should not fail this workspace. To exempt one item, use
    # `#[cfg_attr(dylint_lib = "supplementary", expect(<lint>, reason = "..."))]`.
    deny="-D arg_iter -D commented_out_code -D concatenable_format_args"
    deny+=" -D dir_entry_path_file_name -D escaping_doc_link -D inconsistent_struct_pattern"
    deny+=" -D local_ref_cell -D redundant_reference -D unnecessary_borrow_mut"
    deny+=" -D unnecessary_conversion_for_trait"
    allow="-A nonexistent_path_in_comment -A unnamed_constant"
    # `-- --all-targets` so the lints also see tests.
    DYLINT_RUSTFLAGS="${deny} ${allow}" cargo dylint --lib supplementary --workspace -- --all-targets
    # hyp_hegel requires every test to be a Hegel property test.
    cargo dylint --lib hyp_hegel --workspace -- --all-targets

# Every static check, cheapest first.
lint: fmt-check clippy dylint

# Run every test, doctests included.
test:
    cargo test --workspace

# Prove the Hegel engine generates cases: on success a property test prints the same `ok`
# whether it explored a hundred inputs or none, so count the canary's cases.
hegel-canary:
    #!/usr/bin/env bash
    set -euo pipefail
    floor=50
    out="$(mktemp)"
    trap 'rm -f "${out}"' EXIT
    if ! cargo test -p cargo-testcache-core --test hegel_canary -- --nocapture >"${out}" 2>&1; then
        cat "${out}" >&2
        echo "the hegel canary test itself failed; see above" >&2
        exit 1
    fi
    cases="$(grep -c '^Running test case' "${out}" || true)"
    if (( cases < floor )); then
        cat "${out}" >&2
        echo "hegel generated ${cases} cases, expected at least ${floor}" >&2
        exit 1
    fi
    echo "hegel generated ${cases} cases"

# Build the API docs with warnings (broken links, missing docs) as errors.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Audit dependencies against deny.toml: advisories, licenses, bans and sources.
deny:
    cargo deny --locked check advisories bans licenses sources

# Run the tests under coverage instrumentation and print a summary (needs cargo-llvm-cov).
coverage:
    cargo llvm-cov --workspace --summary-only

# Print the last coverage run as a Markdown table, for a CI step summary.
coverage-markdown:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo llvm-cov report --json --summary-only \
        | jq -r --arg root "$PWD/" '
            def pct: (. * 100 | round) / 100 | tostring + "%";
            def row($name; $metric):
                "| \($name) | \($metric.covered) | \($metric.count) | \($metric.percent | pct) |";
            .data[0] as $data
            | "### Coverage: \($data.totals.lines.percent | pct) of lines", "",
              "| Metric | Covered | Total | % |", "|---|--:|--:|--:|",
              row("Lines"; $data.totals.lines),
              row("Functions"; $data.totals.functions),
              row("Regions"; $data.totals.regions), "",
              "| File | Covered | Total | % |", "|---|--:|--:|--:|",
              ($data.files | map(select(.summary.lines.count > 0))
                | sort_by(.summary.lines.percent)[]
                | row("`\(.filename | ltrimstr($root))`"; .summary.lines))'

# Check the workspace builds on the minimum supported Rust version (`rust-version`).
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    msrv="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)"
    rustup toolchain install "${msrv}" --profile minimal --no-self-update
    cargo "+${msrv}" check --workspace --all-targets --locked

# The pre-commit checklist: everything CI fails on, apart from `deny` and `msrv`.
gate: lint test hegel-canary doc

# Run the binary; arguments pass through (`just run --help`).
run *args:
    cargo run -p cargo-testcache -- "$@"

# Bump the version (`patch`, `minor` or `major`), commit and tag it locally. Run on a clean,
# up-to-date main; `release-ship` publishes it.
release-prepare level:
    scripts/release-prepare.sh "$1"

# Publish the release `release-prepare` made: land its commit on main and push its tag.
release-ship:
    scripts/release-ship.sh

