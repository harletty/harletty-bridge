#!/usr/bin/env bash
# Build libopus from its release source as a static library, for the IAMF
# plugin on platforms with no system libopus to link (Windows, macOS): the
# plugin stays a single file, with libopus inside it.
#
#   scripts/build-static-opus.sh [<prefix>]
#
# Installs into <prefix> (default target/opus) and prints the two variables
# the iamf-opus-ffi build script reads, OPUS_LIB_DIR and OPUS_STATIC=1; in a
# GitHub Actions job it also appends them to $GITHUB_ENV, so the following
# steps link it. Needs curl, tar, a C compiler and CMake 3.16 or later.
#
# On Windows the library uses the static C runtime (/MT), as the plugins do
# (.cargo/config.toml): one runtime in the DLL, and nothing to install.
set -euo pipefail

OPUS_VERSION=1.6.1
OPUS_SHA256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix="${1:-$repo_dir/target/opus}"
mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"
work="$prefix/build"

if [[ ! -f "$prefix/.built-$OPUS_VERSION" ]]; then
  rm -rf "$work"
  mkdir -p "$work"
  tarball="$work/opus-$OPUS_VERSION.tar.gz"
  curl -sSfL -o "$tarball" "https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz"
  actual="$( (sha256sum "$tarball" 2>/dev/null || shasum -a 256 "$tarball") | cut -d' ' -f1)"
  if [[ "$actual" != "$OPUS_SHA256" ]]; then
    echo "opus-$OPUS_VERSION.tar.gz: sha256 $actual, expected $OPUS_SHA256" >&2
    exit 1
  fi
  tar -xzf "$tarball" -C "$work"

  # CMake on Windows is a native program: it reads C:/ paths, not the
  # shell's /c/ ones.
  native() {
    case "$OSTYPE" in
      msys* | cygwin* | win*) cygpath -m "$1" ;;
      *) printf '%s\n' "$1" ;;
    esac
  }
  cmake_args=(
    -S "$(native "$work/opus-$OPUS_VERSION")"
    -B "$(native "$work/cmake")"
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX="$(native "$prefix")"
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DBUILD_SHARED_LIBS=OFF
    -DOPUS_BUILD_SHARED_LIBRARY=OFF
    -DOPUS_BUILD_TESTING=OFF
    -DOPUS_BUILD_PROGRAMS=OFF
    -DOPUS_INSTALL_PKG_CONFIG_MODULE=OFF
    -DOPUS_INSTALL_CMAKE_CONFIG_MODULE=OFF
  )
  case "$OSTYPE" in
    msys* | cygwin* | win*)
      cmake_args+=(-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DCMAKE_POLICY_DEFAULT_CMP0091=NEW)
      ;;
    darwin*)
      # Rust's own default for the target, so the linker does not warn.
      cmake_args+=(-DCMAKE_OSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}")
      ;;
  esac
  cmake "${cmake_args[@]}"
  cmake --build "$(native "$work/cmake")" --config Release --parallel
  cmake --install "$(native "$work/cmake")" --config Release
  touch "$prefix/.built-$OPUS_VERSION"
fi

lib_dir="$prefix/lib"
if [[ ! -f "$lib_dir/libopus.a" && ! -f "$lib_dir/opus.lib" ]]; then
  echo "no static libopus in $lib_dir" >&2
  exit 1
fi
case "$OSTYPE" in
  msys* | cygwin* | win*) lib_dir="$(cygpath -w "$lib_dir")" ;;
esac
echo "OPUS_LIB_DIR=$lib_dir"
echo "OPUS_STATIC=1"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  {
    echo "OPUS_LIB_DIR=$lib_dir"
    echo "OPUS_STATIC=1"
  } >> "$GITHUB_ENV"
fi
