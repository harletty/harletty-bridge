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

# $1 = package, $2 = human description, rest = crate names that must be absent.
# FEATURES, when set, builds the package with exactly those features
# (--no-default-features --features "$FEATURES") instead of its defaults.
check() {
  local pkg="$1" desc="$2"; shift 2
  local args=(-p "$pkg" -e normal)
  if [ -n "${FEATURES:-}" ]; then
    args+=(--no-default-features --features "$FEATURES")
  fi
  local tree
  if ! tree="$(cargo tree "${args[@]}" 2>&1)"; then
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
      cargo tree "${args[@]}" -i "$crate" 2>/dev/null | head -20 >&2
    done
    status=1
  else
    echo "ok: $pkg ($desc) is clean of: $*"
  fi
}

# The realtime plugin must never pull in the offline writers or CLI machinery,
# nor a logger of its own: its diagnostics go through the host's log sink.
check harletty-bridge "realtime plugin" damf clap indicatif indicatif-log-bridge env_logger

# What the codec families share holds no decoder, and a family holds no other
# family's decoders: an IAMF-only bridge carries none of them
# (docs/plan-codec-family-crates.md).
check bridge-common "shared by the codec families" truehd eac3 dca auro iamf-dec iamf-obu iamf-codecs
check bridge-family-iamf "IAMF family" truehd eac3 dca auro
check bridge-family-dolby "Dolby family" dca auro iamf-dec iamf-obu iamf-codecs
FEATURES=iamf check harletty-bridge "IAMF-only plugin" truehd eac3 dca auro
check bridge-family-dts "DTS family" truehd eac3 iamf-dec iamf-obu iamf-codecs

# ...and the offline CLI must never pull in the bridge ABI: it stays a pure
# offline tool, buildable without the sibling Omniphony checkout.
check harletty "offline CLI" bridge_api spdif abi_stable

exit $status
