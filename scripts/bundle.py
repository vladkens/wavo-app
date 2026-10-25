#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Packages the release binary as Wavo.app and signs it.

  scripts/bundle.py <binary> <icon.png> <Wavo.app>

The icon becomes Wavo.icns (every size of an iconset scaled down from the 1024 px PNG). The
version comes from the binary (`wavo-app --version`, i.e. Cargo.toml), the least macOS from
$MACOSX_DEPLOYMENT_TARGET (the Makefile's, which the binary is built for).

The bundle is signed with Wavo's own identity, "Wavo Local Signing" ($WAVO_SIGN_IDENTITY to
use another), made in the login keychain the first time (not in CI): its designated requirement
names the bundle id and the certificate, not the code hash, so the permissions macOS grants
(Microphone, Accessibility) survive rebuilds. Without it, or when codesign does not finish in
time (a keychain dialog nobody answers), the bundle is signed ad hoc and macOS asks again after
every rebuild.

LSEnvironment sets MallocSpaceEfficient=1 (libmalloc's src/malloc.c: no large-block cache, empty
pages given back at once): otherwise malloc keeps the large blocks a model frees after its
unload (`make unload-memory`). It applies when LaunchServices starts the app.
"""

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ID = "me.vladkens.wavo"
IDENTITY = os.environ.get("WAVO_SIGN_IDENTITY", "Wavo Local Signing")
MIC = "Wavo records your voice while you dictate and turns it into text on this Mac. Nothing leaves your computer."

PLIST = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>{id}</string>
  <key>CFBundleName</key><string>Wavo</string>
  <key>CFBundleDisplayName</key><string>Wavo</string>
  <key>CFBundleExecutable</key><string>wavo-app</string>
  <key>CFBundleIconFile</key><string>Wavo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>LSEnvironment</key><dict><key>MallocSpaceEfficient</key><string>1</string></dict>
  <key>LSMinimumSystemVersion</key><string>{macos}</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>{mic}</string>
  <key>NSSupportsAutomaticTermination</key><false/>
  <key>NSSupportsSuddenTermination</key><false/>
</dict>
</plist>
"""


def run(*args: str | Path, **kw) -> subprocess.CompletedProcess:
    return subprocess.run([str(a) for a in args], check=True, **kw)


def icns(png: Path, out: Path) -> None:
    with tempfile.TemporaryDirectory(suffix=".iconset") as tmp:
        for size in (16, 32, 128, 256, 512):
            for scale, suffix in ((1, ""), (2, "@2x")):
                px = str(size * scale)
                dest = Path(tmp) / f"icon_{size}x{size}{suffix}.png"
                run("sips", "-z", px, px, png, "--out", dest, capture_output=True)

        run("iconutil", "-c", "icns", "-o", out, tmp)


def has_identity() -> bool:
    out = run("security", "find-identity", "-p", "codesigning", capture_output=True, text=True)
    return f'"{IDENTITY}"' in out.stdout


def make_identity() -> None:
    """A self-signed code signing certificate in the login keychain, codesign allowed to use it."""
    with tempfile.TemporaryDirectory() as tmp:
        key, cert, p12 = (Path(tmp) / n for n in ("key.pem", "cert.pem", "identity.p12"))
        openssl = "/usr/bin/openssl"
        run(openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
            "-subj", f"/CN={IDENTITY}", "-addext", "keyUsage=critical,digitalSignature",
            "-addext", "extendedKeyUsage=critical,codeSigning", "-keyout", key, "-out", cert,
            capture_output=True)
        run(openssl, "pkcs12", "-export", "-inkey", key, "-in", cert, "-out", p12,
            "-passout", "pass:wavo", capture_output=True)
        run("security", "import", p12, "-P", "wavo", "-T", "/usr/bin/codesign", capture_output=True)

    print(f'bundle: made the code signing identity "{IDENTITY}"', file=sys.stderr)


def sign(app: Path) -> str:
    if not has_identity() and not os.environ.get("CI"):
        make_identity()

    if has_identity():
        try:
            run("codesign", "--force", "--sign", IDENTITY, "--timestamp=none", app, timeout=60)
            return IDENTITY
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
            print(f'bundle: codesign with "{IDENTITY}" failed ({e}): signing ad hoc', file=sys.stderr)

    run("codesign", "--force", "--sign", "-", "--timestamp=none", app)
    return "ad hoc (permissions are asked for again after each rebuild)"


def main() -> int:
    binary, png, app = (Path(a) for a in sys.argv[1:4])
    macos = os.environ["MACOSX_DEPLOYMENT_TARGET"]
    version = run(binary, "--version", capture_output=True, text=True).stdout.split()[1]

    shutil.rmtree(app, ignore_errors=True)
    (app / "Contents/MacOS").mkdir(parents=True)
    (app / "Contents/Resources").mkdir(parents=True)
    shutil.copy2(binary, app / "Contents/MacOS/wavo-app")
    icns(png, app / "Contents/Resources/Wavo.icns")
    (app / "Contents/PkgInfo").write_text("APPL????")
    plist = app / "Contents/Info.plist"
    plist.write_text(PLIST.format(id=ID, version=version, macos=macos, mic=MIC))
    run("plutil", "-lint", "-s", plist)
    run("xattr", "-cr", app)

    signed = sign(app)
    run("codesign", "--verify", "--strict", app)
    print(f"{app}: Wavo {version}, signed: {signed}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
