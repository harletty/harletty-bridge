#!/usr/bin/env bash
# Assert the two artifacts this workspace builds stay in separate crate graphs.
#
# The repo produces a realtime plugin (harletty-bridge) and an offline CLI
# (truehdd) from one decoder lineage. The whole point of splitting them into
# sibling packages instead of feature-gating one inside the other is that the
# bridge's compile time, binary size and runtime cost are unaffected by the
# CLI's existence. That property is invisible in review — someone adds a
# convenient `use damf::…` in bridge/src and nothing looks wrong — so it is
# checked mechanically here instead of being left to convention.
#
# See docs/plan-truehdd-resurrection-in-harletty.md ("Dependency rules").
set -uo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"

status=0

# $1 = package, $2 = human description, rest = crate names that must be absent
check() {
  local pkg="$1" desc="$2"; shift 2
  local tree
  if ! tree="$(cargo tree -p "$pkg" -e normal 2>&1)"; then
    echo "FAIL: cargo tree -p $pkg failed:" >&2
    echo "$tree" >&2
    status=1
    return
  fi

  local found=()
  for crate in "$@"; do
    # Match a dependency line's crate name: "<tree glyphs><name> v<version>".
    if grep -qE "(^|[^[:alnum:]_-])${crate} v[0-9]" <<<"$tree"; then
      found+=("$crate")
    fi
  done

  if [ ${#found[@]} -gt 0 ]; then
    echo "FAIL: $pkg ($desc) must not depend on: ${found[*]}" >&2
    for crate in "${found[@]}"; do
      echo "  --- path to $crate ---" >&2
      cargo tree -p "$pkg" -e normal -i "$crate" 2>/dev/null | head -20 >&2
    done
    status=1
  else
    echo "ok: $pkg ($desc) is clean of: $*"
  fi
}

# The realtime plugin must never pull in the offline writers or CLI machinery.
# env_logger is deliberately NOT listed: it reaches the bridge through
# Omniphony's `sys` crate, so it is not a CLI-side leak and forbidding it here
# would fail for the wrong reason.
check harletty-bridge "realtime plugin" damf clap indicatif indicatif-log-bridge

# ...and the offline CLI must never pull in the bridge ABI: it stays a pure
# offline tool, buildable without the sibling Omniphony checkout.
check truehdd "offline CLI" bridge_api spdif abi_stable

exit $status
