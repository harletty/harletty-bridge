#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

cargo build --release

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
