#!/bin/sh
# Packages the release binary as Wavo.app and signs it.
#   scripts/bundle.sh <binary> <icon.icns> <Wavo.app>
#
# The version comes from the binary (`wavo-app --version`, i.e. Cargo.toml), the least macOS from
# $MACOSX_DEPLOYMENT_TARGET (the Makefile's, which the binary is built for). The bundle is signed
# with the identity $WAVO_SIGN_IDENTITY ("Wavo Local Signing" by default, which
# scripts/sign-identity.py makes) when the keychain has it: its designated
# requirement names the bundle id and the certificate, not the code hash,
# so the permissions macOS grants (Microphone, Accessibility) survive rebuilds. Without the
# identity, or when codesign does not finish in time (a keychain dialog nobody answers), the
# bundle is signed ad hoc: it runs, but macOS asks for the permissions again after every rebuild.
#
# LSEnvironment sets MallocSpaceEfficient=1 (libmalloc's src/malloc.c: no large-block cache,
# empty pages given back at once): otherwise malloc keeps the large blocks a model frees after
# its unload, ~55 MB more after the first and ~20 MB more with each one after it
# (`make unload-memory`). It applies when LaunchServices starts the app (Finder, `open`, login).
set -eu

bin=$1
icon=$2
app=$3
id=me.vladkens.wavo
macos=${MACOSX_DEPLOYMENT_TARGET:?the least macOS version, as the binary is built for}
identity=${WAVO_SIGN_IDENTITY:-Wavo Local Signing}
sign_timeout=${WAVO_SIGN_TIMEOUT:-60}

version=$("$bin" --version | awk '{ print $2 }')
[ -n "$version" ] || { echo "bundle: no version from $bin --version" >&2; exit 1; }

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/wavo-app"
cp "$icon" "$app/Contents/Resources/Wavo.icns"
printf 'APPL????' > "$app/Contents/PkgInfo"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$id</string>
  <key>CFBundleName</key><string>Wavo</string>
  <key>CFBundleDisplayName</key><string>Wavo</string>
  <key>CFBundleExecutable</key><string>wavo-app</string>
  <key>CFBundleIconFile</key><string>Wavo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>LSEnvironment</key><dict><key>MallocSpaceEfficient</key><string>1</string></dict>
  <key>LSMinimumSystemVersion</key><string>$macos</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Wavo records your voice while you dictate and turns it into text on this Mac. Nothing leaves your computer.</string>
  <key>NSSupportsAutomaticTermination</key><false/>
  <key>NSSupportsSuddenTermination</key><false/>
</dict>
</plist>
EOF
plutil -lint -s "$app/Contents/Info.plist"
xattr -cr "$app"

# Runs a command, killed after $1 seconds (exit 124 then).
with_timeout() {
  seconds=$1
  shift
  "$@" &
  pid=$!
  ticks=0
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$ticks" -ge $((seconds * 10)) ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi

    sleep 0.1
    ticks=$((ticks + 1))
  done

  wait "$pid"
}

signed=
if security find-identity -p codesigning | grep -qF "\"$identity\""; then
  if with_timeout "$sign_timeout" codesign --force --sign "$identity" --timestamp=none "$app"; then
    signed=$identity
  else
    echo "bundle: codesign with \"$identity\" failed or timed out: signing ad hoc" >&2
  fi
else
  echo "bundle: no code signing identity \"$identity\": signing ad hoc" >&2
fi

if [ -z "$signed" ]; then
  codesign --force --sign - --timestamp=none "$app"
  signed="ad hoc (permissions are asked for again after each rebuild)"
fi

codesign --verify --strict "$app"
echo "$app: Wavo $version, signed: $signed"
