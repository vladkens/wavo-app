# /// script
# requires-python = ">=3.11"
# ///
# The headless app's memory over load and unload cycles of a real model: fails when the footprint
# after the last unload is more than MAX_GROWTH_MB above the one after the first (macOS).
#   uv run scripts/unload-memory.py <wavo-app> <mic.wav> [cycles] [model]
#
# Starts `wavo-app daemon` in a temp WAVO_HOME with <mic.wav> as the microphone and the model
# (a name or path; the first one found in the Hugging Face cache by default) never unloaded on
# its own. Each cycle dictates once over `wavo-app ctl` (which loads the model), then
# `ctl unload`, and reads the process's phys_footprint (Activity Monitor's Memory) with
# `footprint` before the first load, loaded and unloaded. The daemon gets this script's
# environment: `MallocLargeCache=0 uv run …` measures with that set.
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

MAX_GROWTH_MB = 5
ENDED = "engine thread ended"


def main():
    if not 3 <= len(sys.argv) <= 5:
        sys.exit(f"usage: {sys.argv[0]} <wavo-app> <mic.wav> [cycles] [model]")

    exe = Path(sys.argv[1]).resolve()
    mic = Path(sys.argv[2]).resolve()
    cycles = int(sys.argv[3]) if len(sys.argv) > 3 else 10
    model = sys.argv[4] if len(sys.argv) > 4 else None
    with wave.open(str(mic)) as w:
        seconds = w.getnframes() / w.getframerate()

    with tempfile.TemporaryDirectory(prefix="wavo-unload-") as home:
        settings = {"model": model, "unload_after_minutes": None}
        Path(home, "settings.json").write_text(json.dumps(settings))
        env = {**os.environ, "WAVO_HOME": home, "WAVO_FAKE_MIC": str(mic)}
        env["WAVO_LOG"] = "warn,wavo_app=debug"
        log_path = Path(home, "daemon.log")
        with open(log_path, "w") as log:
            daemon = subprocess.Popen(
                [exe, "daemon"], env=env, stdout=subprocess.DEVNULL, stderr=log
            )
        try:
            run(exe, env, daemon.pid, log_path, seconds, cycles)
        except Exception:
            print(f"--- daemon log\n{log_path.read_text()}", file=sys.stderr)
            raise
        finally:
            subprocess.run([exe, "ctl", "quit"], env=env, capture_output=True)
            try:
                daemon.wait(10)
            except subprocess.TimeoutExpired:
                daemon.kill()


def run(exe, env, pid, log_path, seconds, cycles):
    def ctl(*args):
        out = subprocess.run([exe, "ctl", *args], env=env, capture_output=True, text=True)
        if out.returncode != 0:
            raise RuntimeError(f"ctl {' '.join(args)}: {out.stderr.strip()}")
        return out.stdout.strip()

    def status():
        return json.loads(ctl("status"))

    poll("the daemon", 10, lambda: try_status(status))
    base = footprint(pid)
    print(f"start, no model: {mb(base)}")
    loaded, unloaded = [], []
    for take in range(1, cycles + 1):
        ctl("start")
        time.sleep(seconds + 0.4)
        ctl("stop")
        last = poll(
            "the dictation's end",
            120,
            lambda: (s := status())["state"] == "idle"
            and (s.get("last") or {}).get("take") == take
            and s,
        )["last"]
        if last["outcome"] != "done":
            raise RuntimeError(f"dictation {take}: {last['outcome']} {last.get('error') or ''}")

        loaded.append(footprint(pid))
        ctl("unload")
        poll("the model dropped", 30, lambda: log_path.read_text().count(ENDED) >= take)
        time.sleep(0.5)
        unloaded.append(footprint(pid))
        print(f"cycle {take}: loaded {mb(loaded[-1])}, unloaded {mb(unloaded[-1])}")

    growth = unloaded[-1] - unloaded[0]
    print(
        f"after {cycles} cycles: {mb(unloaded[-1])}, {mb(growth)} above the first unload, "
        f"{mb(unloaded[-1] - base)} above the start"
    )
    if growth > MAX_GROWTH_MB << 20:
        sys.exit(f"the footprint grew by {mb(growth)} over {cycles} cycles (> {MAX_GROWTH_MB} MB)")


def try_status(status):
    try:
        return status()
    except RuntimeError:
        return None


def poll(what, timeout, check):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if value := check():
            return value

        time.sleep(0.1)
    raise TimeoutError(f"{what}: not within {timeout} s")


def footprint(pid):
    out = subprocess.run(
        ["footprint", "-p", str(pid), "-f", "bytes", "--noCategories"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return int(re.search(r"phys_footprint: (\d+) B", out).group(1))


def mb(n):
    return f"{n / (1 << 20):.1f} MB"


main()
