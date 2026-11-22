#!/usr/bin/env bash
# Renders docs/demo/boot.gif from a recorded boot, moving the way a replay in
# `bootmap replay` does: the dots settle into place while the boot plays in real
# time and burns down. The render runs on a moment past the last event, which
# leaves the center on its own for about a second at the end.
#
#   docs/demo/make-demo.sh [recording.json]
#
# Needs nix (for gifski and, unless BOOTMAP is set, a release build of bootmap).
set -euo pipefail

cd "$(dirname "$0")/../.."
recording=${1:-samples/mbair-initrd-boot.json}
out=docs/demo/boot.gif
fps=12
size=1200x750
ui_scale=1.35

bootmap=${BOOTMAP:-$(nix build .#bootmap --no-link --print-out-paths)/bin/bootmap}
frames=$(mktemp -d)
trap 'rm -rf "$frames"' EXIT

"$bootmap" render "$recording" --frames "$frames" --fps "$fps" --size "$size" --ui-scale "$ui_scale"
n=$(find "$frames" -name '*.png' | wc -l)

nix run nixpkgs#gifski -- --fps "$fps" --quality 80 --width "${size%x*}" -o "$out" "$frames"/*.png
echo "wrote $out ($n frames)"
