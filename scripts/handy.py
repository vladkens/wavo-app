#!/usr/bin/env python3
"""Moves the history of Handy (https://github.com/cjpais/Handy), another local dictation app, into
Wavo's, once:

  python3 scripts/handy.py

Handy is not touched: its database is opened read-only, its recordings are only read. The script
writes them into a Wavo archive, ~/Downloads/handy-export.zip, for Wavo's Advanced › Data ›
Import… (as an archive from another computer); entries Wavo has already are left, so importing it
twice adds nothing. $HANDY_DIR is Handy's data dir.

Handy's data dir is ~/Library/Application Support/com.pais.handy by default. It holds `history.db`
and `recordings/`. As of Handy commit 5a9451e70aedc90648ce2befd9d7e0304746f9c1
(src-tauri/src/managers/history.rs):

- `transcription_history (id, file_name, timestamp, ..., transcription_text, post_processed_text,
  ...)`: `timestamp` is Unix seconds, `post_processed_text` the text after Handy's optional LLM pass
  (what it pasted); a Handy from before that pass has no such column.
- `file_name` is `handy-<unix seconds>.wav` in `recordings/`: 16 kHz mono 16-bit, as Wavo's. Of two
  dictations in one second the file is the later one's.

Handy keeps no model or duration per entry: an entry's text is from the model `handy`, its duration
its recording's. A row without text comes with its recording, as an entry no model has tried; one
with neither is left. A row's start is its second in ms plus its id's last three digits, the same
every time, so importing twice adds nothing.

The archive is a Wavo archive (src/transfer.rs): the manifest, then `history.db` in Wavo's
schema 1 (src/history.rs), then the recordings.
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import tempfile
import time
import zipfile
from pathlib import Path

DEFAULT_DIR = Path.home() / "Library/Application Support/com.pais.handy"
OUT = Path.home() / "Downloads/handy-export.zip"
MODEL = "handy"
# Handy's recordings: a 44-byte header, then 16 kHz mono 16-bit samples, 32 bytes a ms.
WAV_HEADER = 44
WAV_BYTES_PER_MS = 32

# Wavo's history, schema 1 (src/history.rs, migrate_v1).
SCHEMA = """
CREATE TABLE entries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created_at INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  audio_path TEXT,
  model TEXT,
  language TEXT,
  text TEXT NOT NULL DEFAULT '',
  pasted INTEGER NOT NULL DEFAULT 0,
  not_pasted TEXT,
  tried_model TEXT,
  busy INTEGER NOT NULL DEFAULT 0,
  prev_text TEXT,
  prev_model TEXT,
  prev_language TEXT
);
CREATE INDEX entries_created_at ON entries (created_at);
CREATE INDEX entries_audio_path ON entries (audio_path);
CREATE INDEX entries_untranscribed ON entries (created_at) WHERE text = '';
PRAGMA user_version = 1;
"""


def read(src: Path) -> list[tuple[int, int, str | None, str]]:
    """Handy's entries with text or a recording: (start ms, duration ms, recording, text)."""
    db_file = src / "history.db"
    if not db_file.is_file():
        sys.exit(f"no Handy history in {src}")

    db = sqlite3.connect(f"{db_file.as_uri()}?mode=ro", uri=True)
    cols = {row[1] for row in db.execute("PRAGMA table_info(transcription_history)")}
    processed = "post_processed_text" if "post_processed_text" in cols else "NULL"
    qs = f"""SELECT id, file_name, timestamp, transcription_text, {processed}
        FROM transcription_history ORDER BY timestamp, id"""
    rows = db.execute(qs).fetchall()
    db.close()

    recordings = src / "recordings"
    listed = recordings.iterdir() if recordings.is_dir() else []
    sizes = {p.name: p.stat().st_size for p in listed if p.is_file()}
    owner = {file: i for i, (_, file, *_) in enumerate(rows)}

    entries = []
    for i, (id, file, at, text, processed) in enumerate(rows):
        text = processed if processed and processed.strip() else text
        own = owner[file] == i and "/" not in file and file not in ("", ".", "..")
        size = sizes.get(file) if own else None
        if not text.strip() and size is None:
            continue

        duration = max(size - WAV_HEADER, 0) // WAV_BYTES_PER_MS if size is not None else 0
        entries.append((at * 1000 + id % 1000, duration, file if size is not None else None, text))

    return entries


def archive(src: Path, out: Path) -> tuple[int, int]:
    """Handy's history in `src` as a Wavo archive `out`; returns its entries and recordings."""
    entries = read(src)
    with tempfile.TemporaryDirectory() as tmp:
        history = Path(tmp) / "history.db"
        db = sqlite3.connect(history)
        db.executescript(SCHEMA)
        qs = """INSERT INTO entries (created_at, duration_ms, audio_path, text, model)
            VALUES (?, ?, ?, ?, ?)"""
        for at, duration, file, text in entries:
            has_text = bool(text.strip())
            row = (at, duration, file, text if has_text else "", MODEL if has_text else None)
            db.execute(qs, row)

        db.commit()
        db.close()

        files = [file for _, _, file, _ in entries if file]
        manifest = {
            "format": 1,
            "version": "handy",
            "created_at": int(time.time() * 1000),
            "entries": len(entries),
            "recordings": len(files),
        }
        with zipfile.ZipFile(out, "w", zipfile.ZIP_STORED) as z:
            z.writestr("wavo-export.json", json.dumps(manifest, indent=2))
            z.write(history, "history.db")
            for file in files:
                z.write(src / "recordings" / file, f"recordings/{file}")

    return len(entries), len(files)


def main() -> int:
    if sys.argv[1:]:
        sys.exit("usage: python3 scripts/handy.py")

    src = Path(os.environ.get("HANDY_DIR") or DEFAULT_DIR)
    entries, recordings = archive(src, OUT)
    print(f"Handy in {src}: {entries} entries, {recordings} recordings")
    print(f"{OUT}: import it in Wavo, Advanced › Data › Import…")
    return 0


if __name__ == "__main__":
    sys.exit(main())
