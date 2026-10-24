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
//! - The database keeps SQLite's rollback journal (Handy sets no WAL): opened read-only, nothing is
//!   written beside it.
//!
//! Handy keeps no model or duration per entry: an imported entry's text is from the model `handy`,
//! its duration is its recording's. A row without text (nothing recognized) comes with its
//! recording, as an entry without text no model has tried; one with neither is left.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::warn;

use super::{Item, Lock, Progress, Report, Target};
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
  /// Entries with text or a recording.
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

/// Handy's history on this computer ([`dir`]), if it has any entry. It reads Handy's database and
/// lists its recordings (a second or two for a big one): call it off the UI thread.
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
  let mut progress = None;
  let result = run(home, dir, Lock::new(dry_run), &mut |done, total| {
    let progress = progress.get_or_insert_with(|| Progress::new("copying recordings", total));
    if done > 0 {
      progress.tick();
    }
  });
  if let Some(progress) = progress {
    progress.finish();
  }

  result
}

/// [`import`] in the running app, which holds the single-instance lock: `copied` hears how many
/// of the recordings are copied, from 0, a few times a second and at the end.
pub fn import_held(
  home: &Path,
  dir: &Path,
  copied: &mut dyn FnMut(usize, usize),
) -> Result<(Summary, Report)> {
  let mut told: Option<Instant> = None;
  run(home, dir, Lock::Held, &mut |done, total| {
    if done == total || told.is_none_or(|t| t.elapsed() >= Progress::EVERY) {
      told = Some(Instant::now());
      copied(done, total);
    }
  })
}

fn run(
  home: &Path,
  dir: &Path,
  lock: Lock,
  copied: &mut dyn FnMut(usize, usize),
) -> Result<(Summary, Report)> {
  let read = read(dir)?;
  let summary = Summary::new(dir, &read);
  let mut target = Target::open(home, lock)?;
  let plan = target.plan(read.items);
  let mut report = plan.report(lock == Lock::DryRun);
  if lock == Lock::DryRun {
    return Ok((summary, report));
  }

  let copies = target.add(plan)?;
  copied(0, copies.len());
  for (i, (src, dst)) in copies.iter().enumerate() {
    report.count(super::copy_recording(dst, src), dst);
    copied(i + 1, copies.len());
  }

  Ok((summary, report))
}

/// Handy's entries with text or a recording on disk, each with that recording, and their size.
struct Read {
  items: Vec<Item<PathBuf>>,
  bytes: u64,
}

fn read(dir: &Path) -> Result<Read> {
  let path = dir.join(DB_FILE);
  if !path.is_file() {
    bail!("no Handy history in {}", dir.display());
  }

  let db = db::read_only(&path)?;
  // A Handy from before its post-processing (its second migration) has no such column.
  let processed: bool = db
    .query_row(
      "SELECT count(*) > 0 FROM pragma_table_info('transcription_history') \
       WHERE name = 'post_processed_text'",
      [],
      |row| row.get(0),
    )
    .with_context(|| format!("read {}", path.display()))?;
  let processed = if processed { "post_processed_text" } else { "NULL" };
  let qs = format!(
    "SELECT id, file_name, timestamp, transcription_text, {processed}
    FROM transcription_history ORDER BY timestamp, id;"
  );

  let mut stmt = db.prepare(&qs).with_context(|| format!("read {}", path.display()))?;
  let rows = stmt.query_map([], |row| {
    let id: i64 = row.get(0)?;
    let file: String = row.get(1)?;
    let at: i64 = row.get(2)?;
    let text: String = row.get(3)?;
    let processed: Option<String> = row.get(4)?;
    // Seconds to ms, the id as the ms: two dictations of one second start apart, the same each
    // time it is imported.
    let at = at.saturating_mul(1000).saturating_add(id.rem_euclid(1000));
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

  // Two dictations of one second name one file, which the later one wrote: it is that one's.
  let rows: Vec<(String, i64, String)> = rows.collect::<rusqlite::Result<_>>()?;
  let owner: HashMap<&str, usize> =
    rows.iter().enumerate().map(|(i, (file, _, _))| (file.as_str(), i)).collect();

  let mut read = Read { items: Vec::new(), bytes: 0 };
  for (i, (file, at, text)) in rows.iter().enumerate() {
    let own = owner.get(file.as_str()) == Some(&i) && recordings::is_plain(file);
    let size = sizes.get(file).filter(|_| own);
    let has_text = !text.trim().is_empty();
    if !has_text && size.is_none() {
      continue;
    }

    read.bytes += size.copied().unwrap_or(0);
    let entry = Entry {
      created_at: *at,
      duration_ms: size.map_or(0, |len| (len.saturating_sub(WAV_HEADER) / WAV_BYTES_PER_MS) as i64),
      text: if has_text { text.clone() } else { String::new() },
      model: has_text.then(|| MODEL.into()),
      ..Entry::default()
    };
    read.items.push(Item { entry, audio: size.map(|_| recordings.join(file)) });
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
      (NOW - 100, "", None, Some(0.5)), // nothing recognized: its recording comes
      (NOW - 50, " ", None, None),      // neither text nor a recording: left
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
    assert_eq!(dry, Report { entries: 4, added: 4, copied: 3, failed: 0 });
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!((summary.dir.as_path(), summary.entries, summary.recordings), (src.path(), 4, 3));
    // Ms from the row ids.
    assert_eq!((summary.first, summary.last), (Some((NOW - 300) * 1000 + 1), Some(NOW * 1000 + 5)));
    assert!(summary.bytes > 5 * 16_000 * 2 / 2, "{}", summary.bytes);

    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 4, added: 4, copied: 3, failed: 0 });
    assert_eq!(snapshot(src.path()), before, "Handy's files are as they were");

    let h = History::open(home.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    let texts: Vec<_> = entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["first one", "Processed text.", "", "no recording left"]);
    let first = &entries[0];
    assert_eq!((first.created_at, first.duration_ms), ((NOW - 300) * 1000 + 1, 1_500));
    assert_eq!(first.model.as_deref(), Some(MODEL));
    let wav = h.audio_file(first).unwrap();
    assert_eq!(
      fs::read(wav).unwrap(),
      fs::read(src.path().join(format!("recordings/handy-{}.wav", NOW - 300))).unwrap()
    );
    // Nothing recognized: its recording, for a model to try.
    let none = &entries[2];
    assert_eq!((none.model.as_deref(), none.tried_model.as_deref()), (None, None));
    assert_eq!(none.duration_ms, 500);
    assert!(h.audio_file(none).is_some());
    assert_eq!(entries[3].audio_path, None);
    assert_eq!(wav_count(&home.path().join(recordings::DIR)), 3);

    // Again: nothing new.
    let again = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(again, Report { entries: 4, ..Report::default() });
    assert_eq!(entry_ids(home.path()).len(), 4);
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
    assert_eq!(report, Report { entries: 4, added: 0, copied: 2, failed: 0 });
    assert!(a.is_file() && b.is_file() && !super::super::part_of(&b).exists());
    assert_eq!(entry_ids(home.path()).len(), 4);

    // Or before any of a day's recordings, and Wavo pruned the empty day folder since; its start
    // dropped the entry without text, whose recording is all there is of it: it comes again.
    fs::remove_dir_all(a.parent().unwrap()).unwrap();
    h.recover(crate::history::now_ms()).unwrap();
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!((report.added, report.copied, report.failed), (1, 3, 0));
    assert!(a.is_file() && b.is_file());
  }

  /// Handy names a recording by its second: of two dictations in one, the file is the last one's.
  #[test]
  fn two_dictations_of_one_second_are_two_entries() {
    let (src, home) = (TempDir::new("handy-second-src"), TempDir::new("handy-second-home"));
    handy(src.path(), &[(NOW, "one", None, Some(0.5)), (NOW, "two", None, None)]);
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 2, added: 2, copied: 1, failed: 0 });
    let entries = History::open(home.path()).unwrap().db.get_entries().unwrap();
    let audio: Vec<_> = entries.iter().map(|e| (e.text.as_str(), e.audio_path.is_some())).collect();
    assert_eq!(audio, [("one", false), ("two", true)], "the file is the later one's");
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 2, ..Report::default() });
  }

  /// An entry transcribed again in Wavo is still the one imported.
  #[test]
  fn an_entry_transcribed_again_is_not_imported_again() {
    let (src, home) = (TempDir::new("handy-again-src"), TempDir::new("handy-again-home"));
    handy(src.path(), &rows());
    import(home.path(), src.path(), false).unwrap();
    let h = History::open(home.path()).unwrap();
    let id = h.db.get_entries().unwrap()[0].id;
    h.db.update_text(id, "another text", "parakeet", None).unwrap();
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!(report, Report { entries: 4, ..Report::default() });
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

    // The running app, which holds the lock, imports, telling how the copies go.
    let mut told = Vec::new();
    let (_, report) =
      import_held(home.path(), src.path(), &mut |n, of| told.push((n, of))).unwrap();
    assert_eq!((report.added, report.copied), (4, 3));
    assert_eq!(told.first(), Some(&(0, 3)));
    assert_eq!(told.last(), Some(&(3, 3)));
  }

  /// A Handy from before its post-processing: its first migration only.
  #[test]
  fn reads_a_handy_without_post_processing() {
    let (src, home) = (TempDir::new("handy-old-src"), TempDir::new("handy-old-home"));
    fs::create_dir_all(src.path().join("recordings")).unwrap();
    let db = Connection::open(src.path().join("history.db")).unwrap();
    db.execute_batch(
      "CREATE TABLE transcription_history (id INTEGER PRIMARY KEY AUTOINCREMENT, file_name TEXT \
       NOT NULL, timestamp INTEGER NOT NULL, saved BOOLEAN NOT NULL DEFAULT 0, title TEXT NOT \
       NULL, transcription_text TEXT NOT NULL);
       INSERT INTO transcription_history (file_name, timestamp, title, transcription_text)
       VALUES ('handy-1.wav', 1, 'Recording', 'old text');",
    )
    .unwrap();
    let report = import(home.path(), src.path(), false).unwrap().1;
    assert_eq!((report.entries, report.added), (1, 1));
  }

  /// One import at a time into a history: the app's second click waits for the first.
  #[test]
  fn one_import_at_a_time() {
    let (src, home) = (TempDir::new("handy-twice-src"), TempDir::new("handy-twice-home"));
    handy(src.path(), &rows());
    let running = super::super::Running::start(home.path()).unwrap();
    let err = import_held(home.path(), src.path(), &mut |_, _| {}).unwrap_err().to_string();
    assert!(err.contains("running already"), "{err}");
    drop(running);
    assert!(import_held(home.path(), src.path(), &mut |_, _| {}).is_ok());
    let busy = History::open(home.path()).unwrap().db.get_entries().unwrap();
    assert!(busy.iter().all(|e| !e.busy), "not busy once it ended");
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
