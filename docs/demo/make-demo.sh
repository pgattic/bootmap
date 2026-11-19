#!/usr/bin/env bash
# Renders docs/demo/boot.gif from a recorded boot: every frame is
# `bootmap render` at a point in the boot, played back in real time, then the
# finished picture is held for a few seconds.
#
#   docs/demo/make-demo.sh [recording.json]
#
# Needs nix (for gifski and, unless BOOTMAP is set, a release build of bootmap).
set -euo pipefail

cd "$(dirname "$0")/../.."
recording=${1:-samples/mbair-initrd-boot.json}
out=docs/demo/boot.gif
fps=12
hold_seconds=4
size=1200x750
ui_scale=1.35

bootmap=${BOOTMAP:-$(nix build .#bootmap --no-link --print-out-paths)/bin/bootmap}
frames=$(mktemp -d)
trap 'rm -rf "$frames"' EXIT

# Seconds are relative to the start of userspace: negative is the initrd.
first=$(jq -r '.phases | if .initrd > 0 then -((.userspace - .initrd) / 1e6) else 0 end' "$recording")
last=$(jq -r '.phases | (.finish - .userspace) / 1e6' "$recording")
settle=2.5

n=0
for at in $(awk -v a="$first" -v b="$last" -v s="$settle" -v f="$fps" \
    'BEGIN { for (t = a; t <= b + s; t += 1 / f) printf "%.3f\n", t }'); do
  "$bootmap" render "$recording" --at "$at" --size "$size" --ui-scale "$ui_scale" \
    -o "$(printf '%s/%05d.png' "$frames" "$n")" 2>/dev/null
  n=$((n + 1))
done
# Hold the finished picture.
for _ in $(seq $((hold_seconds * fps))); do
  cp "$(printf '%s/%05d.png' "$frames" $((n - 1)))" "$(printf '%s/%05d.png' "$frames" "$n")"
  n=$((n + 1))
done

nix run nixpkgs#gifski -- --fps "$fps" --quality 80 --width "${size%x*}" -o "$out" "$frames"/*.png
echo "wrote $out ($n frames)"
