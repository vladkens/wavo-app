#!/usr/bin/env bash
# Speech fixtures for the tests: 16 kHz mono 16-bit WAV plus the reference transcript (.txt).
#
#   scripts/gen-fixtures.sh [out_dir]    # default: target/fixtures
#
# Texts live in scripts/fixtures/*.txt; blank lines between paragraphs become 2.5 s pauses. Lengths
# follow real dictations (median ~4 s, p99 ~27 s): nothing is longer than ~30 s. Needs the macOS
# `say` voices Milena (ru), Samantha and Daniel (en), and ffmpeg.
set -euo pipefail

src="$(cd "$(dirname "$0")/fixtures" && pwd)"
out="${1:-target/fixtures}"
mkdir -p "$out"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

for tool in say ffmpeg ffprobe; do
  command -v "$tool" >/dev/null || { echo "gen-fixtures: $tool not found" >&2; exit 1; }
done

ff() { ffmpeg -nostdin -loglevel error -y "$@"; }

# speak <name> <voice>: scripts/fixtures/<name>.txt -> <out>/<name>.wav + <out>/<name>.txt.
speak() {
  local name="$1" voice="$2"
  awk 'BEGIN { RS = "" } NR > 1 { printf " [[slnc 2500]] " } { printf "%s", $0 }' \
    "$src/$name.txt" > "$tmp/$name.say"
  say -v "$voice" -o "$tmp/$name.aiff" -f "$tmp/$name.say"
  ff -i "$tmp/$name.aiff" -ar 16000 -ac 1 -c:a pcm_s16le "$out/$name.wav"
  cp "$src/$name.txt" "$out/$name.txt"
}

speak ru-short Milena
speak ru-typical Milena
speak ru-long Milena
speak en-short Samantha
speak en-paragraph Daniel

# The en phrase again as 44.1 kHz stereo float: exercises WAV decoding, downmix and resampling.
ff -i "$tmp/en-short.aiff" -ar 44100 -ac 2 -c:a pcm_f32le "$out/en-short-44k.wav"
cp "$src/en-short.txt" "$out/en-short-44k.txt"

# No speech: digital silence and quiet pink noise, 5 s each.
ff -f lavfi -i anullsrc=r=16000:cl=mono -t 5 -c:a pcm_s16le "$out/silence.wav"
ff -f lavfi -i anoisesrc=d=5:c=pink:r=16000:a=0.05 -ac 1 -c:a pcm_s16le "$out/noise.wav"

for wav in "$out"/*.wav; do
  secs="$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$wav")"
  printf '%-28s %6.1f s\n' "$(basename "$wav")" "$secs"
done
