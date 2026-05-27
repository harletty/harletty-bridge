#!/usr/bin/env bash
# Decode a raw .eac3 elementary stream to interleaved float32 PCM using ffmpeg.
#
# Channel order:
#   FFmpeg's 5.1 layout is FL FR FC LFE BL BR (channel_layout=5.1(side) → FL FR FC LFE SL SR).
#   That matches harletty's CorePcmFrame order (fullband_channels +
#   lfe_channel) for a 5.1 side bed.
#
# `-drc_scale 0` disables FFmpeg's default dynamic-range compression
# (applied via the dynrng word per audblk). harletty and Cavern don't
# apply DRC, so leaving it on inflates the cross-decoder RMSE by 1.7e-4
# on LFE → 5.4e-4 on surrounds (verified on Dune Part Two 30 s slice).
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
    -drc_scale 0 \
    -i "$input" \
    -f f32le -c:a pcm_f32le \
    -channel_layout 5.1\(side\) -ac 6 \
    "$output" </dev/null

bytes=$(stat -c %s "$output")
samples=$(( bytes / 4 / 6 ))
echo "[ffmpeg-pcm] $(basename "$input") -> $output (6ch, $samples samples/ch, $bytes bytes)" >&2
