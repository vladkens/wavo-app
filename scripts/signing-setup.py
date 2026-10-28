#!/usr/bin/env python3
"""Sets up the signing of Wavo's releases, once, on the maintainer's Mac: they run it twice.

  scripts/signing-setup.py [<developerID_application.cer>] [--dry-run]   (make signing [CER=...])

1. Without a certificate: a private key and a certificate signing request (CSR) for Apple, and
   how to turn it into a Developer ID certificate.
2. With Apple's .cer: the .p12 (the key, the certificate and Apple's Developer ID G2 intermediate,
   under a random password) imported into the login keychain, so `make bundle` signs with it
   (scripts/bundle.py); the App Store Connect API key asked for and stored for notarytool (the
   profile "wavo-notary"); and the repo's secrets for scripts/sign.py set with `gh secret set`:
   MACOS_CERT_P12, MACOS_CERT_PASSWORD, NOTARY_KEY, NOTARY_KEY_ID, NOTARY_ISSUER.

Both also make the update feed's Ed25519 key, once (`openssl genpkey -algorithm ed25519`, which
nimbo's scripts/update-feed.py signs with), and print its public key, the line the app's feed
takes; the second sets the PEM as the secret UPDATE_KEY.

Everything private stays in ~/.config/wavo-signing (mode 700; $WAVO_SIGNING_DIR for another), the
login keychain and the repo's secrets: nothing secret is printed. Done steps are skipped, so it
can run again; a secret already set stays. --dry-run prints the keychain, notarytool and gh
commands instead of running them.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import secrets
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIR = Path(os.environ.get("WAVO_SIGNING_DIR") or Path.home() / ".config/wavo-signing")
# Apple's intermediate for Developer ID certificates, "G2 Sub-CA", pinned.
G2 = "https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer"
G2_SHA256 = "f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a"
# macOS's LibreSSL writes the .p12 `security import` reads; Ed25519 needs OpenSSL 3.
LIBRESSL = "/usr/bin/openssl"
OPENSSL = shutil.which("openssl") or LIBRESSL
PROFILE = "wavo-notary"
DEVELOPER_ID = "Developer ID Application: "

CERTIFICATE = """
Next, the Developer ID certificate (valid 5 years):
  1. Open https://developer.apple.com/account/resources/certificates/add
  2. Pick "Developer ID Application", then "G2 Sub-CA", and upload {csr}
  3. Download the .cer and run: make signing CER=<the .cer>
"""

NOTARY = """
The notary key: open https://appstoreconnect.apple.com/access/integrations/api, "Team Keys",
add a key with the access "Developer" and download its .p8 (it can be downloaded only once).
"""

dry = False


def run(*args: str | Path, **kw) -> str:
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True,
                          **kw).stdout


def act(*args: str | Path, stdin: str | None = None) -> None:
    """A change outside DIR: printed, not run, with --dry-run. Its arguments are never printed
    whole (`security import` takes the password)."""
    if dry:
        print("would run:", " ".join(str(a) for a in args[:4]), "…")
        return

    try:
        run(*args, input=stdin)
    except subprocess.CalledProcessError as e:
        sys.exit(f"{' '.join(str(a) for a in args[:4])} … failed: {e.stderr.strip()}")


def feed_key() -> Path:
    pem = DIR / "update.pem"
    if not pem.exists():
        try:
            run(OPENSSL, "genpkey", "-algorithm", "ed25519", "-out", pem)
        except subprocess.CalledProcessError:
            sys.exit(f"{OPENSSL} makes no Ed25519 key: OpenSSL 3 needed (brew install openssl)")

        print(f"made the update feed's key: {pem}, back it up")

    public = run(OPENSSL, "pkey", "-in", pem, "-pubout").splitlines()[1]
    print(f"the update feed's public key: {public}")
    return pem


def csr() -> Path:
    key, req = DIR / "developer-id.key", DIR / "developer-id.csr"
    if not key.exists():
        run(LIBRESSL, "genrsa", "-out", key, "2048")
        run(LIBRESSL, "req", "-new", "-key", key, "-out", req, "-subj", "/CN=Wavo Developer ID")
        print(f"made the Developer ID's private key and its request: {req}")

    return req


def common_name(cert: Path) -> str:
    subject = run(OPENSSL, "x509", "-in", cert, "-noout", "-subject", "-nameopt", "multiline")
    return next(ln.split("=", 1)[1].strip() for ln in subject.splitlines() if "commonName" in ln)


def p12(cer: Path | None) -> tuple[Path, str, str]:
    """The .p12 from Apple's certificate, its password, and the identity's name."""
    out, password = DIR / "developer-id.p12", DIR / "developer-id.password"
    key, cert, g2 = DIR / "developer-id.key", DIR / "developer-id.pem", DIR / "g2.pem"
    if not out.exists():
        if not cer:
            sys.exit(CERTIFICATE.format(csr=DIR / "developer-id.csr"))

        run(OPENSSL, "x509", "-inform", "DER", "-in", cer, "-out", cert)
        if not common_name(cert).startswith(DEVELOPER_ID):
            sys.exit(f'{cer}: "{common_name(cert)}", not a "Developer ID Application" certificate')

        public = run(OPENSSL, "x509", "-in", cert, "-noout", "-pubkey")
        if public != run(OPENSSL, "pkey", "-in", key, "-pubout"):
            sys.exit(f"{cer}: made for another request, not {DIR / 'developer-id.csr'}")

        der = urllib.request.urlopen(G2, timeout=30).read()
        if hashlib.sha256(der).hexdigest() != G2_SHA256:
            sys.exit(f"{G2}: not the certificate pinned here")

        (DIR / "g2.cer").write_bytes(der)
        run(OPENSSL, "x509", "-inform", "DER", "-in", DIR / "g2.cer", "-out", g2)
        try:
            run(OPENSSL, "verify", "-partial_chain", "-CAfile", g2, cert)
        except subprocess.CalledProcessError:
            sys.exit(f'{cer}: not from the "G2 Sub-CA": make it again with that one')

        password.write_text(secrets.token_urlsafe(24))
        run(LIBRESSL, "pkcs12", "-export", "-inkey", key, "-in", cert, "-certfile", g2,
            "-out", out, "-passout", f"file:{password}")

    return out, password.read_text(), common_name(cert)


def keychain(p12: Path, password: str, name: str) -> None:
    sha1 = run(OPENSSL, "x509", "-in", DIR / "developer-id.pem", "-noout", "-fingerprint", "-sha1")
    sha1 = sha1.split("=", 1)[1].strip().replace(":", "")
    if sha1 in run("security", "find-identity", "-v", "-p", "codesigning"):
        return

    login = Path.home() / "Library/Keychains/login.keychain-db"
    act("security", "import", p12, "-k", login, "-P", password, "-T", "/usr/bin/codesign")
    print(f'"{name}" is in the login keychain: make bundle signs with it')


def notary() -> dict[str, str]:
    """The notary key, asked for once and stored for notarytool."""
    saved, p8 = DIR / "notary.json", DIR / "notary.p8"
    if saved.exists():
        return json.loads(saved.read_text())

    print(NOTARY)
    path = Path(input("The .p8 file: ").strip().strip("'\"")).expanduser()
    key = {"id": input("Its Key ID: ").strip(), "issuer": input("The Issuer ID: ").strip()}
    shutil.copyfile(path, p8)
    act("xcrun", "notarytool", "store-credentials", PROFILE, "--key", p8, "--key-id", key["id"],
        "--issuer", key["issuer"])
    if not dry:
        saved.write_text(json.dumps(key))

    print(f'the notary key is stored for notarytool as "{PROFILE}"')
    return key


def github(values: dict[str, str]) -> None:
    """The secrets not set yet; the repo is the one gh sees here."""
    repo, have = "<repo>", set()
    if not dry:
        repo = run("gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner",
                   cwd=ROOT)
        repo = repo.strip()
        names = run("gh", "secret", "list", "-R", repo, "--json", "name", "-q", ".[].name")
        have = set(names.split())

    for name, value in values.items():
        if name not in have:
            act("gh", "secret", "set", name, "-R", repo, stdin=value)

    print(f"the secrets of {repo} are set: {', '.join(values)}")


def main() -> int:
    global dry
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("cer", nargs="?", type=Path, help="the Developer ID certificate from Apple")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    dry = args.dry_run

    os.umask(0o077)
    DIR.mkdir(parents=True, exist_ok=True)
    DIR.chmod(0o700)
    update = feed_key()
    req = csr()
    if not args.cer and not (DIR / "developer-id.p12").exists():
        print(CERTIFICATE.format(csr=req))
        return 0

    cert, password, name = p12(args.cer)
    keychain(cert, password, name)
    key = notary()
    github({
        "MACOS_CERT_P12": base64.b64encode(cert.read_bytes()).decode(),
        "MACOS_CERT_PASSWORD": password,
        "NOTARY_KEY": base64.b64encode((DIR / "notary.p8").read_bytes()).decode(),
        "NOTARY_KEY_ID": key["id"],
        "NOTARY_ISSUER": key["issuer"],
        "UPDATE_KEY": update.read_text(),
    })
    return 0


if __name__ == "__main__":
    sys.exit(main())
