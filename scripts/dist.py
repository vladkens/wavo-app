#!/usr/bin/env python3
"""Builds this OS's release files into target/distrib, for CI (vladkens/actions/rust-build).

  scripts/dist.py [target...]

A file per Rust target given, else for this machine; the `dist` profile. Named
`Wavo-dev-<os>[-<arch>]`, renamed `Wavo-vX.Y.Z-…` by a release:

- macOS: `Wavo-dev-macos.zip`, Wavo.app (scripts/bundle.py) zipped by ditto.
- Windows: `Wavo-dev-windows-<x64|arm64>.exe`, which CI packs into a per-user installer
  (vladkens/actions/windows-installer), published in its place.
- Linux: `Wavo-dev-linux-<x86_64|aarch64>.AppImage`: the binary, its .desktop entry and icon. It
  links only glibc and libasound and loads GTK and the rest at run time (nimbo), so nothing else
  goes in. appimagetool and its static runtime (no libfuse2 needed) are pinned below, downloaded
  into target/appimage once and checked by their SHA-256. Beside it `Wavo-dev-linux-<arch>.deb`,
  the same files, by cargo-deb (`[package.metadata.deb]` in Cargo.toml).
"""

from __future__ import annotations

import hashlib
import os
import platform
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "target" / "distrib"
TOOLS = ROOT / "target" / "appimage"
ICON = ROOT / "assets" / "icon.png"

APPIMAGETOOL = "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-{arch}.AppImage"
RUNTIME = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-{arch}"
SHA256 = {
    "appimagetool-x86_64": "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0",
    "appimagetool-aarch64": "f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158",
    "runtime-x86_64": "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d",
    "runtime-aarch64": "00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444",
}

# The app id, nimbo's X11 WM_CLASS and Wayland app id: desktops find the .desktop entry, and its
# icon, by it.
ID = "me.vladkens.wavo"
DESKTOP = f"""[Desktop Entry]
Type=Application
Name=Wavo
Comment=Local dictation: hold a hotkey, speak, and the text lands where you were typing
Exec=wavo-app
Icon={ID}
StartupWMClass={ID}
StartupNotify=true
Categories=Utility;Accessibility;
Terminal=false
"""


def run(*args: str | Path, **kw) -> subprocess.CompletedProcess:
    return subprocess.run([str(a) for a in args], check=True, **kw)


def build(target: str | None) -> Path:
    """The `dist` binary for `target`, or for this machine."""
    to = ["--target", target] if target else []
    run("cargo", "build", "--locked", "--profile", "dist", *to, cwd=ROOT)
    exe = ".exe" if sys.platform == "win32" else ""
    return ROOT / "target" / (target or "") / "dist" / f"wavo-app{exe}"


def arch(target: str | None = None) -> str:
    """`x86_64` or `aarch64`: the target's, else this machine's."""
    machine = target.split("-")[0] if target else platform.machine().lower()
    return {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)


def macos(binary: Path, _arch: str) -> Path:
    app = ROOT / "target" / "Wavo.app"
    run(sys.executable, ROOT / "scripts" / "bundle.py", binary, ICON, app)
    out = OUT / "Wavo-dev-macos.zip"
    run("ditto", "-c", "-k", "--keepParent", app, out)
    return out


def windows(binary: Path, a: str) -> Path:
    name = {"x86_64": "x64", "aarch64": "arm64"}[a]
    out = OUT / f"Wavo-dev-windows-{name}.exe"
    shutil.copy2(binary, out)
    return out


def fetch(url: str, name: str) -> Path:
    """`url` into target/appimage/<name>, once, checked against SHA256."""
    path = TOOLS / name
    if not path.exists():
        TOOLS.mkdir(parents=True, exist_ok=True)
        part = path.with_suffix(".part")
        with urllib.request.urlopen(url) as r, open(part, "wb") as f:
            shutil.copyfileobj(r, f)

        part.rename(path)

    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != SHA256[name]:
        path.unlink()
        sys.exit(f"{name}: SHA-256 {digest}, expected {SHA256[name]}")

    path.chmod(0o755)
    return path


def linux(binary: Path, a: str) -> Path:
    # The tool runs here; the runtime is the AppImage's own, of its arch.
    host = arch()
    tool = fetch(APPIMAGETOOL.format(arch=host), f"appimagetool-{host}")
    runtime = fetch(RUNTIME.format(arch=a), f"runtime-{a}")

    appdir = ROOT / "target" / "Wavo.AppDir"
    shutil.rmtree(appdir, ignore_errors=True)
    (appdir / "usr" / "bin").mkdir(parents=True)
    shutil.copy2(binary, appdir / "usr" / "bin" / "wavo-app")
    (appdir / "AppRun").symlink_to("usr/bin/wavo-app")
    (appdir / f"{ID}.desktop").write_text(DESKTOP)
    shutil.copy2(ICON, appdir / f"{ID}.png")
    (appdir / ".DirIcon").symlink_to(f"{ID}.png")

    out = OUT / f"Wavo-dev-linux-{a}.AppImage"
    # Runs without FUSE (CI has none): the tool unpacks itself.
    env = {**os.environ, "APPIMAGE_EXTRACT_AND_RUN": "1", "ARCH": a}
    run(tool, "--no-appstream", "--runtime-file", runtime, appdir, out, env=env)
    deb(binary, a)
    return out


def deb(binary: Path, a: str) -> Path:
    """The .deb of the binary built for its target: cargo-deb packs what Cargo.toml lists."""
    entry = ROOT / "target" / "deb" / f"{ID}.desktop"
    entry.parent.mkdir(parents=True, exist_ok=True)
    entry.write_text(DESKTOP)
    out = OUT / f"Wavo-dev-linux-{a}.deb"
    # target/<triple>/dist/wavo-app, or target/dist/wavo-app for this machine.
    triple = binary.parent.parent
    to = ["--target", triple.name] if triple != ROOT / "target" else []
    flags = ["--no-build", "--no-strip", "--profile", "dist", "--output", out]
    run("cargo", "deb", *flags, *to, cwd=ROOT)
    return out


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    pack = {"darwin": macos, "win32": windows}.get(sys.platform, linux)
    for target in sys.argv[1:] or [None]:
        out = pack(build(target), arch(target))
        print(f"dist: {out.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
