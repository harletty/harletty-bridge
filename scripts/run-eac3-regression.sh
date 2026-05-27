#!/usr/bin/env bash
# Glue: extract MKV → ffmpeg PCM → cavern PCM → harletty PCM → compare → cargo test.
#
# Reads track definitions from
# harletty-bridge/eac3/tests/data/regression-manifest.toml
# (parsed minimally — same format as the cargo test).
#
# Each step is idempotent: if the destination file exists and is newer than
# its inputs, the step is skipped. Use --force to ignore caches.

set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
workspace_root=$(cd "$repo_root/.." && pwd)
corpus_root="$workspace_root/dumps/eac3-regression"
manifest="$repo_root/eac3/tests/data/regression-manifest.toml"

force=0
bootstrap=0
for arg in "$@"; do
    case $arg in
        --force) force=1 ;;
        --bootstrap) bootstrap=1 ;;
        --help|-h)
            cat <<EOF
usage: $0 [--bootstrap] [--force]

  --bootstrap   Only run the MKV→.eac3 extraction step, then stop.
  --force       Ignore caches; regenerate every artifact.
EOF
            exit 0
            ;;
        *) echo "unknown flag: $arg" >&2; exit 64 ;;
    esac
done

for tool in cargo dotnet ffmpeg mkvmerge jq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool" >&2; exit 69; }
done

if [[ ! -f $manifest ]]; then
    echo "manifest not found: $manifest" >&2
    exit 66
fi

# Parse [[track]] entries (id, role, source). Strict format: only those three
# keys, double-quoted strings, one [[track]] header per stanza.
parse_manifest() {
    awk -v IFS='' '
        BEGIN { in_track = 0 }
        function emit() {
            if (id != "" && role != "" && source != "") {
                print id "\t" role "\t" source
            }
            id = ""; role = ""; source = ""
        }
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*\[\[track\]\][[:space:]]*$/ {
            if (in_track) emit()
            in_track = 1
            next
        }
        in_track {
            line = $0
            sub(/#.*$/, "", line)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
            if (line == "") next
            n = index(line, "=")
            if (n == 0) next
            key = line; val = line
            sub(/=.*$/, "", key); gsub(/[[:space:]]+$/, "", key)
            sub(/^[^=]+=[[:space:]]*/, "", val)
            sub(/^"/, "", val); sub(/"$/, "", val)
            if (key == "id") id = val
            else if (key == "role") role = val
            else if (key == "source") source = val
        }
        END { if (in_track) emit() }
    ' "$1"
}

mkdir -p "$corpus_root/ffmpeg-pcm" "$corpus_root/cavern-pcm" "$corpus_root/harletty-pcm" "$corpus_root/reports"

# 1. Build the Rust + dotnet tools once up front (release mode).
echo "[build] cargo eac3 examples (release)" >&2
( cd "$repo_root" && cargo build --release -p eac3 --examples >/dev/null )
echo "[build] dotnet cavern_pcm_dump (release)" >&2
( cd "$repo_root/tools/cavern_pcm_dump" && dotnet build -c Release --nologo --verbosity quiet >/dev/null )

cavern_bin="$repo_root/tools/cavern_pcm_dump/bin/Release/net10.0/cavern_pcm_dump.dll"
if [[ ! -f $cavern_bin ]]; then
    echo "cavern_pcm_dump build output not found: $cavern_bin" >&2
    exit 70
fi

run_step() {
    local label=$1 dest=$2
    shift 2
    if [[ $force -eq 0 && -f $dest ]]; then
        local newer=0
        for src in "$@"; do
            if [[ -f $src && $src -nt $dest ]]; then newer=1; break; fi
        done
        if [[ $newer -eq 0 ]]; then
            echo "[skip] $label ($dest up to date)" >&2
            return 0
        fi
    fi
    return 1
}

parse_manifest "$manifest" | while IFS=$'\t' read -r id role source; do
    [[ -z $id ]] && continue
    echo
    echo "=== track: $id  role=$role ==="

    track_dir="$corpus_root/$id"
    mkdir -p "$track_dir"
    eac3_path="$track_dir/track.eac3"
    ffmpeg_pcm="$corpus_root/ffmpeg-pcm/$id.f32"
    cavern_pcm="$corpus_root/cavern-pcm/$id.f32"
    harletty_pcm="$corpus_root/harletty-pcm/$id.f32"
    report="$corpus_root/reports/$id.txt"

    # 1. Extract from MKV (skips itself based on size+mtime fingerprint cache).
    if [[ ! -f $source ]]; then
        echo "[warn] source MKV not present, skipping track: $source" >&2
        continue
    fi
    "$script_dir/extract-eac3-corpus.sh" "$source" "$id"
    if [[ $bootstrap -eq 1 ]]; then
        continue
    fi

    # 2. FFmpeg PCM.
    if ! run_step "ffmpeg pcm" "$ffmpeg_pcm" "$eac3_path"; then
        "$script_dir/decode-ffmpeg-pcm.sh" "$eac3_path" "$ffmpeg_pcm"
    fi

    # 3. Cavern PCM.
    if ! run_step "cavern pcm" "$cavern_pcm" "$eac3_path" "$cavern_bin"; then
        echo "[cavern] decoding $id" >&2
        dotnet "$cavern_bin" "$eac3_path" "$cavern_pcm"
    fi

    # 4. Harletty PCM (release example).
    harletty_example="$repo_root/target/release/examples/corpus_stream"
    if ! run_step "harletty pcm" "$harletty_pcm" "$eac3_path" "$harletty_example"; then
        echo "[harletty] decoding $id" >&2
        "$harletty_example" "$eac3_path" --mode pcm --out "$harletty_pcm" >"$corpus_root/reports/$id.harletty.tsv"
    fi

    # 5. Compare.
    compare_example="$repo_root/target/release/examples/compare_pcm"
    "$compare_example" \
        --harletty "$harletty_pcm" \
        --ffmpeg "$ffmpeg_pcm" \
        --cavern "$cavern_pcm" \
        --channels 6 --tolerance 1e-3 \
        | tee "$report"
done

if [[ $bootstrap -eq 1 ]]; then
    echo
    echo "[bootstrap] extraction done. Re-run without --bootstrap to decode + compare."
    exit 0
fi

echo
echo "=== cargo regression test ==="
( cd "$repo_root" && cargo test -p eac3 --test mkv_corpus_regression -- --nocapture )
