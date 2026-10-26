#!/usr/bin/env python3
"""Packages the release binary as Wavo.app and signs it.

  scripts/bundle.py <binary> <icon.png> <Wavo.app>

The icon becomes Wavo.icns (every size of an iconset scaled down from the 1024 px PNG). The
version comes from the binary (`wavo-app --version`, i.e. Cargo.toml), the least macOS too (the
one it is built for, from .cargo/config.toml).

The bundle is signed with Wavo's own identity, "Wavo Local Signing" ($WAVO_SIGN_IDENTITY to
use another), made in the login keychain before the first bundle: its designated requirement
names the bundle id and the certificate, not the code hash, so the permissions macOS grants
(Microphone, Accessibility) survive rebuilds. The certificate stays untrusted, which the
requirement doesn't need. A bundle that can't be signed with it (codesign failed or didn't finish
in time: a keychain dialog nobody answers) is no bundle, never one signed ad hoc, whose
permissions macOS would keep for that build only. In CI, which has no identity, it is ad hoc.

LSEnvironment sets MallocSpaceEfficient=1 (libmalloc's src/malloc.c: no large-block cache, empty
pages given back at once): otherwise malloc keeps the large blocks a model frees after its
unload. It applies when LaunchServices starts the app.
"""

from __future__ import annotations

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
        keychain = Path.home() / "Library/Keychains/login.keychain-db"
        run("security", "import", p12, "-k", keychain, "-P", "wavo", "-T", "/usr/bin/codesign",
            capture_output=True)

    if not has_identity():
        sys.exit(f'bundle: made "{IDENTITY}", but `security find-identity` does not list it')

    print(f'bundle: made the code signing identity "{IDENTITY}" in the login keychain', file=sys.stderr)


def sign(app: Path) -> str:
    if os.environ.get("CI"):
        run("codesign", "--force", "--sign", "-", "--timestamp=none", app)
        return "ad hoc (CI)"

    try:
        run("codesign", "--force", "--sign", IDENTITY, "--timestamp=none", app, timeout=60)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
        shutil.rmtree(app, ignore_errors=True)
        sys.exit(f'bundle: codesign with "{IDENTITY}" failed ({e}): no bundle')

    return IDENTITY


def main() -> int:
    binary, png, app = (Path(a) for a in sys.argv[1:4])
    version = run(binary, "--version", capture_output=True, text=True).stdout.split()[1]
    build = run("vtool", "-show-build", binary, capture_output=True, text=True).stdout
    macos = next(ln.split()[1] for ln in build.splitlines() if ln.split()[:1] == ["minos"])
    if not os.environ.get("CI") and not has_identity():
        make_identity()

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
