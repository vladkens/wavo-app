#!/usr/bin/env python3
"""Packages the release binary as Wavo.app and signs it.

  scripts/bundle.py <binary> <icon.png> <Wavo.app>

The icon becomes Wavo.icns (every size of an iconset scaled down from the 1024 px PNG). The
version comes from the binary (`wavo-app --version`, i.e. Cargo.toml), the least macOS too (the
one it is built for, from .cargo/config.toml).

The identity it signs with, the first there is:

1. $WAVO_SIGN_IDENTITY.
2. A Developer ID in the keychains (the maintainer's, from `make signing`; CI's, from its
   secrets): with the hardened runtime and the microphone entitlement, as notarization wants.
   Core ML, the Neural Engine and wgpu's Metal shaders need no other entitlement (tried under the
   hardened runtime), and libmalloc reads MallocSpaceEfficient unless the process is setuid. A
   secure timestamp only for scripts/sign.py, which notarizes: a build here needs no network.
3. In CI, which has neither: ad hoc.
4. Wavo's own, "Wavo Local Signing", made in the login keychain before the first bundle: what a
   contributor gets, with nothing to set up.

A certificate's designated requirement names the bundle id and the certificate (a Developer ID's:
its team), not the code hash, so the permissions macOS grants (Microphone, Accessibility) survive
rebuilds. Wavo's own certificate stays untrusted, which the requirement doesn't need. A bundle
that can't be signed with it (codesign failed or didn't finish in time: a keychain dialog nobody
answers) is no bundle, never one signed ad hoc, whose permissions macOS would keep for that build
only.

LSEnvironment sets MallocSpaceEfficient=1 (libmalloc's src/malloc.c: no large-block cache, empty
pages given back at once): otherwise malloc keeps the large blocks a model frees after its
unload. It applies when LaunchServices starts the app.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ID = "me.vladkens.wavo"
LOCAL = "Wavo Local Signing"
DEVELOPER_ID = "Developer ID Application: "
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

ENTITLEMENTS = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.device.audio-input</key><true/>
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


def identities(valid: bool) -> list[str]:
    """The code signing identities in the keychains, by name; `valid`: only trusted ones."""
    out = run("security", "find-identity", *(["-v"] if valid else []), "-p", "codesigning",
              capture_output=True, text=True).stdout
    return list(dict.fromkeys(re.findall(r'\d+\) [0-9A-F]{40} "(.+)"', out)))


def make_identity() -> None:
    """A self-signed code signing certificate in the login keychain, codesign allowed to use it."""
    with tempfile.TemporaryDirectory() as tmp:
        key, cert, p12 = (Path(tmp) / n for n in ("key.pem", "cert.pem", "identity.p12"))
        openssl = "/usr/bin/openssl"
        run(openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
            "-subj", f"/CN={LOCAL}", "-addext", "keyUsage=critical,digitalSignature",
            "-addext", "extendedKeyUsage=critical,codeSigning", "-keyout", key, "-out", cert,
            capture_output=True)
        run(openssl, "pkcs12", "-export", "-inkey", key, "-in", cert, "-out", p12,
            "-passout", "pass:wavo", capture_output=True)
        keychain = Path.home() / "Library/Keychains/login.keychain-db"
        run("security", "import", p12, "-k", keychain, "-P", "wavo", "-T", "/usr/bin/codesign",
            capture_output=True)

    if LOCAL not in identities(valid=False):
        sys.exit(f'bundle: made "{LOCAL}", but `security find-identity` does not list it')

    print(f'bundle: made the code signing identity "{LOCAL}" in the login keychain', file=sys.stderr)


def identity() -> str:
    """The one to sign with (see above); "-" is ad hoc."""
    if name := os.environ.get("WAVO_SIGN_IDENTITY"):
        return name

    developer = [n for n in identities(valid=True) if n.startswith(DEVELOPER_ID)]
    if len(developer) > 1:
        print(f"bundle: {len(developer)} Developer IDs, signing with the first", file=sys.stderr)

    if developer:
        return developer[0]

    if os.environ.get("CI"):
        return "-"

    if LOCAL not in identities(valid=False):
        make_identity()

    return LOCAL


def sign(app: Path, name: str, timestamp: bool = False) -> None:
    if name == "-":
        run("codesign", "--force", "--sign", "-", "--timestamp=none", app)
        return

    with tempfile.NamedTemporaryFile("w", suffix=".entitlements") as ent:
        flags = ["--timestamp" if timestamp else "--timestamp=none"]
        if name.startswith(DEVELOPER_ID):
            ent.write(ENTITLEMENTS)
            ent.flush()
            flags += ["--options", "runtime", "--entitlements", ent.name]

        try:
            run("codesign", "--force", "--sign", name, *flags, app, timeout=60)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
            shutil.rmtree(app, ignore_errors=True)
            sys.exit(f'bundle: codesign with "{name}" failed ({e}): no bundle')


def main() -> int:
    binary, png, app = (Path(a) for a in sys.argv[1:4])
    version = run(binary, "--version", capture_output=True, text=True).stdout.split()[1]
    build = run("vtool", "-show-build", binary, capture_output=True, text=True).stdout
    macos = next(ln.split()[1] for ln in build.splitlines() if ln.split()[:1] == ["minos"])
    name = identity()

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

    sign(app, name)
    run("codesign", "--verify", "--strict", app)
    print(f"{app}: Wavo {version}, signed: {'ad hoc (CI)' if name == '-' else name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
