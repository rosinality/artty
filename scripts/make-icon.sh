#!/bin/sh
# Render artty's app icon from assets/icon.svg: assets/icon.png (1024 px) and
# assets/artty.icns. Needs rsvg-convert (brew install librsvg); iconutil comes
# with macOS. Each size is drawn from the SVG, not scaled down from 1024 px.
set -eu
cd "$(dirname "$0")/.."
command -v rsvg-convert >/dev/null || { echo "needs rsvg-convert: brew install librsvg" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
rsvg-convert -w 1024 -h 1024 assets/icon.svg -o assets/icon.png
mkdir "$tmp/artty.iconset"
for points in 16 32 128 256 512; do
  rsvg-convert -w $points -h $points assets/icon.svg -o "$tmp/artty.iconset/icon_${points}x${points}.png"
  rsvg-convert -w $((points * 2)) -h $((points * 2)) assets/icon.svg -o "$tmp/artty.iconset/icon_${points}x${points}@2x.png"
done
iconutil -c icns "$tmp/artty.iconset" -o assets/artty.icns
echo "wrote assets/icon.png and assets/artty.icns"
