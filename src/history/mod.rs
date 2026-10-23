// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation history: `history.db` (SQLite) in the data home and the recordings it points to,
//! `recordings/YYYY-MM-DD/<unix ms>.wav` (16 kHz mono 16-bit, the whole recording without VAD
//! trimming), in a folder per local date of the recording's start: one flat folder of a year's
//! dictations is more than Finder copes with.
//!
//! - It keeps what was transcribed (`done`, `partial`), what failed (`failed`: the audio is the
//!   only copy of the dictation, and it can still be transcribed) and speech no text came of
//!   (`no_text`, `no_model`: to try again). A cancelled dictation and one without speech leave
//!   nothing: no entry, no recording ([`History::discard_audio`]).
//! - The schema is migrated through `PRAGMA user_version`; a database from a newer version is
//!   refused rather than written in a layout this version does not know.
//! - [`History::cleanup`] applies the retention: entries and their audio files go together, the
//!   file first, so a crash in between leaves a row whose file is already gone, never a file
//!   nothing points to. Entries still being transcribed are never removed.
//! - [`History::recover`] runs on start: entries left `transcribing` by a crash become `failed`,
//!   and recordings without an entry (the app died while recording) get a `failed` entry, so their
//!   audio can be transcribed again and the retention covers them. A day folder goes with its last
//!   recording.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, mem};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const DB_FILE: &str = "history.db";
pub const AUDIO_DIR: &str = "recordings";

/// Errors of entries that [`History::recover`] fixes up.
pub const STOPPED_TRANSCRIBING: &str = "Wavo stopped during the transcription";
pub const STOPPED_RECORDING: &str =
  "Wavo stopped during the recording; the audio up to then is kept";

/// Schema versions, in order: `MIGRATIONS[i]` takes `user_version` `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
  // Ids are never used again (`AUTOINCREMENT`): an id the History tab or `wavo-app ctl` holds
  // must not come back as another dictation.
  "
  CREATE TABLE entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    audio_path TEXT,
    model TEXT,
    language TEXT,
    text TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    error TEXT,
    pasted INTEGER NOT NULL DEFAULT 0,
    not_pasted TEXT
  );
  CREATE INDEX entries_created_at ON entries (created_at);
  CREATE INDEX entries_audio_path ON entries (audio_path);
  ",
];

const COLUMNS: &str = "id, created_at, duration_ms, audio_path, model, language, text, status, \
  error, pasted, not_pasted";

/// How long history is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Retention {
  #[default]
  #[serde(rename = "unlimited")]
  Unlimited,
  /// The newest N entries.
  #[serde(rename = "count")]
  Count(u32),
  #[serde(rename = "3d")]
  Days3,
  #[serde(rename = "2w")]
  Weeks2,
  #[serde(rename = "3mo")]
  Months3,
}

impl Retention {
  const DAY: Duration = Duration::from_secs(24 * 60 * 60);

  fn max_age(self) -> Option<Duration> {
    match self {
      Retention::Days3 => Some(Self::DAY * 3),
      Retention::Weeks2 => Some(Self::DAY * 14),
      Retention::Months3 => Some(Self::DAY * 90),
      Retention::Unlimited | Retention::Count(_) => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
  Transcribing,
  Done,
  /// Text that the engine could only get in part (aborted, truncated or repeating decode).
  Partial,
  Failed,
  /// Speech (the VAD said so) of which the engine made no text: a model that does not know the
  /// language, most likely. Kept with its recording, to transcribe again with another model; the
  /// error says what is known (`parakeet-tdt-0.6b-v2 understands English only`).
  NoText,
  /// Speech recorded while no model could transcribe it (none downloaded, the chosen one gone
  /// from the disk, one that does not load). Kept with its recording, to transcribe once there is
  /// one; the error says why (`parakeet-tdt-0.6b-v3 is not on disk`).
  NoModel,
}

impl Status {
  const ALL: [Self; 6] =
    [Self::Transcribing, Self::Done, Self::Partial, Self::Failed, Self::NoText, Self::NoModel];

  pub fn as_str(self) -> &'static str {
    match self {
      Status::Transcribing => "transcribing",
      Status::Done => "done",
      Status::Partial => "partial",
      Status::Failed => "failed",
      Status::NoText => "no_text",
      Status::NoModel => "no_model",
    }
  }

  fn parse(s: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|status| status.as_str() == s)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
  pub id: i64,
  /// When the recording started, Unix time in ms.
  pub created_at: i64,
  pub duration_ms: i64,
  /// Path in the recordings directory: `YYYY-MM-DD/<unix ms>.wav`, or a file at its top.
  pub audio_path: Option<String>,
  /// Model file name without extension.
  pub model: Option<String>,
  /// Language asked for; `None` is detected by the model.
  pub language: Option<String>,
  pub text: String,
  pub status: Status,
  pub error: Option<String>,
  pub pasted: bool,
  /// Why a paste was refused (the focus moved, a password field…); `None` also without
  /// auto-paste.
  pub not_pasted: Option<String>,
}

impl Entry {
  fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
    let status: String = row.get(7)?;
    Ok(Self {
      id: row.get(0)?,
      created_at: row.get(1)?,
      duration_ms: row.get(2)?,
      audio_path: row.get(3)?,
      model: row.get(4)?,
      language: row.get(5)?,
      text: row.get(6)?,
      status: Status::parse(&status).unwrap_or(Status::Failed),
      error: row.get(8)?,
      pasted: row.get(9)?,
      not_pasted: row.get(10)?,
    })
  }
}

/// A new entry; the rest of [`Entry`] starts empty.
#[derive(Debug, Clone, Default)]
pub struct NewEntry<'a> {
  pub created_at: i64,
  pub duration_ms: i64,
  pub audio_path: Option<&'a str>,
  pub model: Option<&'a str>,
  pub language: Option<&'a str>,
  pub status: Option<Status>,
  pub error: Option<&'a str>,
}

pub struct History {
  conn: Connection,
  audio_dir: PathBuf,
}

impl History {
  /// Opens (creating, migrating) the history in `home`.
  pub fn open(home: &Path) -> Result<Self> {
    let audio_dir = home.join(AUDIO_DIR);
    fs::create_dir_all(&audio_dir).with_context(|| format!("create {}", audio_dir.display()))?;

    let path = home.join(DB_FILE);
    let mut conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&mut conn).with_context(|| format!("migrate {}", path.display()))?;
    Ok(Self { conn, audio_dir })
  }

  /// Where a new recording started at `created_at` (Unix ms) goes, in its day folder (created
  /// here): its path as the entry keeps it and its full path.
  pub fn new_audio(&self, created_at: i64) -> (String, PathBuf) {
    let day = day_of(created_at);
    if let Some(day) = &day {
      // A failure shows when the recording is created.
      let _ = fs::create_dir_all(self.audio_dir.join(day));
    }

    let mut n = 0;
    loop {
      let file = if n == 0 { format!("{created_at}.wav") } else { format!("{created_at}-{n}.wav") };
      let name = day.as_ref().map_or_else(|| file.clone(), |day| format!("{day}/{file}"));
      let path = self.audio_dir.join(&name);
      if !path.exists() {
        return (name, path);
      }

      n += 1;
    }
  }

  /// Deletes recording `name` that gets no entry (a cancelled dictation, one without speech).
  pub fn discard_audio(&self, name: &str) -> Result<()> {
    let Some(removed) = self.remove_recording(name) else { bail!("not a recording: {name:?}") };
    self.prune(&removed?);
    Ok(())
  }

  /// Recording `name` from [`new_audio`](Self::new_audio) was not made: its day folder goes if
  /// nothing else is in it.
  pub fn unused_audio(&self, name: &str) {
    if let Some(path) = self.audio_path(name) {
      self.prune(&path);
    }
  }

  /// The entry's recording, if it has one that is still on disk.
  pub fn audio_file(&self, entry: &Entry) -> Option<PathBuf> {
    let path = entry.audio_path.as_deref().and_then(|name| self.audio_path(name));
    path.filter(|p| p.is_file())
  }

  /// A path from the database in the recordings directory; anything but a plain file name, in a
  /// day folder or not, is refused, so a damaged row cannot point outside.
  fn audio_path(&self, name: &str) -> Option<PathBuf> {
    let plain = |name: &str| Path::new(name).file_name().is_some_and(|n| n == name);
    let inside = match name.split_once('/') {
      Some((day, file)) => is_day(day) && plain(file),
      None => plain(name),
    };
    inside.then(|| self.audio_dir.join(name))
  }

  /// Deletes recording `name` (one gone already is no error) and returns where it was. `None` if
  /// the name is refused.
  fn remove_recording(&self, name: &str) -> Option<Result<PathBuf>> {
    let path = self.audio_path(name)?;
    match fs::remove_file(&path) {
      Ok(()) => Some(Ok(path)),
      Err(e) if e.kind() == ErrorKind::NotFound => Some(Ok(path)),
      Err(e) => Some(Err(e).with_context(|| format!("delete {}", path.display()))),
    }
  }

  /// Removes the day folder of recording `path` if no recording is left in it.
  fn prune(&self, path: &Path) {
    match path.parent() {
      Some(dir) if dir != self.audio_dir => prune_day(dir),
      _ => {}
    }
  }

  pub fn insert(&self, entry: &NewEntry<'_>) -> Result<i64> {
    let status = entry.status.unwrap_or(Status::Transcribing);
    self
      .conn
      .prepare_cached(
        "INSERT INTO entries (created_at, duration_ms, audio_path, model, language, status, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
      )?
      .execute(params![
        entry.created_at,
        entry.duration_ms,
        entry.audio_path,
        entry.model,
        entry.language,
        status.as_str(),
        entry.error,
      ])?;
    Ok(self.conn.last_insert_rowid())
  }

  /// Marks an entry as being transcribed again, with `model` and `language`. Its text stays until
  /// the new result replaces it.
  pub fn retranscribing(&self, id: i64, model: &str, language: Option<&str>) -> Result<()> {
    self
      .conn
      .prepare_cached(
        "UPDATE entries SET status = 'transcribing', error = NULL, model = ?2, language = ?3
         WHERE id = ?1",
      )?
      .execute(params![id, model, language])?;
    Ok(())
  }

  /// Sets the outcome; `text` `None` keeps the text the entry has.
  pub fn finish(
    &self,
    id: i64,
    status: Status,
    text: Option<&str>,
    error: Option<&str>,
  ) -> Result<()> {
    self
      .conn
      .prepare_cached(
        "UPDATE entries SET status = ?2, text = COALESCE(?3, text), error = ?4 WHERE id = ?1",
      )?
      .execute(params![id, status.as_str(), text, error])?;
    Ok(())
  }

  /// Whether the text was pasted, and if a paste was refused, why.
  pub fn set_pasted(&self, id: i64, pasted: bool, not_pasted: Option<&str>) -> Result<()> {
    self
      .conn
      .prepare_cached("UPDATE entries SET pasted = ?2, not_pasted = ?3 WHERE id = ?1")?
      .execute(params![id, pasted, not_pasted])?;
    Ok(())
  }

  pub fn get(&self, id: i64) -> Result<Option<Entry>> {
    let sql = format!("SELECT {COLUMNS} FROM entries WHERE id = ?1");
    Ok(self.conn.prepare_cached(&sql)?.query_row([id], Entry::from_row).optional()?)
  }

  /// Deletes entry `id` and its recording, the file first: if it cannot be deleted, the entry
  /// stays. Returns false if there is no such entry.
  pub fn delete(&self, id: i64) -> Result<bool> {
    let Some(entry) = self.get(id)? else { return Ok(false) };
    let removed = entry.audio_path.as_deref().and_then(|a| self.remove_recording(a)).transpose()?;
    self.conn.prepare_cached("DELETE FROM entries WHERE id = ?1")?.execute([id])?;
    if let Some(path) = &removed {
      self.prune(path);
    }

    Ok(true)
  }

  /// Applies `retention` at `now` (Unix ms): removes older entries with their audio. An entry
  /// whose file cannot be deleted stays, to be tried again next time. Returns how many entries
  /// were removed.
  pub fn cleanup(&self, retention: Retention, now: i64) -> Result<usize> {
    let (sql, arg) = match (retention, retention.max_age()) {
      (Retention::Unlimited, _) => return Ok(0),
      (Retention::Count(n), _) => (
        "SELECT id, audio_path FROM entries WHERE status != 'transcribing'
         ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1",
        i64::from(n.max(1)),
      ),
      (_, Some(age)) => (
        "SELECT id, audio_path FROM entries WHERE status != 'transcribing' AND created_at < ?1",
        now - age.as_millis() as i64,
      ),
      (_, None) => unreachable!("every other retention has an age"),
    };
    let victims: Vec<(i64, Option<String>)> = self
      .conn
      .prepare_cached(sql)?
      .query_map([arg], |r| Ok((r.get(0)?, r.get(1)?)))?
      .collect::<rusqlite::Result<_>>()?;

    let mut removed = Vec::with_capacity(victims.len());
    let mut emptied = HashSet::new();
    for (id, audio) in victims {
      match audio.as_deref().and_then(|a| self.remove_recording(a)) {
        Some(Err(e)) => {
          warn!("could not {e:#}; its history entry stays");
          continue;
        }
        Some(Ok(path)) => {
          emptied.insert(path);
        }
        None => {}
      }

      removed.push(id);
    }

    if removed.is_empty() {
      return Ok(0);
    }

    let tx = self.conn.unchecked_transaction()?;
    {
      let mut delete = tx.prepare_cached("DELETE FROM entries WHERE id = ?1")?;
      for id in &removed {
        delete.execute([id])?;
      }
    }
    tx.commit()?;

    let days: HashSet<&Path> = emptied.iter().filter_map(|p| p.parent()).collect();
    for day in days.into_iter().filter(|&d| d != self.audio_dir) {
      prune_day(day);
    }

    Ok(removed.len())
  }

  /// Fixes up what a crash left behind (see the module docs); `now` stamps recordings whose start
  /// time is unknown.
  pub fn recover(&self, now: i64) -> Result<Recovered> {
    let failed = self.conn.execute(
      "UPDATE entries SET status = 'failed', error = ?1 WHERE status = 'transcribing'",
      [STOPPED_TRANSCRIBING],
    )?;

    let known: HashSet<String> = self
      .conn
      .prepare("SELECT audio_path FROM entries WHERE audio_path IS NOT NULL")?
      .query_map([], |r| r.get(0))?
      .collect::<rusqlite::Result<_>>()?;
    let mut orphans: Vec<String> =
      self.recordings()?.into_iter().filter(|name| !known.contains(name)).collect();
    orphans.sort();

    for name in &orphans {
      let file = name.rsplit('/').next().unwrap_or(name);
      let duration_ms = hound::WavReader::open(self.audio_dir.join(name))
        .map(|r| i64::from(r.duration()) * 1000 / i64::from(r.spec().sample_rate.max(1)))
        .unwrap_or(0);
      self.insert(&NewEntry {
        created_at: started_at(file).unwrap_or(now),
        duration_ms,
        audio_path: Some(name),
        status: Some(Status::Failed),
        error: Some(STOPPED_RECORDING),
        ..NewEntry::default()
      })?;
    }

    let recovered = Recovered { failed, adopted: orphans.len() };
    if recovered != Recovered::default() {
      info!(
        "history: {failed} interrupted transcription(s) marked failed, {} recording(s) without an \
         entry adopted",
        recovered.adopted
      );
    }

    Ok(recovered)
  }

  /// Every recording on disk, named as an entry names it; a day folder without one goes.
  fn recordings(&self) -> Result<Vec<String>> {
    let wavs = |dir: &Path| -> Result<Vec<String>> {
      Ok(
        fs::read_dir(dir)
          .with_context(|| format!("read {}", dir.display()))?
          .flatten()
          .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
          .filter_map(|e| e.file_name().into_string().ok())
          .filter(|name| name.ends_with(".wav"))
          .collect(),
      )
    };

    let mut found = Vec::new();
    for e in fs::read_dir(&self.audio_dir)
      .with_context(|| format!("read {}", self.audio_dir.display()))?
      .flatten()
    {
      let (Ok(name), Ok(kind)) = (e.file_name().into_string(), e.file_type()) else { continue };
      if kind.is_file() && name.ends_with(".wav") {
        found.push(name);
      } else if kind.is_dir() && is_day(&name) {
        let files = wavs(&e.path())?;
        if files.is_empty() {
          prune_day(&e.path());
        }

        found.extend(files.into_iter().map(|file| format!("{name}/{file}")));
      }
    }

    Ok(found)
  }
}

/// What [`History::recover`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
  /// Entries left `transcribing`, now `failed`.
  pub failed: usize,
  /// Recordings without an entry, now `failed` entries.
  pub adopted: usize,
}

/// Start time (Unix ms) in a recording's file name: `<unix ms>.wav` or `<unix ms>-<n>.wav`.
fn started_at(file: &str) -> Option<i64> {
  file.strip_suffix(".wav")?.split('-').next()?.parse().ok()
}

/// The day folder of a recording started at `ms` (Unix ms): its local date (the C runtime's time
/// zone), `YYYY-MM-DD`.
fn day_of(ms: i64) -> Option<String> {
  let time = ms.div_euclid(1000) as libc::time_t;
  // SAFETY: `tm` is plain data, and `localtime_r` writes only to it.
  let mut tm: libc::tm = unsafe { mem::zeroed() };
  #[cfg(unix)]
  if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
    return None;
  }

  // SAFETY: as above, Windows' C runtime's version.
  #[cfg(windows)]
  if unsafe { libc::localtime_s(&mut tm, &time) } != 0 {
    return None;
  }

  let year = tm.tm_year + 1900;
  (1970..=9999).contains(&year).then(|| format!("{year:04}-{:02}-{:02}", tm.tm_mon + 1, tm.tm_mday))
}

/// `YYYY-MM-DD`: the name of a day folder.
fn is_day(name: &str) -> bool {
  name.len() == 10
    && name
      .bytes()
      .enumerate()
      .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
}

/// Removes day folder `dir` if nothing but Finder's `.DS_Store` is in it.
fn prune_day(dir: &Path) {
  let Ok(entries) = fs::read_dir(dir) else { return };
  let mut ds_store = None;
  for e in entries.flatten() {
    if e.file_name() != ".DS_Store" {
      return;
    }

    ds_store = Some(e.path());
  }

  if let Some(file) = ds_store {
    let _ = fs::remove_file(file);
  }

  let _ = fs::remove_dir(dir);
}

fn migrate(conn: &mut Connection) -> Result<()> {
  let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
  let known = MIGRATIONS.len() as i64;
  if version > known {
    bail!("the database has schema {version}, newer than this Wavo knows ({known})");
  }

  for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
    let tx = conn.transaction()?;
    tx.execute_batch(sql)?;
    tx.pragma_update(None, "user_version", i as i64 + 1)?;
    tx.commit()?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::audio::wav::WavSink;
  use crate::testing::TempDir;

  const DAY: i64 = 24 * 60 * 60 * 1000;
  /// A fixed "now" for the fake clock: 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry with a real WAV file, `age` ms before [`NOW`].
  fn add(h: &History, age: i64, status: Status) -> (i64, PathBuf) {
    let (name, path) = h.new_audio(NOW - age);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&[0.1; 1_600]).unwrap();
    sink.finalize().unwrap();

    let id = h
      .insert(&NewEntry {
        created_at: NOW - age,
        duration_ms: 100,
        audio_path: Some(&name),
        status: Some(status),
        ..NewEntry::default()
      })
      .unwrap();
    (id, path)
  }

  /// Every entry, by id.
  fn entries(h: &History) -> Vec<Entry> {
    let sql = format!("SELECT {COLUMNS} FROM entries ORDER BY id");
    let mut stmt = h.conn.prepare(&sql).unwrap();
    stmt.query_map([], Entry::from_row).unwrap().map(Result::unwrap).collect()
  }

  fn ids(h: &History) -> Vec<i64> {
    entries(h).iter().map(|e| e.id).collect()
  }

  /// How many `.wav` files are in the recordings directory, at any depth.
  fn wavs(h: &History) -> usize {
    fn count(dir: &Path) -> usize {
      fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| match e.path() {
          path if path.is_dir() => count(&path),
          path => usize::from(path.extension().is_some_and(|x| x == "wav")),
        })
        .sum()
    }

    count(&h.audio_dir)
  }

  /// The files and folders at the top of the recordings directory.
  fn top(h: &History) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&h.audio_dir)
      .unwrap()
      .map(|e| e.unwrap().file_name().into_string().unwrap())
      .collect();
    names.sort();
    names
  }

  #[test]
  fn creates_and_migrates_once() {
    let dir = TempDir::new("history-migrate");
    let h = History::open(dir.path()).unwrap();
    let version: i64 = h.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, MIGRATIONS.len() as i64);
    let (id, _) = add(&h, 0, Status::Done);
    drop(h);

    // Reopening keeps the data and runs no migration again.
    let h = History::open(dir.path()).unwrap();
    assert_eq!(ids(&h), [id]);
    assert!(dir.path().join(AUDIO_DIR).is_dir());
  }

  #[test]
  fn ids_are_never_used_again() {
    let dir = TempDir::new("history-ids");
    let h = History::open(dir.path()).unwrap();
    let (first, _) = add(&h, 0, Status::Done);
    let (newest, _) = add(&h, 0, Status::Done);
    assert!(h.delete(newest).unwrap());
    let (next, _) = add(&h, 0, Status::Done);
    assert_eq!((first, newest, next), (1, 2, 3), "the deleted newest id is not used again");
  }

  #[test]
  fn refuses_a_newer_schema() {
    let dir = TempDir::new("history-newer");
    History::open(dir.path()).unwrap().conn.pragma_update(None, "user_version", 99).unwrap();
    let err = format!("{:#}", History::open(dir.path()).err().unwrap());
    assert!(err.contains("newer"), "{err}");
  }

  /// Speech nothing was recognized in stays, with its recording, through a restart's recovery.
  #[test]
  fn nothing_recognized_is_kept_with_its_recording() {
    let dir = TempDir::new("history-no-text");
    let h = History::open(dir.path()).unwrap();
    let (id, wav) = add(&h, 0, Status::Transcribing);
    let note = "parakeet-tdt-0.6b-v2 understands English only";
    h.finish(id, Status::NoText, None, Some(note)).unwrap();
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());

    let entry = h.get(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.text.as_str()), (Status::NoText, ""));
    assert_eq!((entry.error.as_deref(), h.audio_file(&entry)), (Some(note), Some(wav)));
    assert_eq!(Status::parse("no_text"), Some(Status::NoText));
  }

  /// Speech recorded without a model stays, with its recording, through a restart's recovery.
  #[test]
  fn no_model_is_kept_with_its_recording() {
    let dir = TempDir::new("history-no-model");
    let h = History::open(dir.path()).unwrap();
    let (id, wav) = add(&h, 0, Status::NoModel);
    let why = "parakeet-tdt-0.6b-v3 is not on disk";
    h.finish(id, Status::NoModel, None, Some(why)).unwrap();
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());

    let entry = h.get(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.error.as_deref()), (Status::NoModel, Some(why)));
    assert_eq!(h.audio_file(&entry), Some(wav));
    assert_eq!(Status::parse("no_model"), Some(Status::NoModel));
  }

  #[test]
  fn statuses_read_back_and_an_unknown_one_is_failed() {
    for status in Status::ALL {
      assert_eq!(Status::parse(status.as_str()), Some(status));
    }

    let dir = TempDir::new("history-status");
    let h = History::open(dir.path()).unwrap();
    let (id, _) = add(&h, 0, Status::Done);
    h.conn.execute("UPDATE entries SET status = 'cancelled' WHERE id = ?1", [id]).unwrap();
    assert_eq!(h.get(id).unwrap().unwrap().status, Status::Failed);
  }

  #[test]
  fn entry_lifecycle() {
    let dir = TempDir::new("history-entry");
    let h = History::open(dir.path()).unwrap();
    let (audio, path) = h.new_audio(NOW);
    let day = day_of(NOW).unwrap();
    assert_eq!(audio, format!("{day}/{NOW}.wav"));
    assert_eq!(path, dir.path().join(AUDIO_DIR).join(&day).join(format!("{NOW}.wav")));
    fs::write(&path, b"").unwrap();
    assert_eq!(h.new_audio(NOW).0, format!("{day}/{NOW}-1.wav"), "never overwrites a recording");

    let id = h
      .insert(&NewEntry {
        created_at: NOW,
        duration_ms: 4_200,
        audio_path: Some(&audio),
        model: Some("gigaam-v3-e2e-rnnt-Q8_0"),
        ..NewEntry::default()
      })
      .unwrap();
    let entry = h.get(id).unwrap().unwrap();
    assert_eq!(
      (entry.status, entry.text.as_str(), entry.pasted),
      (Status::Transcribing, "", false)
    );
    assert_eq!((entry.created_at, entry.duration_ms), (NOW, 4_200));
    assert_eq!(h.audio_file(&entry), Some(path));

    h.finish(id, Status::Done, Some("Привет"), None).unwrap();
    h.set_pasted(id, false, Some("the focus moved")).unwrap();
    assert_eq!(h.get(id).unwrap().unwrap().not_pasted.as_deref(), Some("the focus moved"));
    h.set_pasted(id, true, None).unwrap();

    // Again with another model: the text stays until the new result; a failure keeps it.
    h.retranscribing(id, "whisper", Some("ru")).unwrap();
    let entry = h.get(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.text.as_str()), (Status::Transcribing, "Привет"));
    assert_eq!((entry.model.as_deref(), entry.language.as_deref()), (Some("whisper"), Some("ru")));
    h.finish(id, Status::Failed, None, Some("the engine crashed")).unwrap();
    let entry = h.get(id).unwrap().unwrap();
    assert_eq!(entry.status, Status::Failed);
    assert_eq!(
      (entry.text.as_str(), entry.error.as_deref()),
      ("Привет", Some("the engine crashed"))
    );
    assert!(entry.pasted && entry.not_pasted.is_none());
    assert_eq!(h.get(id + 1).unwrap(), None);
  }

  #[test]
  fn audio_paths_stay_inside_the_recordings_directory() {
    let dir = TempDir::new("history-paths");
    let h = History::open(dir.path()).unwrap();
    for good in ["1.wav", "2026-10-09/1.wav", "2026-10-09/1-1.wav"] {
      assert_eq!(h.audio_path(good), Some(h.audio_dir.join(good)), "{good}");
    }

    for bad in [
      "../history.db",
      "/etc/passwd",
      "..",
      ".",
      "a/b.wav",
      "",
      "2026-10-09/../../history.db",
      "2026-10-09/..",
      "2026-10-09/",
      "2026-10-09/a/b.wav",
      "/2026-10-09/1.wav",
      "../2026-10-09/1.wav",
      "2026-1-09/1.wav",
      "2026-10-0x/1.wav",
      "2026/10/09/1.wav",
      "2026-10-09//1.wav",
    ] {
      assert_eq!(h.audio_path(bad), None, "{bad}");
    }
  }

  #[test]
  fn day_folders_are_local_dates() {
    // 2026-10-09 00:00 UTC is the 8th or the 9th in any time zone.
    let day = day_of(NOW).unwrap();
    assert!(["2026-10-08", "2026-10-09"].contains(&day.as_str()), "{day}");
    assert!(is_day(&day));
    assert_eq!(day_of(NOW + 1), Some(day));
    assert_eq!(day_of(i64::MAX), None);
    for bad in ["2026-10-9", "2026-10-090", "2026_10_09", "２０２６-10-09", "abcd-ef-gh"] {
      assert!(!is_day(bad), "{bad}");
    }

    assert_eq!(started_at("1791504000000.wav"), Some(NOW));
    assert_eq!(started_at("1791504000000-2.wav"), Some(NOW));
    assert_eq!((started_at("notes.wav"), started_at("1791504000000.txt")), (None, None));
  }

  #[test]
  fn a_day_folder_goes_with_its_last_recording() {
    let dir = TempDir::new("history-prune");
    let h = History::open(dir.path()).unwrap();
    // 7 minutes past a quarter hour: the same day in every time zone.
    let at = 7 * 60_000;
    let (a, a_wav) = add(&h, -at, Status::Done);
    let (b, b_wav) = add(&h, -at - 1, Status::Done);
    let day = a_wav.parent().unwrap().to_owned();
    assert_eq!(b_wav.parent(), Some(day.as_path()));
    fs::write(day.join(".DS_Store"), b"Finder").unwrap();
    assert!(h.delete(a).unwrap());
    assert!(day.is_dir(), "one recording is left");

    // A new recording of that day, its folder made; the day's last entry goes before the file is
    // created, and the recording still gets its folder.
    let (_, new_wav) = h.new_audio(NOW + at + 10);
    assert_eq!(new_wav.parent(), Some(day.as_path()));
    assert!(h.delete(b).unwrap());
    assert!(!day.exists(), "Finder's file does not keep it");
    WavSink::create(&new_wav).unwrap().finalize().unwrap();
    fs::remove_file(&new_wav).unwrap();
    fs::remove_dir(&day).unwrap();

    // A recording that was never made does not keep the folder made for it.
    let (name, path) = h.new_audio(NOW + at);
    assert!(path.parent().unwrap().is_dir());
    h.unused_audio(&name);
    assert!(!path.parent().unwrap().exists());

    // Retention and discard too; the recordings directory itself stays.
    for i in 0..3 {
      add(&h, 10 * DAY + i, Status::Done);
    }

    let (_, kept) = add(&h, 0, Status::Done);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 3);
    assert_eq!(top(&h), [kept.parent().unwrap().file_name().unwrap().to_str().unwrap()]);
    fs::remove_file(&kept).unwrap();
    let (name, path) = h.new_audio(NOW - 5 * DAY);
    fs::write(&path, b"RIFF").unwrap();
    h.discard_audio(&name).unwrap();
    assert!(!path.parent().unwrap().exists());

    // An empty day folder left by a crash goes on start.
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());
    assert!(top(&h).is_empty() && h.audio_dir.is_dir());
  }

  #[test]
  fn retention_by_count_keeps_the_newest() {
    let dir = TempDir::new("history-count");
    let h = History::open(dir.path()).unwrap();
    let entries: Vec<(i64, PathBuf)> =
      (0..5).map(|i| add(&h, (5 - i) * 1_000, Status::Done)).collect();
    // One still being transcribed is neither removed nor counted.
    let (busy, _) = add(&h, 10 * DAY, Status::Transcribing);
    assert_eq!(h.cleanup(Retention::Unlimited, NOW).unwrap(), 0);
    assert_eq!(h.cleanup(Retention::Count(2), NOW).unwrap(), 3);

    let kept = [entries[3].0, entries[4].0, busy];
    assert_eq!(ids(&h), kept);
    for (i, (_, path)) in entries.iter().enumerate() {
      assert_eq!(path.exists(), i >= 3, "{}", path.display());
    }

    assert_eq!(wavs(&h), 3);
    assert_eq!(h.cleanup(Retention::Count(2), NOW).unwrap(), 0, "idempotent");
    // Zero keeps one: the retention never empties the history entirely.
    assert_eq!(h.cleanup(Retention::Count(0), NOW).unwrap(), 1);
  }

  #[test]
  fn retention_by_age_with_a_fake_clock() {
    let dir = TempDir::new("history-age");
    let h = History::open(dir.path()).unwrap();
    let ages = [DAY, 3 * DAY - 1, 3 * DAY + 1, 13 * DAY, 15 * DAY, 89 * DAY, 91 * DAY];
    let entries: Vec<(i64, PathBuf)> = ages.iter().map(|&a| add(&h, a, Status::Done)).collect();
    let (busy, busy_wav) = add(&h, 400 * DAY, Status::Transcribing);
    let left = |h: &History| ids(h).len() - 1;

    assert_eq!(h.cleanup(Retention::Months3, NOW).unwrap(), 1);
    assert_eq!(left(&h), 6);
    assert!(!entries[6].1.exists());
    assert_eq!(h.cleanup(Retention::Weeks2, NOW).unwrap(), 2);
    assert_eq!(left(&h), 4);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 2);
    assert_eq!(ids(&h), [entries[0].0, entries[1].0, busy]);
    assert_eq!(wavs(&h), 3);
    assert!(entries[1].1.exists() && !entries[2].1.exists() && busy_wav.exists());

    // Time passes: a day later the 3-day-old entry goes too.
    assert_eq!(h.cleanup(Retention::Days3, NOW + DAY).unwrap(), 1);
    assert_eq!(ids(&h), [entries[0].0, busy]);
  }

  #[test]
  fn an_entry_whose_audio_is_already_gone_is_removed() {
    let dir = TempDir::new("history-gone");
    let h = History::open(dir.path()).unwrap();
    let (_, path) = add(&h, 10 * DAY, Status::Done);
    fs::remove_file(&path).unwrap();
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 1);
    assert!(ids(&h).is_empty());
  }

  #[test]
  fn recovers_interrupted_transcriptions_and_orphan_recordings() {
    let dir = TempDir::new("history-recover");
    let h = History::open(dir.path()).unwrap();
    let (done, _) = add(&h, DAY, Status::Done);
    let (stuck, _) = add(&h, 0, Status::Transcribing);

    // A recording the app was killed during: 1.5 s written, no entry. A killed app leaves the
    // header at the last whole second; a drop fixes it up to 1.5 s.
    let (name, path) = h.new_audio(NOW + 5);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&[0.2; 24_000]).unwrap();
    drop(sink);
    assert_eq!(h.recover(NOW + 10).unwrap(), Recovered { failed: 1, adopted: 1 });

    assert_eq!(h.get(done).unwrap().unwrap().status, Status::Done);
    let stuck = h.get(stuck).unwrap().unwrap();
    assert_eq!(
      (stuck.status, stuck.error.as_deref()),
      (Status::Failed, Some(STOPPED_TRANSCRIBING))
    );

    let adopted = entries(&h).pop().unwrap();
    assert_eq!(adopted.audio_path.as_deref(), Some(name.as_str()));
    assert_eq!((adopted.created_at, adopted.status), (NOW + 5, Status::Failed));
    assert_eq!(adopted.error.as_deref(), Some(STOPPED_RECORDING));
    assert!((1_000..=1_500).contains(&adopted.duration_ms), "{}", adopted.duration_ms);
    assert_eq!(h.recover(NOW + 20).unwrap(), Recovered::default(), "nothing left to fix");
  }

  #[test]
  fn a_recording_without_an_entry_is_discarded() {
    let dir = TempDir::new("history-discard");
    let h = History::open(dir.path()).unwrap();
    let (name, path) = h.new_audio(NOW);
    fs::write(&path, b"RIFF").unwrap();
    h.discard_audio(&name).unwrap();
    assert!(!path.exists());
    h.discard_audio(&name).unwrap(); // gone already
    assert!(h.discard_audio("../history.db").is_err());
    assert!(dir.path().join(DB_FILE).exists());
  }

  #[test]
  fn delete_takes_the_recording() {
    let dir = TempDir::new("history-delete");
    let h = History::open(dir.path()).unwrap();
    let (id, path) = add(&h, 0, Status::Done);
    let (other, other_path) = add(&h, 1_000, Status::Done);
    assert!(h.delete(id).unwrap());
    assert!(!path.exists() && other_path.exists());
    assert_eq!(h.get(id).unwrap(), None);
    assert_eq!(ids(&h), [other]);
    assert!(!h.delete(id).unwrap(), "already gone");

    // A recording that is gone already does not keep the entry.
    fs::remove_file(&other_path).unwrap();
    assert!(h.delete(other).unwrap());
    assert!(ids(&h).is_empty());
  }

  #[test]
  fn retention_values_in_json() {
    let json = |r: Retention| serde_json::to_string(&r).unwrap();
    assert_eq!(json(Retention::Unlimited), r#""unlimited""#);
    assert_eq!(json(Retention::Count(500)), r#"{"count":500}"#);
    assert_eq!(json(Retention::Days3), r#""3d""#);
    assert_eq!(json(Retention::Weeks2), r#""2w""#);
    assert_eq!(json(Retention::Months3), r#""3mo""#);
    assert_eq!(serde_json::from_str::<Retention>(r#""2w""#).unwrap(), Retention::Weeks2);
  }
}
