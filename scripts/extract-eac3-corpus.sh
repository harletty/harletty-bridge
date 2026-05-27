#!/usr/bin/env bash
# Extract a raw E-AC3 audio track from an MKV into the regression corpus.
#
# Usage:
#   extract-eac3-corpus.sh <mkv_path> <track_id> [audio_track_index]
#
# - <mkv_path>            Source MKV (typically on /mnt/nas/...).
# - <track_id>            Short id used as directory name under dumps/eac3-regression/.
# - [audio_track_index]   Optional 0-based audio stream index. Default: first
#                         E-AC3 stream reported by mkvmerge -J.

set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "usage: $0 <mkv_path> <track_id> [audio_track_index]" >&2
    exit 64
fi

mkv_path=$1
track_id=$2
forced_index=${3:-}

for tool in mkvmerge ffmpeg ffprobe sha256sum jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 69
    fi
done

if [[ ! -f $mkv_path ]]; then
    echo "source MKV not found: $mkv_path" >&2
    exit 66
fi

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)
corpus_dir="$workspace_root/dumps/eac3-regression/$track_id"
mkdir -p "$corpus_dir"

# Resolve audio track index via mkvmerge JSON.
if [[ -z $forced_index ]]; then
    audio_index=$(mkvmerge -J "$mkv_path" | jq '
        [ .tracks[] | select(.type=="audio" and (.codec=="E-AC-3" or .codec=="EAC3" or .properties.codec_id=="A_EAC3")) ] | .[0].id
    ')
    if [[ $audio_index == "null" || -z $audio_index ]]; then
        echo "no E-AC3 audio track found in $mkv_path" >&2
        exit 65
    fi
    # mkvmerge "id" is the track id within the matroska container; mapping to
    # ffmpeg stream index requires asking ffprobe directly.
    audio_index=$(ffprobe -v error -select_streams a -show_entries 'stream=index,codec_name' \
        -of csv=p=0 "$mkv_path" \
        | awk -F, '$2=="eac3"{print NR-1; exit}')
    if [[ -z $audio_index ]]; then
        echo "ffprobe could not find an eac3 stream in $mkv_path" >&2
        exit 65
    fi
else
    audio_index=$forced_index
fi

echo "[extract] track_id=$track_id audio_index=$audio_index" >&2
echo "[extract] source=$mkv_path" >&2
echo "[extract] dest=$corpus_dir" >&2

source_sha=$(sha256sum "$mkv_path" | awk '{print $1}')
sha_file="$corpus_dir/track.sha256"
track_file="$corpus_dir/track.eac3"
info_file="$corpus_dir/track.info.txt"

# Skip extraction if cached output is consistent.
if [[ -f $sha_file && -f $track_file ]]; then
    cached=$(awk '{print $1}' "$sha_file" 2>/dev/null || true)
    if [[ $cached == "$source_sha" ]]; then
        echo "[extract] cached output matches source sha256, skipping ffmpeg" >&2
        exit 0
    fi
fi

# Header dump first so it lands even if ffmpeg fails.
{
    echo "# mkvmerge -i"
    mkvmerge -i "$mkv_path"
    echo
    echo "# ffprobe (selected audio stream)"
    ffprobe -hide_banner -select_streams "a:$audio_index" -show_streams "$mkv_path" 2>&1 | head -80
    echo
    echo "# extraction time: $(date --iso-8601=seconds)"
    echo "# source sha256: $source_sha"
} >"$info_file"

# Use ffmpeg to extract raw E-AC3 access units. -c:a copy preserves syncframes
# bit-for-bit; -f eac3 forces the elementary stream muxer.
ffmpeg -y -hide_banner -loglevel error \
    -i "$mkv_path" \
    -map "0:a:$audio_index" \
    -c:a copy -f eac3 \
    "$track_file"

echo "$source_sha  $(basename "$mkv_path")" >"$sha_file"

bytes=$(stat -c %s "$track_file")
echo "[extract] wrote $track_file ($bytes bytes)" >&2
