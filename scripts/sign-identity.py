#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Makes Wavo's code signing identity, "Wavo Local Signing", in the login keychain when it isn't
there: a self-signed certificate codesign may use. `make bundle` signs Wavo.app with it, so the
permissions macOS grants survive rebuilds."""

import subprocess
import sys
import tempfile
from pathlib import Path

NAME = "Wavo Local Signing"


def has_identity() -> bool:
    out = subprocess.run(
        ["security", "find-identity", "-p", "codesigning"], capture_output=True, text=True
    )
    return f'"{NAME}"' in out.stdout


def main() -> int:
    if has_identity():
        return 0

    with tempfile.TemporaryDirectory() as tmp:
        dir = Path(tmp)
        key, cert, p12 = dir / "key.pem", dir / "cert.pem", dir / "identity.p12"
        openssl = "/usr/bin/openssl"
        subprocess.run(
            [
                openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
                "-subj", f"/CN={NAME}", "-addext", "keyUsage=critical,digitalSignature",
                "-addext", "extendedKeyUsage=critical,codeSigning",
                "-keyout", key, "-out", cert,
            ],
            check=True, capture_output=True,
        )
        subprocess.run(
            [openssl, "pkcs12", "-export", "-inkey", key, "-in", cert, "-out", p12,
             "-passout", "pass:wavo"],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["security", "import", p12, "-P", "wavo", "-T", "/usr/bin/codesign"],
            check=True, capture_output=True,
        )

    print(f'sign-identity: made "{NAME}" in the login keychain', file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
