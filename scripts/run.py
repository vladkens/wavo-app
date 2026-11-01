#!/usr/bin/env python3
"""Starts a built Wavo.app or Wavo Dev.app in place of its running copy, optionally installing it
first.

  scripts/run.py <Wavo.app> [<dest, e.g. /Applications/Wavo.app>]
  scripts/run.py --copy-history <Wavo Dev.app>

The app is the bundle's, by its id (Info.plist): Wavo Dev (`make run`) has its own, so its own
data home, permissions and running copy, and the installed Wavo is left alone.

The bundle has to be signed with a certificate (`make bundle`), never ad hoc: the permissions
macOS grants are kept for its designated requirement. The requirement they were last granted to
is kept in the app's data home; when the bundle's differs (another identity, or the first run of
this script), the permissions of the bundle's id are reset first, so System Settings has no stale
entry whose switch is on but counts for no build of it any more.

The running copy of the app (any copy of this bundle's name) is asked to quit (`wavo-app quit`),
so it finishes what it is doing; one still running after 10 s is killed. With --copy-history,
Wavo Dev's history is then replaced by a copy of Wavo's, which stays as it is. With a dest the
bundle replaces the one there. The app opens with its log in $WAVO_LOG_FILE (/tmp/wavo.log by
default).
"""

import os
import plistlib
import shutil
import subprocess
import sys
import time
from pathlib import Path

# The release's id, whose history --copy-history copies.
RELEASE = "me.vladkens.wavo"
# In the data home, which `wavo-app uninstall` deletes along with the permissions.
GRANTED = "permissions-requirement"
# The fakes passed to the app, which `open` starts without this shell's environment: the
# microphone's, and shipo's update and failed relaunch (its readme's Fake update).
FAKES = ("WAVO_FAKE_MIC", "SHIPO_FAKE_UPDATE", "SHIPO_UPDATE_ERROR")


def home(app_id: str) -> Path:
    return Path.home() / "Library/Application Support" / app_id


def running(binary: str) -> bool:
    return subprocess.run(["pgrep", "-f", binary], capture_output=True).returncode == 0


def copy_history(src: Path, dst: Path):
    """The history of the home `src` into `dst`, in place of its own, which stays until the copy
    is whole: the database by SQLite's backup, safe while the app at `src` writes, then the
    recordings, cloned (APFS). One the app at `src` records or deletes in between is an entry
    without text or a recording missing in the copy, which the app's recovery handles."""
    db, recordings = src / "history.db", src / "recordings"
    if not db.is_file():
        sys.exit(f"{db}: no history to copy, {dst}'s left as it is")

    new = dst / "history-copy"
    shutil.rmtree(new, ignore_errors=True)
    new.mkdir(parents=True)
    subprocess.run(["sqlite3", db, f".backup '{new / db.name}'"], check=True)

    if recordings.is_dir():
        subprocess.run(["cp", "-Rc", recordings, new / recordings.name], check=True)

    for name in ("history.db", "history.db-wal", "history.db-shm"):
        (dst / name).unlink(missing_ok=True)

    shutil.rmtree(dst / "recordings", ignore_errors=True)
    for path in new.iterdir():
        path.rename(dst / path.name)

    new.rmdir()
    print(f"the history of {src} copied into {dst}")


def requirement(app: Path) -> str:
    """The bundle's designated requirement; none for one signed ad hoc (its cdhash only)."""
    out = subprocess.run(["codesign", "-d", "-r-", app], capture_output=True, text=True, check=True)
    dr = next((ln for ln in out.stdout.splitlines() if ln.startswith("designated => ")), "")
    if "certificate leaf" not in dr:
        sys.exit(f"{app}: signed ad hoc, not with Wavo's identity (make bundle): not started")

    return dr.removeprefix("designated => ")


def main() -> int:
    args = sys.argv[1:]
    copy = "--copy-history" in args
    args = [a for a in args if a != "--copy-history"]
    app = Path(args[0])
    dest = Path(args[1]) if len(args) > 1 else None
    log = os.environ.get("WAVO_LOG_FILE", "/tmp/wavo.log")
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    app_id, name = info["CFBundleIdentifier"], info["CFBundleName"]
    if copy and app_id == RELEASE:
        sys.exit(f"{app}: --copy-history copies Wavo's history into another app, not into Wavo")

    # Of its own name only: Wavo.app's matches /Applications/Wavo.app's, not Wavo Dev.app's.
    binary = f"{(dest or app).name}/Contents/MacOS/wavo-app"
    dr = requirement(app)

    if running(binary):
        # Quits the copy of the bundle's id: the one whose home is that id's.
        subprocess.run([app / "Contents/MacOS/wavo-app", "quit"], capture_output=True)
        for _ in range(50):
            if not running(binary):
                break

            time.sleep(0.2)

        # Still there: killed, and gone before the new one starts (LaunchServices would only
        # activate it).
        subprocess.run(["pkill", "-KILL", "-f", binary], capture_output=True)
        while running(binary):
            time.sleep(0.1)

    if copy:
        copy_history(home(RELEASE), home(app_id))

    granted = home(app_id) / GRANTED
    if not granted.is_file() or granted.read_text() != dr:
        subprocess.run(["tccutil", "reset", "All", app_id], check=True, capture_output=True)
        granted.parent.mkdir(parents=True, exist_ok=True)
        granted.write_text(dr)
        print(f"a new signature: {name}'s permissions reset, macOS asks for them again")

    if dest:
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["ditto", app, dest], check=True)
        app = dest

    env = ["WAVO_LOG=warn,wavo_app=debug"]
    env += [f"{k}={v}" for k in FAKES if (v := os.environ.get(k))]
    flags = [a for e in env for a in ("--env", e)]
    subprocess.run(["open", "--stderr", log, *flags, app], check=True)
    print(f"started {app}, log in {log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
