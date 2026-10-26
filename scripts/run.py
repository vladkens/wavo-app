#!/usr/bin/env python3
"""Starts a built Wavo.app in place of the running one, optionally installing it first.

  scripts/run.py <Wavo.app> [<dest, e.g. /Applications/Wavo.app>]

The bundle has to be signed with a certificate (scripts/bundle.py), never ad hoc: the permissions
macOS grants are kept for its designated requirement. The requirement they were last granted to
is kept in Wavo's data home; when the bundle's differs (another identity, or the first run of this
script), Wavo's permissions are reset first, so System Settings has no stale Wavo whose switch is
on but counts for no build of it any more.

The running Wavo (any copy) is asked to quit (`wavo-app quit`), so it finishes what it is
doing; one still running after 10 s is killed. With a dest the bundle replaces the one there. The
app opens with its log in $WAVO_LOG_FILE (/tmp/wavo.log by default).
"""

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ID = "me.vladkens.wavo"
RUNNING = "Wavo.app/Contents/MacOS/wavo-app"
# In the data home, which `wavo-app uninstall` deletes along with the permissions.
GRANTED = Path.home() / "Library/Application Support" / ID / "permissions-requirement"


def running() -> bool:
    return subprocess.run(["pgrep", "-f", RUNNING], capture_output=True).returncode == 0


def requirement(app: Path) -> str:
    """The bundle's designated requirement; none for one signed ad hoc (its cdhash only)."""
    out = subprocess.run(["codesign", "-d", "-r-", app], capture_output=True, text=True, check=True)
    dr = next((ln for ln in out.stdout.splitlines() if ln.startswith("designated => ")), "")
    if "certificate leaf" not in dr:
        sys.exit(f"{app}: signed ad hoc, not with Wavo's identity (make bundle): not started")

    return dr.removeprefix("designated => ")


def main() -> int:
    app = Path(sys.argv[1])
    dest = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    log = os.environ.get("WAVO_LOG_FILE", "/tmp/wavo.log")
    dr = requirement(app)

    if running():
        subprocess.run([app / "Contents/MacOS/wavo-app", "quit"], capture_output=True)
        for _ in range(50):
            if not running():
                break

            time.sleep(0.2)

        # Still there: killed, and gone before the new one starts (LaunchServices would only
        # activate it).
        subprocess.run(["pkill", "-KILL", "-f", RUNNING], capture_output=True)
        while running():
            time.sleep(0.1)

    if not GRANTED.is_file() or GRANTED.read_text() != dr:
        subprocess.run(["tccutil", "reset", "All", ID], check=True, capture_output=True)
        GRANTED.parent.mkdir(parents=True, exist_ok=True)
        GRANTED.write_text(dr)
        print("a new signature: Wavo's permissions reset, macOS asks for them again")

    if dest:
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["ditto", app, dest], check=True)
        app = dest

    env = "WAVO_LOG=warn,wavo_app=debug"
    subprocess.run(["open", "--stderr", log, "--env", env, app], check=True)
    print(f"started {app}, log in {log}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
