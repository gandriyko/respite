#!/usr/bin/env bash
# Regenerate crates/app/assets/Respite.icns from crates/app/assets/icon.svg.
# Needs rsvg-convert (brew install librsvg) and iconutil (Xcode Command Line Tools).
set -euo pipefail
cd "$(dirname "$0")/.."

svg=crates/app/assets/icon.svg
out=crates/app/assets/Respite.icns
set_dir=$(mktemp -d)/Respite.iconset
mkdir -p "$set_dir"

for size in 16 32 128 256 512; do
  rsvg-convert -w "$size" -h "$size" "$svg" -o "$set_dir/icon_${size}x${size}.png"
  rsvg-convert -w $((size * 2)) -h $((size * 2)) "$svg" -o "$set_dir/icon_${size}x${size}@2x.png"
done

iconutil -c icns "$set_dir" -o "$out"
rm -rf "$(dirname "$set_dir")"
echo "Wrote $out"
