#!/usr/bin/env bash
# Bumps the workspace version, commits and tags it locally with cargo-release (release.toml).
# Refuses unless on a clean main that is exactly origin/main, so the release contains nothing
# that hasn't been pushed. Usage: scripts/release-prepare.sh <patch|minor|major>
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

fail() {
    printf 'release prepare failed: %s\n' "$1" >&2
    exit 1
}

(($# == 1)) || fail "usage: $0 <patch|minor|major>"
[[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] || fail "must prepare from main"
[[ -z "$(git status --porcelain)" ]] || fail "working tree is not clean"

git fetch origin main
[[ "$(git rev-parse HEAD)" == "$(git rev-parse refs/remotes/origin/main)" ]] \
    || fail "local main is not exactly origin/main; pull first"

exec cargo release --no-publish --no-verify --no-push "$1" --execute
