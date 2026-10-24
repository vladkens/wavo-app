// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The history of [Handy](https://github.com/cjpais/Handy), another local dictation app, taken
//! over without touching it: its database is opened read-only, its recordings are copied (APFS
//! clones them), nothing of Handy's is written or deleted.
//!
//! Its data directory (Tauri's app data, id `com.pais.handy`; `HANDY_DIR` overrides it) holds
//! `history.db` and `recordings/`. As of Handy commit `5a9451e70aedc90648ce2befd9d7e0304746f9c1`
//! (`src-tauri/src/managers/history.rs`, `actions.rs`, `audio_toolkit/audio/utils.rs`):
//!
//! - `transcription_history (id, file_name, timestamp, saved, title, transcription_text,
//!   post_processed_text, post_process_prompt, post_process_requested)`; `timestamp` is Unix
//!   seconds, `post_processed_text` the text after Handy's optional LLM pass (what it pasted).
//! - `file_name` is `handy-<unix seconds>.wav` in `recordings/`: 16 kHz mono 16-bit, as Wavo's.
//!
//! Handy keeps no model or duration per entry: an imported entry's model is `handy`, its duration
//! is its recording's.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::warn;

use super::{Item, Progress, Report, Target};
use crate::history::db::{self, Entry};
use crate::history::recordings;

/// The model imported entries name.
const MODEL: &str = "handy";
const DB_FILE: &str = "history.db";
/// Handy's recordings: a 44-byte header, then 16 kHz mono 16-bit samples, 32 bytes a ms.
const WAV_HEADER: u64 = 44;
const WAV_BYTES_PER_MS: u64 = 32;

/// Handy's data directory: `HANDY_DIR`, else Tauri's app data directory for its id
/// (`~/Library/Application Support/com.pais.handy` on macOS).
pub fn dir() -> PathBuf {
  match std::env::var_os("HANDY_DIR") {
    Some(dir) if !dir.is_empty() => PathBuf::from(dir),
    // Tauri's app data is the roaming one on Windows.
    _ if cfg!(windows) => nimbo::paths::config_dir("com.pais.handy"),
    _ => nimbo::paths::data_dir("com.pais.handy"),
  }
}

/// What Handy's history holds: what an import takes the first time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
  pub dir: PathBuf,
  /// Entries with text.
  pub entries: usize,
  /// Of their recordings, those on disk, and how much they take.
  pub recordings: usize,
  pub bytes: u64,
  /// The oldest and the newest entry, Unix ms.
  pub first: Option<i64>,
  pub last: Option<i64>,
}

impl Summary {
  fn new(dir: &Path, read: &Read) -> Self {
    let at = || read.items.iter().map(|x| x.entry.created_at);
    Self {
      dir: dir.to_owned(),
      entries: read.items.len(),
      recordings: read.items.iter().filter(|x| x.audio.is_some()).count(),
      bytes: read.bytes,
      first: at().min(),
      last: at().max(),
    }
  }
}

/// Handy's history on this computer ([`dir`]), if it has one with text. It reads Handy's database
/// and lists its recordings (a second or two for a big one): call it off the UI thread.
#[expect(dead_code, reason = "the settings window's Import from Handy (step 8)")]
pub fn found() -> Option<Summary> {
  let dir = dir();
  if !dir.join(DB_FILE).is_file() {
    return None;
  }

  let read = read(&dir).inspect_err(|e| warn!("Handy's history in {}: {e:#}", dir.display()));
  read.ok().map(|read| Summary::new(&dir, &read)).filter(|s| s.entries > 0)
}

/// Imports Handy's history in `dir` into the history in `home` (see the [module](super) docs); a
/// dry run says what it would do and writes nothing. Returns what Handy has, and what was done.
pub fn import(home: &Path, dir: &Path, dry_run: bool) -> Result<(Summary, Report)> {
  let read = read(dir)?;
  let summary = Summary::new(dir, &read);
  let target = Target::open(home, dry_run)?;
  let plan = target.plan(read.items)?;
  let mut report = plan.report(dry_run);
  if dry_run {
    return Ok((summary, report));
  }

  let copies = target.add(plan)?;
  let mut progress = Progress::new("copying recordings", copies.len());
  for (src, dst) in &copies {
    report.count(super::copy_recording(dst, src), dst);
    progress.tick();
  }

  progress.finish();
  Ok((summary, report))
}

/// Handy's entries with text, each with its recording if that is on disk, and their size.
struct Read {
  items: Vec<Item<PathBuf>>,
  bytes: u64,
}

fn read(dir: &Path) -> Result<Read> {
  let path = dir.join(DB_FILE);
  if !path.is_file() {
    bail!("no Handy history in {}", dir.display());
  }

  let db = db::open_read_only(&path)?;
  let qs = "
  SELECT file_name, timestamp, transcription_text, post_processed_text
  FROM transcription_history ORDER BY timestamp, id;
  ";

  let mut stmt = db.prepare(qs).with_context(|| format!("read {}", path.display()))?;
  let rows = stmt.query_map([], |row| {
    let file: String = row.get(0)?;
    let at: i64 = row.get(1)?;
    let text: String = row.get(2)?;
    let processed: Option<String> = row.get(3)?;
    Ok((file, at, processed.filter(|t| !t.trim().is_empty()).unwrap_or(text)))
  })?;

  // One listing rather than a look-up per entry: a big history has tens of thousands.
  let recordings = dir.join("recordings");
  let sizes: HashMap<String, u64> = match fs::read_dir(&recordings) {
    Ok(list) => list
      .flatten()
      .filter_map(|e| Some((e.file_name().into_string().ok()?, e.metadata().ok()?)))
      .filter(|(_, meta)| meta.is_file())
      .map(|(name, meta)| (name, meta.len()))
      .collect(),
    Err(e) => {
      warn!("read {}: {e}", recordings.display());
      HashMap::new()
    }
  };

  let mut read = Read { items: Vec::new(), bytes: 0 };
  for row in rows {
    let (file, at, text) = row?;
    if text.trim().is_empty() {
      continue;
    }

    let size = sizes.get(&file).filter(|_| recordings::is_plain(&file));
    read.bytes += size.copied().unwrap_or(0);
    let entry = Entry {
      id: 0,
      created_at: at.saturating_mul(1000),
      duration_ms: size.map_or(0, |len| (len.saturating_sub(WAV_HEADER) / WAV_BYTES_PER_MS) as i64),
      audio_path: None,
      model: Some(MODEL.into()),
      language: None,
      text,
      pasted: false,
      not_pasted: None,
    };
    read.items.push(Item { entry, audio: size.map(|_| recordings.join(&file)) });
  }

  Ok(read)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::SystemTime;

  use rusqlite::Connection;

  use super::*;
  use crate::history::History;
  use crate::testing::{TempDir, entry_ids, tone, wav_count, write_wav};

  /// 2026-10-09 00:00 UTC, Unix seconds.
  const NOW: i64 = 1_791_504_000;

  /// A Handy data directory as Handy makes it: its four migrations, then `(seconds, text,
  /// post-processed text, recording seconds)` rows, `None` seconds for an entry whose recording
  /// is gone.
  fn handy(dir: &Path, rows: &[(i64, &str, Option<&str>, Option<f64>)]) {
    fs::create_dir_all(dir.join("recordings")).unwrap();
    let db = Connection::open(dir.join("history.db")).unwrap();
    for qs in [
      "CREATE TABLE IF NOT EXISTS transcription_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        file_name TEXT NOT NULL,
        timestamp INTEGER NOT NULL,
        saved BOOLEAN NOT NULL DEFAULT 0,
        title TEXT NOT NULL,
        transcription_text TEXT NOT NULL
      );",
      "ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;",
      "ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;",
      "ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL \
       DEFAULT 0;",
      "PRAGMA user_version = 4;",
    ] {
      db.execute_batch(qs).unwrap();
    }

    for &(at, text, processed, seconds) in rows {
      let file = format!("handy-{at}.wav");
      db.execute(
        "INSERT INTO transcription_history (file_name, timestamp, saved, title, \
         transcription_text, post_processed_text, post_process_requested) VALUES (?1, ?2, 0, ?3, \
         ?4, ?5, ?6)",
        rusqlite::params![
          file,
          at,
          format!("Recording {at}"),
          text,
          processed,
          processed.is_some()
        ],
      )
      .unwrap();
      if let Some(seconds) = seconds {
        write_wav(&dir.join("recordings").join(&file), &tone(seconds, 0.1));
      }
    }
  }

  /// Every file in `dir` with its size and modification time.
  fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut found = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
      let meta = e.metadata().unwrap();
      if meta.is_dir() {
        found.extend(snapshot(&e.path()));
      } else {
        found.push((e.path(), meta.len(), meta.modified().unwrap()));
      }
    }

    found.sort();
    found
  }

  fn rows() -> Vec<(i64, &'static str, Option<&'static str>, Option<f64>)> {
    vec![
      (NOW - 300, "first one", None, Some(1.5)),
      (NOW - 200, "raw text", Some("Processed text."), Some(0.5)),
      (NOW - 100, "", None, Some(0.5)), // nothing recognized: not imported
      (NOW, "no recording left", None, None),
    ]
  }

  #[test]
  fn imports_entries_and_recordings_and_leaves_handy_alone() {
    let (src, home) = (TempDir::new("handy-src"), TempDir::new("handy-home"));
    handy(src.path(), &rows());
    let before = snapshot(src.path());

    // A dry run counts and writes nothing.
    let (summary, dry) = import(home.path(), src.path(), true).unwrap();
    assert_eq!(dry, Report { entries: 3, added: 3, copied: 2, failed: 0 });
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!((summary.dir.as_path(), summary.entries, summary.recordings), (src.path(), 3, 2));
    assert_eq!((summary.first, summary.last), (Some((NOW - 300) * 1000), Some(NOW * 1000)));
    assert!(summary.bytes > 2 * 16_000 * 2, "{}", summary.bytes);

    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 3, added: 3, copied: 2, failed: 0 });
    assert_eq!(snapshot(src.path()), before, "Handy's files are as they were");

    let h = History::open(home.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    let texts: Vec<_> = entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["first one", "Processed text.", "no recording left"]);
    let first = &entries[0];
    assert_eq!((first.created_at, first.duration_ms), ((NOW - 300) * 1000, 1_500));
    assert_eq!(first.model.as_deref(), Some(MODEL));
    let wav = h.audio_file(first).unwrap();
    assert_eq!(
      fs::read(wav).unwrap(),
      fs::read(src.path().join(format!("recordings/handy-{}.wav", NOW - 300))).unwrap()
    );
    assert_eq!(entries[2].audio_path, None);
    assert_eq!(wav_count(&home.path().join(recordings::DIR)), 2);

    // Again: nothing new.
    let again = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(again, Report { entries: 3, ..Report::default() });
    assert_eq!(entry_ids(home.path()).len(), 3);
  }

  #[test]
  fn an_import_cut_short_is_finished_by_the_next() {
    let (src, home) = (TempDir::new("handy-resume-src"), TempDir::new("handy-resume-home"));
    handy(src.path(), &rows());
    import(home.path(), src.path(), false).unwrap();

    // As if it stopped after the entries: a recording missing, another half written.
    let h = History::open(home.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    let (a, b) = (h.audio_file(&entries[0]).unwrap(), h.audio_file(&entries[1]).unwrap());
    fs::remove_file(&a).unwrap();
    fs::rename(&b, super::super::part_of(&b)).unwrap();

    let report = import(home.path(), src.path(), true).unwrap().1;
    assert_eq!((report.added, report.copied), (0, 2), "a dry run sees it too");
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 3, added: 0, copied: 2, failed: 0 });
    assert!(a.is_file() && b.is_file() && !super::super::part_of(&b).exists());
    assert_eq!(entry_ids(home.path()).len(), 3);
  }

  #[test]
  fn refuses_while_wavo_runs() {
    let (src, home) = (TempDir::new("handy-running-src"), TempDir::new("handy-running-home"));
    handy(src.path(), &rows());
    let crate::ctl::Claim::Server(_app) = crate::ctl::claim(home.path()).unwrap() else {
      panic!("the home is taken")
    };
    let err = import(home.path(), src.path(), false).unwrap_err().to_string();
    assert!(err.contains("running"), "{err}");
    assert!(import(home.path(), src.path(), true).is_ok(), "a dry run takes no lock");
  }

  #[test]
  fn nothing_found_without_a_history() {
    let dir = TempDir::new("handy-none");
    assert!(read(dir.path()).err().unwrap().to_string().contains("no Handy history"));
    handy(dir.path(), &[]);
    let summary = Summary::new(dir.path(), &read(dir.path()).unwrap());
    assert_eq!((summary.entries, summary.first), (0, None));
  }
}
