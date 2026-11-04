#!/usr/bin/env bash
# Publishes the release `scripts/release-prepare.sh` made: lands its commit on main and pushes
# its `v<version>` tag, which CI builds the release from.
set -Eeuo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

fail() {
    printf 'release ship failed: %s\n' "$1" >&2
    exit 1
}

[[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] || fail "must ship from main"
[[ -z "$(git status --porcelain)" ]] || fail "working tree is not clean"

TAG="$(git tag --points-at HEAD --list 'v*' | head -n 1)"
readonly TAG
[[ -n "${TAG}" ]] || fail "HEAD carries no release tag; run 'just release-prepare <level>' first"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
readonly VERSION
[[ "${TAG}" == "v${VERSION}" ]] || fail "tag ${TAG} does not match Cargo.toml version ${VERSION}"

# The release commit must sit directly on the current remote main: this catches a change
# merged after the release was prepared.
git fetch origin main
[[ "$(git rev-parse HEAD^)" == "$(git rev-parse refs/remotes/origin/main)" ]] \
    || fail "release commit is not based on current origin/main; prepare it again"
if git ls-remote --exit-code --tags origin "refs/tags/${TAG}" >/dev/null 2>&1; then
    fail "tag ${TAG} already exists on origin"
fi

# main requires a pull request, so the release commit lands through one. Then tag the merge
# commit, which is what main actually holds.
BRANCH="release/${TAG}"
git push origin "HEAD:refs/heads/${BRANCH}"
PR_URL="$(gh pr create --base main --head "${BRANCH}" --title "chore: Release ${TAG}" \
    --body "Version bump prepared by cargo-release. Merging publishes ${TAG}.")"
[[ "${PR_URL}" == https://* ]] || fail "creating the release pull request returned no URL"
# `--auto` waits for required checks before merging.
gh pr merge "${PR_URL}" --merge --delete-branch --auto
until [[ "$(gh pr view "${PR_URL}" --json state --jq .state)" == "MERGED" ]]; do
    printf 'waiting for %s to merge\n' "${PR_URL}" >&2
    sleep 15
done
MERGE_COMMIT="$(gh pr view "${PR_URL}" --json mergeCommit --jq .mergeCommit.oid)"
git fetch origin main
git merge-base --is-ancestor HEAD "${MERGE_COMMIT}" \
    || fail "merge commit ${MERGE_COMMIT} does not contain the release commit"
git tag --force --annotate "${TAG}" "${MERGE_COMMIT}" --message "Release ${TAG}"
git push origin "refs/tags/${TAG}"
git merge --ff-only refs/remotes/origin/main

printf 'released %s\n' "${TAG}"
