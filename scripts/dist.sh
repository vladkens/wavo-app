#!/usr/bin/env bash
# This OS's release files in target/distrib, for CI (vladkens/actions/rust-build): the dist
# profile's binaries packed by nimbo's scripts/pack.py (at Cargo.lock's rev) and named
# Wavo-dev-<platform>, as rust-publish wants them; a release renames them Wavo-vX.Y.Z-<platform>.
set -euo pipefail
cd "$(dirname "$0")/.."

# Windows has python, where python3 may be the Store's stub.
py=python3
[ "${OS:-}" = Windows_NT ] && py=python
nimbo=$(cargo metadata --format-version 1 --locked | $py -c '
import json, os, sys
pkgs = json.load(sys.stdin)["packages"]
print(next(os.path.dirname(p["manifest_path"]) for p in pkgs if p["name"] == "nimbo"))')
mkdir -p target/distrib

# pack <command> <triple> <name> [args]: pack.py's file for the triple as
# target/distrib/Wavo-dev-<name>.
pack() {
  rm -rf target/pack
  $py "$nimbo/scripts/pack.py" "$1" --target "$2" --out target/pack "${@:4}"
  mv target/pack/*.* "target/distrib/Wavo-dev-$3"
}

case "$(uname -s)" in
Darwin)
  t=aarch64-apple-darwin
  cargo build --locked --profile dist --target $t
  $py "$nimbo/scripts/pack.py" app --target $t --out target
  pack dmg $t macos-aarch64.dmg target/Wavo.app
  ;;
MINGW* | MSYS*)
  for t in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
    cargo build --locked --profile dist --target $t
  done

  pack setup x86_64-pc-windows-msvc windows-x64-setup.exe
  pack setup aarch64-pc-windows-msvc windows-arm64-setup.exe
  ;;
*)
  for a in x86_64 aarch64; do
    cargo build --locked --profile dist --target $a-unknown-linux-gnu
    pack appimage $a-unknown-linux-gnu linux-$a.AppImage
    pack deb $a-unknown-linux-gnu linux-$a.deb
  done
  ;;
esac

ls -l target/distrib
