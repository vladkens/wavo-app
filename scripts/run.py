#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Starts a built Wavo.app in place of the running one, optionally installing it first.

  scripts/run.py <Wavo.app> [<dest, e.g. /Applications/Wavo.app>]

The running Wavo (any copy) is asked to quit over its control socket, so it finishes what it is
doing; one still running after 10 s is killed. With a dest the bundle replaces the one there. The
app opens with its log in $WAVO_LOG_FILE (/tmp/wavo.log by default).
"""

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

RUNNING = "Wavo.app/Contents/MacOS/wavo-app"


def running() -> bool:
    return subprocess.run(["pgrep", "-f", RUNNING], capture_output=True).returncode == 0


def main() -> int:
    app = Path(sys.argv[1])
    dest = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    log = os.environ.get("WAVO_LOG_FILE", "/tmp/wavo.log")

    if running():
        subprocess.run([app / "Contents/MacOS/wavo-app", "ctl", "quit"], capture_output=True)
        for _ in range(50):
            if not running():
                break

            time.sleep(0.2)

        # Still there: killed, and gone before the new one starts (LaunchServices would only
        # activate it).
        subprocess.run(["pkill", "-KILL", "-f", RUNNING], capture_output=True)
        while running():
            time.sleep(0.1)

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
