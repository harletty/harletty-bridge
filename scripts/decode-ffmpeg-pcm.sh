#!/usr/bin/env bash
# Decode a raw .eac3 elementary stream to interleaved float32 PCM using ffmpeg.
#
# Channel order:
#   FFmpeg's 5.1 layout is FL FR FC LFE BL BR (channel_layout=5.1(side) → FL FR FC LFE SL SR).
#   That matches harletty's CorePcmFrame order (fullband_channels +
#   lfe_channel) for a 5.1 side bed.
#
# Usage:
#   decode-ffmpeg-pcm.sh <input.eac3> <output.f32>

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <input.eac3> <output.f32>" >&2
    exit 64
fi

input=$1
output=$2

if [[ ! -f $input ]]; then
    echo "input not found: $input" >&2
    exit 66
fi

ffmpeg -y -hide_banner -loglevel error \
    -i "$input" \
    -f f32le -c:a pcm_f32le \
    -channel_layout 5.1\(side\) -ac 6 \
    "$output"

bytes=$(stat -c %s "$output")
samples=$(( bytes / 4 / 6 ))
echo "[ffmpeg-pcm] $(basename "$input") -> $output (6ch, $samples samples/ch, $bytes bytes)" >&2
