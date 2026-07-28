#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

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
