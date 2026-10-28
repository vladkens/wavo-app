#!/usr/bin/env python3
"""CI: signs the macOS release zip's Wavo.app with the Developer ID and notarizes it.

  scripts/sign.py <Wavo-dev-macos.zip>

A job of its own after the build, which never compiles: the certificate never meets the
dependencies' build scripts. The secrets are set by scripts/signing-setup.py: MACOS_CERT_P12
(base64) and MACOS_CERT_PASSWORD go into a keychain of this run's own, deleted at the end, and the
app is signed as scripts/bundle.py signs with a Developer ID, with a secure timestamp; NOTARY_KEY
(the App Store Connect API key's .p8, base64), NOTARY_KEY_ID and NOTARY_ISSUER notarize it. The
ticket is stapled to the app and the zip made again from it.

Without the certificate (a fork, or before the setup) the zip stays as the build made it, signed
ad hoc; without the notary key it is signed but not notarized: a notice each, never a failure.
"""

from __future__ import annotations

import base64
import json
import os
import secrets
import subprocess
import sys
import tempfile
from pathlib import Path

from bundle import DEVELOPER_ID, identity, run, sign


def notice(msg: str) -> None:
    print(f"::notice::{msg}")


def keychain(kc: Path, tmp: Path) -> None:
    """The certificate in a new keychain, first in the search list, codesign allowed to use it."""
    p12, password = tmp / "cert.p12", secrets.token_hex(16)
    p12.write_bytes(base64.b64decode(os.environ["MACOS_CERT_P12"]))
    listed = run("security", "list-keychains", "-d", "user", capture_output=True, text=True).stdout
    run("security", "create-keychain", "-p", password, kc)
    run("security", "set-keychain-settings", "-lut", "3600", kc)
    run("security", "unlock-keychain", "-p", password, kc)
    run("security", "import", p12, "-k", kc, "-P", os.environ["MACOS_CERT_PASSWORD"],
        "-T", "/usr/bin/codesign", capture_output=True)
    run("security", "set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s",
        "-k", password, kc, capture_output=True)
    run("security", "list-keychains", "-d", "user", "-s", kc,
        *(ln.strip().strip('"') for ln in listed.splitlines()))


def notarize(archive: Path, app: Path, tmp: Path) -> None:
    """Notarizes the zip of the signed app and staples the ticket to the app."""
    key = tmp / "notary.p8"
    key.write_bytes(base64.b64decode(os.environ["NOTARY_KEY"]))
    auth = ["--key", key, "--key-id", os.environ["NOTARY_KEY_ID"],
            "--issuer", os.environ["NOTARY_ISSUER"]]
    submit = ["xcrun", "notarytool", "submit", archive, *auth, "--wait", "--timeout", "30m",
              "--output-format", "json"]
    out = subprocess.run([str(a) for a in submit], check=False, capture_output=True, text=True)
    result = json.loads(out.stdout or "{}")
    if result.get("status") != "Accepted":
        if "id" in result:
            run("xcrun", "notarytool", "log", result["id"], *auth)

        sys.exit(f"sign: notarization {result.get('status', 'failed')}: {out.stderr.strip()}")

    run("xcrun", "stapler", "staple", app)


def main() -> int:
    archive = Path(sys.argv[1]).resolve()
    if not os.environ.get("MACOS_CERT_P12"):
        notice("no Developer ID in the secrets (a fork?): the macOS app stays signed ad hoc")
        return 0

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        kc = tmp / "signing.keychain-db"
        try:
            keychain(kc, tmp)
            name = identity()
            if not name.startswith(DEVELOPER_ID):
                sys.exit(f'sign: the secrets\' certificate gave "{name}", not a Developer ID')

            run("ditto", "-x", "-k", archive, tmp)
            app = tmp / "Wavo.app"
            sign(app, name, timestamp=True)
            run("codesign", "--verify", "--strict", app)
            run("ditto", "-c", "-k", "--keepParent", app, archive)
            if os.environ.get("NOTARY_KEY"):
                notarize(archive, app, tmp)
                run("spctl", "--assess", "--type", "execute", "--verbose=2", app)
                run("ditto", "-c", "-k", "--keepParent", app, archive)
            else:
                notice("no notary key in the secrets: the macOS app is signed, not notarized")
        finally:
            if kc.exists():
                run("security", "delete-keychain", kc)

    print(f"{archive.name}: signed with {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
