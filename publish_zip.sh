#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_name="$(basename "$repo_dir")"
stamp="$(date -u +%Y%m%d-%H%M%S)"
output_zip="${1:-$repo_dir/${repo_name}-${stamp}.zip}"

tmp_dir="$(mktemp -d)"
stage_dir="$tmp_dir/$repo_name"

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

mkdir -p "$stage_dir"

rsync -a \
  --exclude '.git/' \
  --exclude '.git' \
  --exclude '.gitignore' \
  --exclude '.gitattributes' \
  --exclude '.gitmodules' \
  --exclude '.github/' \
  --exclude 'target/' \
  --exclude '.idea/' \
  --exclude '*.zip' \
  "$repo_dir/" "$stage_dir/"

mkdir -p "$(dirname "$output_zip")"
rm -f "$output_zip"

(
  cd "$tmp_dir"
  zip -qr "$output_zip" "$repo_name"
)

echo "Created: $output_zip"
