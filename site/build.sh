#!/bin/sh
# Assemble the GitHub Pages site into $1 (default _site): the pages in site/
# plus the screenshots and videos from docs/images, so they live in one place.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$root/_site"}
rm -rf "$out"
mkdir -p "$out/images"
cp -R "$root/site/index.html" "$root/site/en" "$root/site/assets" "$out/"
cp "$root"/docs/images/*.webp "$root"/docs/images/*.png "$root"/docs/images/*.mp4 "$out/images/"
touch "$out/.nojekyll"
echo "site assembled in $out"
