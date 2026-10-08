#!/usr/bin/env bash
# Build the three bridge plugins, one per codec family, as a release does:
# harletty_dolby_bridge, harletty_dts_bridge and harletty_iamf_bridge. A
# host loads them together (put all three where it looks for bridges).
#
# The IAMF plugin links libopus: on Linux the system's (pkg-config; install
# libopus-dev or your distribution's equivalent). On Windows and macOS, with
# no OPUS_LIB_DIR set, libopus is built from source first and linked in
# statically (scripts/build-static-opus.sh), as the release does.
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

# The plugins take bridge_api from an Omniphony checkout next to this
# repository; .omniphony-ref names the commit releases build against.
if [[ ! -d "$repo_dir/../Omniphony/omniphony-renderer/bridge_api" ]]; then
  echo "No Omniphony checkout at $repo_dir/../Omniphony. Clone it there first:" >&2
  echo "  git clone https://github.com/mgth/Omniphony \"$repo_dir/../Omniphony\"" >&2
  echo "  git -C \"$repo_dir/../Omniphony\" checkout $("$repo_dir/scripts/omniphony-ref.sh")" >&2
  exit 1
fi

if [[ "$OSTYPE" == msys* || "$OSTYPE" == cygwin* ]]; then
  prefix="" suffix=".dll"
elif [[ "$OSTYPE" == darwin* ]]; then
  prefix="lib" suffix=".dylib"
else
  prefix="lib" suffix=".so"
fi

if [[ "$suffix" != ".so" && -z "${OPUS_LIB_DIR:-}" ]]; then
  while IFS='=' read -r name value; do
    export "$name=$value"
  done < <(scripts/build-static-opus.sh | grep -E '^OPUS_(LIB_DIR|STATIC)=')
fi

# -p: the workspace also holds the offline CLI; only build the plugins. The
# artifacts stay under the workspace-root `target/`.
cargo build --release -p harletty-dolby-bridge -p harletty-dts-bridge -p harletty-iamf-bridge

for family in dolby dts iamf; do
  artifact="$repo_dir/target/release/${prefix}harletty_${family}_bridge${suffix}"
  if [[ ! -f "$artifact" ]]; then
    echo "Build succeeded but artifact not found: $artifact" >&2
    exit 1
  fi
  echo "Built $family bridge: $artifact"
done
