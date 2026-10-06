#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

# bridge/Cargo.toml takes bridge_api and spdif from an Omniphony checkout next
# to this repository; .omniphony-ref names the commit releases build against.
if [[ ! -d "$repo_dir/../Omniphony/omniphony-renderer/bridge_api" ]]; then
  echo "No Omniphony checkout at $repo_dir/../Omniphony. Clone it there first:" >&2
  echo "  git clone https://github.com/mgth/Omniphony \"$repo_dir/../Omniphony\"" >&2
  echo "  git -C \"$repo_dir/../Omniphony\" checkout $("$repo_dir/scripts/omniphony-ref.sh")" >&2
  exit 1
fi

# -p: the workspace also holds the offline `truehdd` CLI; only build the plugin.
# The artifact stays under the workspace-root `target/`, so paths are unchanged.
cargo build --release -p harletty-bridge

if [[ "$OSTYPE" == msys* || "$OSTYPE" == cygwin* ]]; then
  artifact="$repo_dir/target/release/harletty_bridge.dll"
  label=".dll"
elif [[ "$OSTYPE" == darwin* ]]; then
  artifact="$repo_dir/target/release/libharletty_bridge.dylib"
  label=".dylib"
else
  artifact="$repo_dir/target/release/libharletty_bridge.so"
  label=".so"
fi

if [[ ! -f "$artifact" ]]; then
  echo "Build succeeded but artifact not found: $artifact" >&2
  exit 1
fi

echo "Built $label bridge: $artifact"
