#!/usr/bin/env bash
# Time harletty's decoders against FFmpeg's on the codecs both decode.
#
# Each input is decoded in memory, on one thread, by
# `harletty/examples/decode_bench.rs` and by `tools/ffmpeg-decode-bench`
# (libavcodec), both timing framing plus decoding over the same 64 KiB
# chunks. The table shows the median of the timed passes.
#
# harletty's benches (`decode_bench`, and `bridge_bench` for IAMF) are built
# with every function and every branch target aligned to a 64-byte line
# (`-align-all-functions=6`, `-align-all-nofallthru-blocks=6`), in their own
# target directory (`target/bench`). Without that, a change anywhere in the
# binary moves the other decoders' hot loops across cache lines, and a decoder
# whose code did not change measures a few per cent faster or slower: a change
# to the E-AC-3 decoder once read as a 3 % loss on every lossless DTS stream.
# With it, the same code measures the same whatever sits next to it. The
# shipped CLI is built without these flags, so its absolute times differ
# slightly; the comparison between two harletty versions, and against FFmpeg,
# is what this bench is for. Set BENCH_LAYOUT=plain to build as the CLI is.
#
# Like for like, per codec:
#   TrueHD   harletty presentation 2 (else the widest below it) against FFmpeg,
#            which never decodes past substream 2. On a stream with a
#            presentation 3, harletty decoding it is shown as an extra.
#            Like the CLI, harletty parses only the substreams its
#            presentation is made of; FFmpeg reads every substream up to the
#            one it decodes, so on a stream whose 7.1 has its own substreams
#            the stereo one is work only FFmpeg does.
#   AC-3 /   harletty's bed (core + dependent channel extension) against
#   E-AC-3   FFmpeg with drc_scale=0 (harletty applies no DRC). On a JOC
#            stream, harletty reconstructing the objects is shown as an extra.
#   DTS      harletty's CLI path against FFmpeg's full decode — or against
#            FFmpeg's core_only when harletty decoded the core alone (an
#            XBR-only HRA extension, which harletty does not decode). On a
#            DTS:X stream harletty also decodes the X feeds: compare channels.
#   IAMF     not like for like: harletty's decoder lives in the bridge, so
#            `bridge/examples/bridge_bench.rs` times the bridge (built with
#            the `iamf` feature) decoding and rendering every mix to 7.1.4;
#            FFmpeg renders nothing, it demuxes and decodes the substreams.
#            FFmpeg reads no IAMF v2.0 sequence: those rows are harletty's.
#
# Codec by extension: .thd .mlp → TrueHD; .ac3 → AC-3; .eac3 .ec3 → E-AC-3;
# .dts .dtshd → DTS; .iamf → IAMF.
#
# Usage:
#   scripts/bench-vs-ffmpeg.sh [-n ITERATIONS] [-c CPU] [-j OUT.jsonl] <input>...
#   scripts/bench-vs-ffmpeg.sh -r RESULTS.jsonl
#
#   -n   timed passes per decoder and input (default 5)
#   -c   pin both decoders to this CPU (taskset) to cut scheduler noise
#   -j   also append every raw JSON result to OUT.jsonl
#   -r   print the table of results saved with -j, without running anything

set -euo pipefail

iterations=5
cpu=""
json_out=""
replay=""
while getopts "n:c:j:r:h" opt; do
    case $opt in
        n) iterations=$OPTARG ;;
        c) cpu=$OPTARG ;;
        j) json_out=$OPTARG ;;
        r) replay=$OPTARG ;;
        *) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 64 ;;
    esac
done
shift $((OPTIND - 1))
if [[ -z $replay && $# -eq 0 ]]; then
    echo "usage: $0 [-n ITERATIONS] [-c CPU] [-j OUT.jsonl] <input>..." >&2
    exit 64
fi
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Print the comparison table for a file of tagged JSON results.
table() {
    local results=$1
    echo
    echo "CPU: $(jq -rs '.[0].cpu // "?"' "$results"), $(jq -rs '.[0].iterations' "$results") timed passes, median shown$(jq -rs '.[0].pinned // empty | ", pinned to CPU \(.)"' "$results")."
    echo "FFmpeg $(jq -rs 'map(select(.role == "ffmpeg"))[0].ffmpeg_version // "?"' "$results"); harletty $(jq -rs '.[0].harletty // "?"' "$results")$(jq -rs 'map(select(.role == "harletty"))[0].layout // empty | ", \(.) layout"' "$results")."
    echo "×RT = seconds of audio decoded per second of CPU. ratio = harletty time / FFmpeg time (> 1 = harletty slower)."
    echo
    jq -rs '
        def ms: . * 10 | round / 10;
        def rt($r): ($r.audio_seconds / ($r.median_ms / 1000)) | round;
        def codec_name:
            if .codec == "truehd" then "TrueHD"
            elif .codec == "dts" or .codec == "dca" then "DTS"
            elif .codec == "iamf" then "IAMF"
            elif .role == "ffmpeg" then (.codec | ascii_upcase)
            else "E-AC-3" end;
        group_by(.row)
        | ["| input | codec | audio s | harletty ms | FFmpeg ms | ratio | harletty ×RT | FFmpeg ×RT | channels h/F | harletty extra |",
           "|---|---|---:|---:|---:|---:|---:|---:|---|---|"]
          + map(
            (map(select(.role == "harletty"))[0]) as $h
            | (map(select(.role == "ffmpeg"))[0]) as $f
            | (map(select(.role == "extra"))[0]) as $x
            | if $f == null then
              "| \($h.row) | \($h | codec_name) | \($h.audio_seconds | . * 10 | round / 10)"
              + " | \($h.median_ms | ms) | — | — | \(rt($h)) | — | \($h.channels)/—"
              + "\(if $h.errors > 0 then " ⚠ errors \($h.errors)" else "" end) | |"
              else
              "| \($h.row) | \($f | codec_name)\(if $f.codec == "dca" and $h.hd_frames == 0 then " (core)" else "" end)"
              + " | \($h.audio_seconds | . * 10 | round / 10)"
              + " | \($h.median_ms | ms) | \($f.median_ms | ms)"
              + " | \($h.median_ms / $f.median_ms | . * 100 | round / 100)"
              + " | \(rt($h)) | \(rt($f))"
              + " | \($h.channels)/\($f.channels)\(if $h.samples != $f.samples then " ⚠ samples \($h.samples)≠\($f.samples)" else "" end)"
              + "\(if $h.errors + $f.errors > 0 then " ⚠ errors \($h.errors)/\($f.errors)" else "" end)"
              + " | \(if $x == null then ""
                     elif $x.codec == "truehd" then "presentation 3: \($x.median_ms | ms) ms, \($x.channels) ch"
                     else "JOC objects: \($x.median_ms | ms) ms, \($x.channels) ch" end) |"
              end
          )
        | .[]' "$results"
}

if [[ -n $replay ]]; then
    table "$replay"
    exit 0
fi

for tool in cc pkg-config jq; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }
done

target=${CARGO_TARGET_DIR:-$root/target}

# The bench binaries in their own directory: their RUSTFLAGS would otherwise
# have cargo rebuild everything in the shared one, both ways.
layout=${BENCH_LAYOUT:-aligned}
case $layout in
    aligned) rustflags="-C llvm-args=-align-all-functions=6 -C llvm-args=-align-all-nofallthru-blocks=6" ;;
    plain) rustflags="" ;;
    *) echo "BENCH_LAYOUT must be aligned or plain" >&2; exit 64 ;;
esac
bench_target=$target/bench/$layout
echo "building harletty's decode_bench (release, $layout layout)…" >&2
RUSTFLAGS="$rustflags" cargo build --quiet --release --manifest-path "$root/Cargo.toml" \
    --target-dir "$bench_target" -p harletty --example decode_bench
harletty_bench=$bench_target/release/examples/decode_bench
bridge_bench=""
if printf '%s\n' "$@" | grep -qi '\.iamf$'; then
    echo "building the bridge's bridge_bench with IAMF (release, $layout layout)…" >&2
    RUSTFLAGS="$rustflags" cargo build --quiet --release --manifest-path "$root/Cargo.toml" \
        --target-dir "$bench_target" -p harletty-bridge --example bridge_bench --features iamf
    bridge_bench=$bench_target/release/examples/bridge_bench
fi

echo "building ffmpeg_decode_bench against $(pkg-config --modversion libavcodec | sed 's/^/libavcodec /')…" >&2
ffmpeg_bench=$target/ffmpeg-decode-bench/ffmpeg_decode_bench
mkdir -p "$(dirname "$ffmpeg_bench")"
# shellcheck disable=SC2046 # pkg-config output is meant to split
cc -O2 -o "$ffmpeg_bench" "$root/tools/ffmpeg-decode-bench/ffmpeg_decode_bench.c" \
    $(pkg-config --cflags --libs libavformat libavcodec libavutil)

pin=()
[[ -n $cpu ]] && pin=(taskset -c "$cpu")
cpu_name=$(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1)
harletty_version=$(git -C "$root" describe --always --dirty)

results=$(mktemp)
trap 'rm -f "$results"' EXIT

# Run one bench, tag its JSON with the row and role it belongs to.
run() {
    local row=$1 role=$2
    shift 2
    "${pin[@]}" "$@" | jq -c --arg row "$row" --arg role "$role" --arg cpu "$cpu_name" --arg pin "$cpu" \
            --arg harletty "$harletty_version" --arg layout "$layout" \
            '. + {row: $row, role: $role, cpu: $cpu, harletty: $harletty}
             + (if .decoder == "harletty" then {layout: $layout} else {} end)
             + (if $pin == "" then {} else {pinned: $pin} end)' \
        | tee -a "$results"
}

for input in "$@"; do
    name=$(basename "$input")
    case ${input,,} in
        *.thd | *.mlp)
            echo "TrueHD   $name" >&2
            run "$name" harletty "$harletty_bench" truehd auto "$iterations" "$input" >/dev/null
            run "$name" ffmpeg "$ffmpeg_bench" truehd "$iterations" "$input" >/dev/null
            # A presentation 3 (Atmos) is decodable only by harletty.
            if "$harletty_bench" truehd 3 1 "$input" | jq -e '.errors == 0 and .frames > 0' >/dev/null; then
                run "$name" extra "$harletty_bench" truehd 3 "$iterations" "$input" >/dev/null
            fi
            ;;
        *.ac3 | *.eac3 | *.ec3)
            decoder=eac3
            [[ ${input,,} == *.ac3 ]] && decoder=ac3
            echo "$(printf '%-8s' "${decoder^^}") $name" >&2
            run "$name" harletty "$harletty_bench" eac3 bed "$iterations" "$input" >/dev/null
            run "$name" ffmpeg "$ffmpeg_bench" "$decoder" "$iterations" "$input" drc_scale=0 >/dev/null
            if "$harletty_bench" eac3 objects 1 "$input" | jq -e '.object_frames > 0' >/dev/null; then
                run "$name" extra "$harletty_bench" eac3 objects "$iterations" "$input" >/dev/null
            fi
            ;;
        *.dts | *.dtshd)
            echo "DTS      $name" >&2
            hd=$(run "$name" harletty "$harletty_bench" dts auto "$iterations" "$input" | jq '.hd_frames')
            if [[ $hd -eq 0 ]]; then
                run "$name" ffmpeg "$ffmpeg_bench" dca "$iterations" "$input" core_only=1 >/dev/null
            else
                run "$name" ffmpeg "$ffmpeg_bench" dca "$iterations" "$input" >/dev/null
            fi
            ;;
        *.iamf)
            echo "IAMF     $name" >&2
            run "$name" harletty "$bridge_bench" iamf "$iterations" "$input" >/dev/null
            # FFmpeg reads no IAMF v2.0 sequence (objects): no FFmpeg row then.
            if "$ffmpeg_bench" iamf 1 "$input" >/dev/null 2>&1; then
                run "$name" ffmpeg "$ffmpeg_bench" iamf "$iterations" "$input" >/dev/null
            fi
            ;;
        *)
            echo "skipping $name: unknown extension" >&2
            ;;
    esac
done

[[ -n $json_out ]] && cat "$results" >>"$json_out"
table "$results"

