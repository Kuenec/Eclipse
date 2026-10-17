#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

bench=false
if [ "${1:-}" = --bench ]; then
    bench=true
    shift
fi
out="${1:-$repo/tests/fixtures/media}"
bench_out="$repo/build/media-bench"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

VIDEO_SOURCE="nullsrc=size=64x48:rate=25,geq=lum='128+90*sin(X/7+T*4)*cos(Y/5-T*3)'"
VIDEO_SOURCE+=":cb='128+50*sin(X/11+T*5)':cr='128+50*cos(Y/9-T*6)'"
TONE_HZ=440
AUDIO_SOURCE="sine=frequency=$TONE_HZ:sample_rate=48000:duration=1"
AAC_FRAME_SAMPLES=1024
FRAMES=10
BENCH_SECONDS=60
BENCH_FRAMES=$((30 * BENCH_SECONDS))
BENCH_SOURCE="testsrc2=size=1920x1080:rate=30,noise=alls=12:allf=t"

run_ffmpeg() {
    ffmpeg -hide_banner -loglevel error -nostdin -y "$@"
}

require_encoders() {
    local encoders name
    encoders="$(ffmpeg -hide_banner -encoders 2>/dev/null)"
    for name in "$@"; do
        if ! grep -q " $name " <<<"$encoders"; then
            echo "make-fixtures: ffmpeg has no $name encoder; install an ffmpeg built with it" >&2
            exit 1
        fi
    done
}

hex_of() {
    od -An -v -t x1 "$1" | tr -d ' \n'
}

video_size() {
    ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x "$1" |
        tr x ' '
}

packet_index() {
    local source="$1" filter="$2"
    run_ffmpeg -i "$source" -map 0:0 -c copy -bsf:0 "$filter" -f framecrc - | awk -F', *' '
        /^#tb 0:/ { split($0, field, " "); split(field[3], tb, "/"); next }
        /^#/ { next }
        {
            if (sep == "") {
                first_pts = $3
            }
            key = ($0 !~ /F=0x/) || ($0 ~ /F=0x[0-9A-F]*[13579BDF](,|$)/)
            pts_us = int(($3 - first_pts) * 1000000 * tb[1] / tb[2])
            printf "%s\n    {\"offset\": %d, \"size\": %d, ", sep, offset, $5
            printf "\"pts_us\": %d, \"flags\": %d}", pts_us, key
            sep = ","
            offset += $5
        }'
}

packet_data() {
    run_ffmpeg -i "$1" -map 0:0 -c copy -bsf:0 "$2" -f data "$3"
}

parameter_sets() {
    run_ffmpeg -i "$1" -map 0:0 -c copy -bsf:0 "$2" -frames:0 1 -f data "$3"
}

frame_hashes() {
    run_ffmpeg -i "$1" -map 0:0 -pix_fmt nv12 -f framehash -hash sha256 - |
        awk -F', *' '!/^#/ { printf "%s\n    \"%s\"", sep, $6; sep = "," }'
}

stream_hash() {
    run_ffmpeg -i "$1" -map 0:0 -pix_fmt nv12 -f hash -hash sha256 - | sed 's/^SHA256=//'
}

write_video() {
    local dir="$1" name="$2" mime="$3" source="$4" annexb="$5" config_types="$6" csd1_types="$7"
    local frames="$8" check="$9"
    local strip="$annexb,filter_units=remove_types=$config_types${csd1_types:+|$csd1_types}"
    local width height
    read -r width height < <(video_size "$source")
    packet_data "$source" "$strip" "$dir/$name.bin"
    parameter_sets "$source" "$annexb,filter_units=pass_types=$config_types" "$work/$name.csd0"
    local csd1=""
    if [ -n "$csd1_types" ]; then
        parameter_sets "$source" "$annexb,filter_units=pass_types=$csd1_types" "$work/$name.csd1"
        csd1="$(hex_of "$work/$name.csd1")"
    fi
    {
        printf '{\n  "mime": "%s",\n  "width": %d,\n  "height": %d,\n' "$mime" "$width" "$height"
        printf '  "csd-0": "%s",\n  "csd-1": "%s",\n' "$(hex_of "$work/$name.csd0")" "$csd1"
        printf '  "packets": [%s\n  ],\n' "$(packet_index "$source" "$strip")"
        printf '  "frames": %d,\n' "$frames"
        if [ "$check" = timed ]; then
            printf '  "bench": true\n}\n'
        else
            printf '  "frame_sha256": [%s\n  ],\n' "$(frame_hashes "$source")"
            printf '  "stream_sha256": "%s"\n}\n' "$(stream_hash "$source")"
        fi
    } >"$dir/$name.json"
}

write_audio() {
    local name="$1" source="$2"
    packet_data "$source" null "$out/$name.bin"
    local csd0
    csd0="$(ffprobe -v error -select_streams a:0 -show_entries stream=extradata -show_data \
        -of default=nw=1:nk=1 "$source" |
        awk '/^[0-9a-f]+: / { hex = hex substr($0, 11, 40) }
            END { gsub(/ /, "", hex); print hex }')"
    {
        printf '{\n  "mime": "audio/mp4a-latm",\n  "sample_rate": 48000,\n  "channel_count": 2,\n'
        printf '  "samples_per_frame": %d,\n  "tone_hz": %d,\n' "$AAC_FRAME_SAMPLES" "$TONE_HZ"
        printf '  "csd-0": "%s",\n  "csd-1": "",\n' "$csd0"
        printf '  "packets": [%s\n  ],\n' "$(packet_index "$source" null)"
        printf '  "frames": %d\n}\n' "$FRAMES"
    } >"$out/$name.json"
}

require_encoders libx264 libx265 aac

mkdir -p "$out"

run_ffmpeg -f lavfi -i "$VIDEO_SOURCE" -frames:v "$FRAMES" -c:v libx264 -pix_fmt yuv420p \
    -bf 2 -g "$FRAMES" -x264-params b-adapt=0:scenecut=0 -flags +global_header "$work/h264.mkv"
write_video "$out" h264-bframes video/avc "$work/h264.mkv" h264_mp4toannexb 7 8 "$FRAMES" hashes

x265_params="log-level=error:info=0:pools=none:frame-threads=1"
x265_params+=":bframes=2:b-adapt=0:keyint=$FRAMES:scenecut=0"
run_ffmpeg -f lavfi -i "$VIDEO_SOURCE" -frames:v "$FRAMES" -c:v libx265 -pix_fmt yuv420p \
    -x265-params "$x265_params" -flags +global_header "$work/hevc.mkv"
write_video "$out" hevc video/hevc "$work/hevc.mkv" hevc_mp4toannexb 32-34 "" "$FRAMES" hashes

run_ffmpeg -f lavfi -i "$AUDIO_SOURCE" -ac 2 -c:a aac -b:a 128k "$work/aac-full.m4a"
run_ffmpeg -i "$work/aac-full.m4a" -map 0:0 -c copy -frames:a "$FRAMES" "$work/aac.m4a"
write_audio aac-lc "$work/aac.m4a"

if [ "$bench" = true ]; then
    mkdir -p "$bench_out"
    run_ffmpeg -f lavfi -i "$BENCH_SOURCE" -frames:v "$BENCH_FRAMES" -c:v libx264 \
        -preset medium -pix_fmt yuv420p -b:v 6M -maxrate 8M -bufsize 12M -g 60 \
        -flags +global_header "$bench_out/h264-1080p30.mkv"
    write_video "$bench_out" h264-1080p30 video/avc "$bench_out/h264-1080p30.mkv" \
        h264_mp4toannexb 7 8 "$BENCH_FRAMES" timed
fi
