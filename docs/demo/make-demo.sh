#!/usr/bin/env bash
# Renders docs/demo/boot.gif from a recorded boot, moving the way a replay in
# `bootmap view` does: the dots settle into place while the boot plays in real
# time, then the finished picture is held for a few seconds.
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

"$bootmap" render "$recording" --frames "$frames" --fps "$fps" --size "$size" --ui-scale "$ui_scale"

# Hold the finished picture.
n=$(find "$frames" -name '*.png' | wc -l)
for _ in $(seq $((hold_seconds * fps))); do
  cp "$(printf '%s/%05d.png' "$frames" $((n - 1)))" "$(printf '%s/%05d.png' "$frames" "$n")"
  n=$((n + 1))
done

nix run nixpkgs#gifski -- --fps "$fps" --quality 80 --width "${size%x*}" -o "$out" "$frames"/*.png
echo "wrote $out ($n frames)"
