#!/usr/bin/env bash
# The mgth/Omniphony ref this tree builds against, read from `.omniphony-ref`
# at the repository root: its first line that is neither blank nor a `#`
# comment. CI and the release workflow both read it here, so they cannot
# build against different Omniphony trees.
#
#   scripts/omniphony-ref.sh           print the ref as written
#   scripts/omniphony-ref.sh --check   check that it is a release pin, and
#                                      print the full commit it names
#
# A release pin is a tag of mgth/Omniphony, or a full commit SHA that is on
# its `main`: something that still resolves to the same commit when a tag of
# this repository is rebuilt months later. A branch moves, and a commit of an
# unmerged branch goes away once that branch is squash-merged and deleted.
# A pull request may name either while its Omniphony side is unmerged; CI
# then fails its last step until the pin is moved to a release pin.
#
# --check asks the GitHub API (the `gh` CLI, GH_TOKEN in CI).
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
file="$repo_dir/.omniphony-ref"
omniphony=mgth/Omniphony

if [ ! -f "$file" ]; then
  echo "error: $file is missing; it names the $omniphony commit to build against" >&2
  exit 1
fi
ref=$(grep -v -E '^[[:space:]]*(#|$)' "$file" | head -n 1 | tr -d '[:space:]' || true)
if [ -z "$ref" ]; then
  echo "error: $file names no ref" >&2
  exit 1
fi

case "${1:-}" in
  "")
    echo "$ref"
    exit 0
    ;;
  --check) ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

if [[ "$ref" =~ ^[0-9a-f]{40}$ ]]; then
  # `main...SHA`: "behind" or "identical" when SHA is an ancestor of main.
  if ! status=$(gh api "repos/$omniphony/compare/main...$ref" --jq .status 2>&1); then
    echo "error: $ref is not a commit of $omniphony: $status" >&2
    exit 1
  fi
  case "$status" in
    behind | identical)
      echo "$ref"
      ;;
    *)
      echo "error: $ref is not on $omniphony main (compare: $status); pin a commit of main or a tag" >&2
      exit 1
      ;;
  esac
elif commit=$(gh api "repos/$omniphony/commits/refs/tags/$ref" --jq .sha 2>/dev/null); then
  echo "$commit"
else
  echo "error: '$ref' is neither a tag of $omniphony nor a full commit SHA; a branch is not a release pin" >&2
  exit 1
fi
