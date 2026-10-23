#!/bin/sh
# The app icon as .icns: every size of an .iconset scaled down from one 1024 px PNG.
#
#   scripts/icns.sh <icon.png> <Wavo.icns>
set -eu

png=$1
icns=$2
set=${icns%.icns}.iconset
rm -rf "$set"
mkdir -p "$set"
for size in 16 32 128 256 512; do
  sips -z $size $size "$png" --out "$set/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$png" --out "$set/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns -o "$icns" "$set"
