#!/usr/bin/env bash
# Copy the licence texts and notices a release archive must carry into DEST,
# under the names the archives list.
#   scripts/stage-licences.sh cli|bridge DEST
# `cli`: the CLI binary is conveyed under LGPL-3.0 (LICENSES/binary-cli.txt),
# so it ships the LGPL-3.0 and GPL-3.0 texts besides the Apache-2.0 and
# LGPL-2.1 texts of its sources. `bridge`: the bridges are GPL-3.0-or-later
# (LICENSES/binary-bridge.txt).
set -euo pipefail
kind="${1:?usage: stage-licences.sh cli|bridge DEST}"
dest="${2:?usage: stage-licences.sh cli|bridge DEST}"
repo="$(cd "$(dirname "$0")/.." && pwd)"

cp "$repo/LICENSE" "$dest/LICENSE.Apache-2.0"
cp "$repo/dca/LICENSE" "$dest/LICENSE.LGPL-2.1"
cp "$repo/dca/NOTICE" "$dest/NOTICE.dca"
cp "$repo/LICENSES/GPL-3.0.txt" "$dest/LICENSE.GPL-3.0"
case "$kind" in
  cli)
    cp "$repo/LICENSES/LGPL-3.0.txt" "$dest/LICENSE.LGPL-3.0"
    cp "$repo/LICENSES/binary-cli.txt" "$dest/LICENSES.txt"
    ;;
  bridge)
    cp "$repo/LICENSES/binary-bridge.txt" "$dest/LICENSES.txt"
    ;;
  *)
    echo "unknown kind: $kind" >&2
    exit 64
    ;;
esac
