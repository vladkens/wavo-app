// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation history in the data home: the entries in `history.db` and the recordings they point
//! to, `recordings/YYYY-MM-DD/<unix ms>.wav` (16 kHz mono 16-bit, the whole recording).
//!
//! - An entry is a recording with speech; its text may be empty (no model yet, nothing recognized):
//!   text can always be made again from the audio.
//! - Entries and their recordings go together, the file first: a crash in between leaves a row
//!   whose file is gone, never a file nothing points to. Entries being transcribed (`busy`) are
//!   never removed.
//! - [`History::recover`] runs on start: entries a crash left busy are not any more, entries
//!   without text whose recording is gone go, and recordings without an entry (the app died while
//!   recording) become entries without text if they have speech, else they go.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, mem};

use anyhow::{Context, Result, bail};
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::audio::{Clip, read_wav};

pub const DB_FILE: &str = "history.db";
pub const RECORDINGS_DIR: &str = "recordings";

// MARK: Time and text

/// Now as the history keeps time: Unix ms.
pub fn now_ms() -> i64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A moment in local time (the C runtime's time zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Local {
  pub year: i32,
  pub month: u32,
  pub day: u32,
  pub hour: u32,
  pub minute: u32,
}

const MONTHS: [&str; 12] = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
];

impl Local {
  pub fn date(self) -> (i32, u32, u32) {
    (self.year, self.month, self.day)
  }

  /// Empty for the month 0 of a date out of range (an imported entry's).
  pub fn month_name(self) -> &'static str {
    MONTHS.get((self.month as usize).wrapping_sub(1)).copied().unwrap_or_default()
  }

  /// `09:05`.
  pub fn hm(self) -> String {
    format!("{:02}:{:02}", self.hour, self.minute)
  }
}

/// Unix ms `ms` in local time; `None` outside the years 1970 to 9999.
pub fn local_time(ms: i64) -> Option<Local> {
  let time = ms.div_euclid(1000) as libc::time_t;
  // SAFETY: `tm` is plain data, and `localtime_r` writes only to it.
  let mut tm: libc::tm = unsafe { mem::zeroed() };
  #[cfg(unix)]
  if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
    return None;
  }

  // SAFETY: as above, Windows' C runtime's own.
  #[cfg(windows)]
  if unsafe { libc::localtime_s(&mut tm, &time) } != 0 {
    return None;
  }

  let year = tm.tm_year + 1900;
  (1970..=9999).contains(&year).then(|| Local {
    year,
    month: (tm.tm_mon + 1) as u32,
    day: tm.tm_mday as u32,
    hour: tm.tm_hour as u32,
    minute: tm.tm_min as u32,
  })
}

/// A dictation's text on one line, its first words up to `max` characters, `…` where it is cut
/// (a word longer than that cut inside).
pub fn shorten(text: &str, max: usize) -> String {
  let mut start = String::new();
  for (i, w) in text.split_whitespace().enumerate() {
    let len = start.chars().count() + usize::from(i > 0) + w.chars().count();
    if len > max {
      if start.is_empty() {
        start = w.chars().take(max).collect();
      }

      let start = start.trim_end_matches([',', ';', ':', '.', '—', '-']);
      return format!("{start}…");
    }

    if i > 0 {
      start.push(' ');
    }

    start.push_str(w);
  }

  start
}

/// What a search of the history compares: lower case in every script, `ё` as `е`.
pub fn fold(text: &str) -> String {
  text.to_lowercase().replace('ё', "е")
}

// MARK: Migrations

fn migrate_v1(db: &Connection) -> Result<()> {
  let queries = vec![
    // Ids are never used again (`AUTOINCREMENT`): an id the History tab holds
    // must not come back as another dictation.
    "CREATE TABLE entries (
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
    );",
    "CREATE INDEX entries_created_at ON entries (created_at);",
    "CREATE INDEX entries_audio_path ON entries (audio_path);",
    "CREATE INDEX entries_untranscribed ON entries (created_at) WHERE text = '';",
  ];

  for qs in queries {
    db.execute(qs, [])?;
  }

  Ok(())
}

/// Runs the migrations the database has not had; one from a newer Wavo is refused rather than
/// written in a layout this one does not know.
fn migrate(db: &Connection) -> Result<()> {
  let migrations: Vec<fn(&Connection) -> Result<()>> = vec![migrate_v1];
  let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
  if version > migrations.len() as i64 {
    bail!("the database has schema {version}, newer than this Wavo knows ({})", migrations.len());
  }

  for (i, migration) in migrations.iter().enumerate().skip(version as usize) {
    let tx = db.unchecked_transaction()?;
    migration(&tx)?;
    tx.pragma_update(None, "user_version", i as i64 + 1)?;
    tx.commit()?;
  }

  Ok(())
}

// MARK: Models

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
  pub id: i64,
  /// When the recording started, Unix ms.
  pub created_at: i64,
  pub duration_ms: i64,
  /// The recording, as [Recordings] names it.
  pub audio_path: Option<String>,
  /// Empty until a transcription makes some.
  pub text: String,
  /// The model the text came from: its file name without extension.
  pub model: Option<String>,
  /// The language the text was asked in; `None`: detected by the model.
  pub language: Option<String>,
  /// The model that last transcribed the recording, whatever came of it: an entry without text is
  /// transcribed again by itself only by another one.
  pub tried_model: Option<String>,
  /// Being transcribed: the retention leaves it alone. Cleared on start (a crash left it).
  pub busy: bool,
  pub pasted: bool,
  /// Why a paste was refused (the focus moved, a password field…).
  pub not_pasted: Option<String>,
}

impl Entry {
  fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
    Ok(Self {
      id: row.get("id")?,
      created_at: row.get("created_at")?,
      duration_ms: row.get("duration_ms")?,
      audio_path: row.get("audio_path")?,
      text: row.get("text")?,
      model: row.get("model")?,
      language: row.get("language")?,
      tried_model: row.get("tried_model")?,
      busy: row.get("busy")?,
      pasted: row.get("pasted")?,
      not_pasted: row.get("not_pasted")?,
    })
  }
}

// MARK: Db

pub struct Db {
  db: Connection,
}

impl Db {
  /// Opens (creating, migrating) the database at `path`.
  pub fn open(path: &Path) -> Result<Self> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    // The session, the history window and an export use it at once.
    db.busy_timeout(Duration::from_secs(2))?;
    db.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&db).with_context(|| format!("migrate {}", path.display()))?;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    db.create_scalar_function("wavo_fold", 1, flags, |ctx| Ok(fold(&ctx.get::<String>(0)?)))?;
    Ok(Self { db })
  }

  /// Moves on every row this connection writes and on every commit of another one: a history
  /// window reloads when it does.
  pub fn changes(&self) -> u64 {
    let others: i64 = self.db.query_row("PRAGMA data_version", [], |r| r.get(0)).unwrap_or(0);
    self.db.total_changes() + others as u64
  }

  fn get_all(&self, qs: &str, params: impl rusqlite::Params) -> Result<Vec<Entry>> {
    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map(params, Entry::from_row)?;
    Ok(items.collect::<rusqlite::Result<_>>()?)
  }

  fn update_all<T: rusqlite::ToSql>(&self, qs: &str, items: &[T]) -> Result<()> {
    let tx = self.db.unchecked_transaction()?;
    for x in items {
      tx.prepare_cached(qs)?.execute([x])?;
    }

    tx.commit()?;
    Ok(())
  }

  // MARK: Getters

  pub fn get_entry(&self, id: i64) -> Result<Option<Entry>> {
    let qs = "SELECT * FROM entries WHERE id = ?1;";
    Ok(self.db.prepare_cached(qs)?.query_row([id], Entry::from_row).optional()?)
  }

  /// Every entry, oldest first.
  pub fn get_entries(&self) -> Result<Vec<Entry>> {
    self.get_all("SELECT * FROM entries ORDER BY created_at, id;", [])
  }

  /// The newest `limit` entries whose text holds `search` in any case ([fold]; empty: all).
  pub fn get_page(&self, search: &str, limit: usize) -> Result<Vec<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE ?1 = '' OR instr(wavo_fold(text), ?1) > 0
    ORDER BY created_at DESC, id DESC LIMIT ?2;
    ";

    self.get_all(qs, params![fold(search), limit as i64])
  }

  /// Every entry: their count, their duration in ms and the first one's start (Unix ms).
  pub fn get_total(&self) -> Result<(usize, i64, Option<i64>)> {
    let qs = "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0), MIN(created_at) FROM entries;";
    let total = |r: &Row| Ok((r.get::<_, i64>(0)? as usize, r.get(1)?, r.get(2)?));
    Ok(self.db.prepare_cached(qs)?.query_row([], total)?)
  }

  /// The newest entry without text that `model` has not tried and nothing transcribes now, in one
  /// of the model's `langs` (any for `None`: a file of the person's own). One without a language
  /// (detected) goes to a model of more than one only.
  pub fn get_untried(&self, model: &str, langs: Option<&[&str]>) -> Result<Option<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE text = '' AND busy = 0 AND audio_path IS NOT NULL
      AND (tried_model IS NULL OR tried_model != ?1)
      AND (?2 IS NULL
        OR lower(language) IN (SELECT value FROM json_each(?2))
        OR (language IS NULL AND json_array_length(?2) > 1))
    ORDER BY created_at DESC, id DESC LIMIT 1;
    ";

    let langs = langs.map(serde_json::to_string).transpose()?;
    Ok(self.db.prepare_cached(qs)?.query_row(params![model, langs], Entry::from_row).optional()?)
  }

  /// The entries after the newest `keep`; those being transcribed, and those without text when
  /// `spare`, are neither returned nor counted.
  pub fn get_beyond_newest(&self, keep: u32, spare: bool) -> Result<Vec<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE busy = 0 AND (text != '' OR ?2 = 0)
    ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1;
    ";

    self.get_all(qs, params![keep, spare])
  }

  /// The entries recorded before `at` (Unix ms), but those being transcribed, and those without
  /// text when `spare`.
  pub fn get_created_before(&self, at: i64, spare: bool) -> Result<Vec<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE busy = 0 AND (text != '' OR ?2 = 0) AND created_at < ?1;
    ";

    self.get_all(qs, params![at, spare])
  }

  // MARK: Inserters

  /// Adds `entries` (their ids are not used) in one transaction; returns their ids.
  pub fn insert(&self, entries: &[Entry]) -> Result<Vec<i64>> {
    let qs = "
    INSERT INTO entries (
      created_at, duration_ms, audio_path, text, model, language, tried_model, busy, pasted,
      not_pasted
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10);
    ";

    let tx = self.db.unchecked_transaction()?;
    let mut ids = Vec::with_capacity(entries.len());
    for e in entries {
      tx.prepare_cached(qs)?.execute(params![
        e.created_at,
        e.duration_ms,
        e.audio_path,
        e.text,
        e.model,
        e.language,
        e.tried_model,
        e.busy,
        e.pasted,
        e.not_pasted,
      ])?;
      ids.push(tx.last_insert_rowid());
    }

    tx.commit()?;
    Ok(ids)
  }

  // MARK: Updater

  /// Entry `id` has `text`, from `model` in `language`. (Schema 1's `prev_*` columns, an undo
  /// of old, are not used.)
  pub fn update_text(
    &self,
    id: i64,
    text: &str,
    model: Option<&str>,
    language: Option<&str>,
  ) -> Result<()> {
    let qs = "UPDATE entries SET text = ?2, model = ?3, language = ?4, busy = 0 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, text, model, language])?;
    Ok(())
  }

  /// The entries naming one of `names` name no recording.
  pub fn update_without_audio(&self, names: &[String]) -> Result<()> {
    self.update_all("UPDATE entries SET audio_path = NULL WHERE audio_path = ?1;", names)
  }

  /// Entry `id` is being transcribed by `model`.
  pub fn mark_busy(&self, id: i64, model: &str) -> Result<()> {
    let qs = "UPDATE entries SET busy = 1, tried_model = ?2 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, model])?;
    Ok(())
  }

  /// The transcription of entry `id` ended without text: it keeps what it had.
  pub fn mark_idle(&self, id: i64) -> Result<()> {
    self.db.prepare_cached("UPDATE entries SET busy = 0 WHERE id = ?1;")?.execute([id])?;
    Ok(())
  }

  /// A run whose model did not even load did not try entry `id`.
  pub fn mark_untried(&self, id: i64) -> Result<()> {
    let qs = "UPDATE entries SET busy = 0, tried_model = NULL WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute([id])?;
    Ok(())
  }

  pub fn mark_pasted(&self, id: i64, pasted: bool, not_pasted: Option<&str>) -> Result<()> {
    let qs = "UPDATE entries SET pasted = ?2, not_pasted = ?3 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, pasted, not_pasted])?;
    Ok(())
  }

  /// Entries a crash left busy are not any more; returns how many.
  pub fn mark_interrupted(&self) -> Result<usize> {
    Ok(self.db.execute("UPDATE entries SET busy = 0 WHERE busy = 1;", [])?)
  }

  /// Entries an import added, once their recordings are in.
  pub fn mark_idle_entries(&self, ids: &[i64]) -> Result<()> {
    self.update_all("UPDATE entries SET busy = 0 WHERE id = ?1;", ids)
  }

  pub fn delete_entries(&self, ids: &[i64]) -> Result<()> {
    self.update_all("DELETE FROM entries WHERE id = ?1;", ids)
  }
}

// MARK: Recordings

/// A folder per local day of the recordings' start: a flat folder of a year's dictations is more
/// than Finder copes with. A day folder goes with its last recording.
pub struct Recordings {
  dir: PathBuf,
}

impl Recordings {
  pub fn open(home: &Path) -> Result<Self> {
    let dir = home.join(RECORDINGS_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(Self { dir })
  }

  /// Where a new recording started at `at` (Unix ms) goes: its name as entries keep it, and its
  /// path. Never an existing file.
  pub fn new_file(&self, at: i64) -> (String, PathBuf) {
    self.new_file_besides(at, &HashSet::new())
  }

  /// [new_file](Self::new_file), not named `taken` either: names entries hold before their files
  /// are there (an import).
  pub fn new_file_besides(&self, at: i64, taken: &HashSet<String>) -> (String, PathBuf) {
    let day = day_of(at);
    for n in 0.. {
      let file = if n == 0 { format!("{at}.wav") } else { format!("{at}-{n}.wav") };
      let name = day.as_ref().map_or_else(|| file.clone(), |day| format!("{day}/{file}"));
      let path = self.dir.join(&name);
      if !taken.contains(&name) && !path.exists() {
        return (name, path);
      }
    }

    unreachable!()
  }

  /// Recording `name`'s path: `[YYYY-MM-DD/]<file>`, nothing that could point outside, so a damaged
  /// row cannot.
  pub fn path(&self, name: &str) -> Option<PathBuf> {
    let plain = |file: &str| Path::new(file).file_name().is_some_and(|n| n == file);
    let ok = match name.split_once('/') {
      Some((day, file)) => is_day(day) && plain(file),
      None => plain(name),
    };

    ok.then(|| self.dir.join(name))
  }

  /// Deletes recording `name` (one gone already is no error), and its day folder if that is left
  /// empty.
  pub fn remove(&self, name: &str) -> Result<()> {
    let Some(path) = self.path(name) else { return Ok(()) };
    match fs::remove_file(&path) {
      Err(e) if e.kind() != ErrorKind::NotFound => {
        return Err(e).with_context(|| format!("delete {}", path.display()));
      }
      _ => {}
    }

    let dir = path.parent().unwrap();
    if dir != self.dir {
      // Finder's `.DS_Store` does not keep it.
      let _ = fs::remove_file(dir.join(".DS_Store"));
      let _ = fs::remove_dir(dir);
    }

    Ok(())
  }

  /// Every recording, named as entries name them.
  pub fn list(&self) -> Result<Vec<String>> {
    let read = |dir: &Path| fs::read_dir(dir).with_context(|| format!("read {}", dir.display()));
    let mut found = Vec::new();
    for e in read(&self.dir)?.flatten() {
      let (Ok(name), Ok(kind)) = (e.file_name().into_string(), e.file_type()) else { continue };
      if kind.is_file() && name.ends_with(".wav") {
        found.push(name);
      } else if kind.is_dir() && is_day(&name) {
        for f in read(&e.path())?.flatten() {
          let file = f.file_name().into_string().unwrap_or_default();
          if file.ends_with(".wav") && f.file_type().is_ok_and(|t| t.is_file()) {
            found.push(format!("{name}/{file}"));
          }
        }
      }
    }

    Ok(found)
  }
}

/// The day folder of a recording started at `ms` (Unix ms): its local date, `YYYY-MM-DD`.
pub fn day_of(ms: i64) -> Option<String> {
  let (year, month, day) = local_time(ms)?.date();
  Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn is_day(name: &str) -> bool {
  let digit = |(i, b): (usize, u8)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() };
  name.len() == 10 && name.bytes().enumerate().all(digit)
}

/// The start time (Unix ms) in recording `name`: `[YYYY-MM-DD/]<unix ms>[-<n>].wav`.
fn started_at(name: &str) -> Option<i64> {
  let file = name.rsplit('/').next()?;
  file.strip_suffix(".wav")?.split('-').next()?.parse().ok()
}

// MARK: History

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

pub struct History {
  pub db: Db,
  pub recordings: Recordings,
}

impl History {
  pub fn open(home: &Path) -> Result<Self> {
    let recordings = Recordings::open(home)?;
    let db = Db::open(&home.join(DB_FILE))?;
    Ok(Self { db, recordings })
  }

  /// The entry's recording, if it has one still on disk.
  pub fn audio_file(&self, entry: &Entry) -> Option<PathBuf> {
    let name = entry.audio_path.as_deref()?;
    self.recordings.path(name).filter(|p| p.is_file())
  }

  /// Deletes entry `id` and its recording, the file first: if it cannot be deleted, the entry
  /// stays. False if there is no such entry.
  pub fn delete(&self, id: i64) -> Result<bool> {
    let Some(entry) = self.db.get_entry(id)? else { return Ok(false) };
    if let Some(name) = &entry.audio_path {
      self.recordings.remove(name)?;
    }

    self.db.delete_entries(&[id])?;
    Ok(true)
  }

  /// Applies `retention` at `now` (Unix ms); entries without text stay when `spare`, and do not
  /// count: their audio is all there is of them. Returns how many entries were removed.
  pub fn cleanup(&self, retention: Retention, now: i64, spare: bool) -> Result<usize> {
    let days = |n: i64| now - n * 24 * 60 * 60 * 1000;
    let doomed = match retention {
      Retention::Unlimited => return Ok(0),
      Retention::Count(n) => self.db.get_beyond_newest(n.max(1), spare)?,
      Retention::Days3 => self.db.get_created_before(days(3), spare)?,
      Retention::Weeks2 => self.db.get_created_before(days(14), spare)?,
      Retention::Months3 => self.db.get_created_before(days(90), spare)?,
    };

    let mut removed = Vec::new();
    for e in doomed {
      // An entry whose file cannot be deleted stays, to be tried again next time.
      match e.audio_path.as_deref().map(|name| self.recordings.remove(name)) {
        Some(Err(err)) => warn!("{err:#}; its history entry stays"),
        _ => removed.push(e.id),
      }
    }

    self.db.delete_entries(&removed)?;
    Ok(removed.len())
  }

  /// Fixes up what a crash left behind (see the module docs); `now` stamps recordings whose start
  /// is unknown.
  pub fn recover(&self, now: i64) -> Result<()> {
    let interrupted = self.db.mark_interrupted()?;
    let entries = self.db.get_entries()?;
    let gone: Vec<i64> = (entries.iter())
      .filter(|e| e.text.is_empty() && self.audio_file(e).is_none())
      .map(|e| e.id)
      .collect();
    self.db.delete_entries(&gone)?;

    let known: HashSet<&str> = entries.iter().filter_map(|e| e.audio_path.as_deref()).collect();
    let mut orphans = self.recordings.list()?;
    orphans.retain(|name| !known.contains(name.as_str()));
    orphans.sort();

    let mut adopted = Vec::new();
    for name in &orphans {
      let clip = read_wav(&self.recordings.dir.join(name)).map(Clip::analyze);
      match clip {
        Ok(clip) if clip.has_speech() => adopted.push(Entry {
          created_at: started_at(name).unwrap_or(now),
          duration_ms: clip.duration_ms(),
          audio_path: Some(name.clone()),
          ..Entry::default()
        }),
        _ => self.recordings.remove(name).unwrap_or_else(|e| warn!("{e:#}")),
      }
    }

    self.db.insert(&adopted)?;
    if interrupted + gone.len() + orphans.len() > 0 {
      let (n, gone, adopted) = (orphans.len(), gone.len(), adopted.len());
      info!("history recovered: {interrupted} interrupted, {gone} gone, {adopted} of {n} adopted");
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::audio::WavSink;
  use crate::testing::{TempDir, entry_ids, tone, wav_count, write_wav};

  const DAY: i64 = 24 * 60 * 60 * 1000;
  /// 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry with `text` and a recording, `age` ms before [NOW].
  fn add(h: &History, age: i64, text: &str, busy: bool) -> (i64, PathBuf) {
    let (name, path) = h.recordings.new_file(NOW - age);
    write_wav(&path, &[0.1; 1_600]);
    let entry = Entry {
      created_at: NOW - age,
      audio_path: Some(name),
      text: text.into(),
      busy,
      ..Entry::default()
    };
    (h.db.insert(&[entry]).unwrap()[0], path)
  }

  #[test]
  fn transcribed_and_tried() {
    let dir = TempDir::new("history-text");
    let h = History::open(dir.path()).unwrap();
    let (id, _) = add(&h, 0, "", true);
    h.db.mark_busy(id, "gigaam").unwrap();
    h.db.update_text(id, "Привет", Some("gigaam"), Some("ru")).unwrap();
    h.db.mark_busy(id, "whisper").unwrap();
    h.db.mark_idle(id).unwrap();
    let e = h.db.get_entry(id).unwrap().unwrap();
    let got = (e.text.as_str(), e.model.as_deref(), e.tried_model.as_deref(), e.busy);
    assert_eq!(got, ("Привет", Some("gigaam"), Some("whisper"), false));
  }

  /// An entry without text goes to a model of its language; one without a language to a model of
  /// more than one; a file of the person's own takes any.
  #[test]
  fn entries_without_text_go_to_a_model_of_their_language() {
    let dir = TempDir::new("history-untried");
    let h = History::open(dir.path()).unwrap();
    let ids: Vec<i64> = (1..)
      .zip([Some("ru"), Some("EN"), None])
      .map(|(age, language)| {
        let (name, path) = h.recordings.new_file(NOW - age);
        write_wav(&path, &[0.1; 1_600]);
        let language = language.map(str::to_owned);
        let entry =
          Entry { created_at: NOW - age, audio_path: Some(name), language, ..Entry::default() };
        h.db.insert(&[entry]).unwrap()[0]
      })
      .collect();

    let untried =
      |model: &str, langs: Option<&[&str]>| h.db.get_untried(model, langs).unwrap().map(|e| e.id);

    let (ru, en, multi) = (&["ru"][..], &["en"][..], &["en", "ru"][..]);
    assert_eq!(untried("gigaam", Some(ru)), Some(ids[0]));
    assert_eq!(untried("parakeet-v2", Some(en)), Some(ids[1]), "in any case");
    assert_eq!(untried("parakeet-he", Some(&["he"][..])), None, "nor one detected");
    assert_eq!(untried("parakeet-v3", Some(multi)), Some(ids[0]), "the newest");
    assert_eq!(untried("own", None), Some(ids[0]));

    for id in &ids[..2] {
      h.db.mark_busy(*id, "parakeet-v3").unwrap();
      h.db.mark_idle(*id).unwrap();
    }

    assert_eq!(untried("parakeet-v3", Some(multi)), Some(ids[2]), "detected: a model of more");
    assert_eq!(untried("gigaam", Some(ru)), Some(ids[0]), "tried by another one");
  }

  #[test]
  fn pages_are_newest_first_and_searched_in_any_case() {
    let dir = TempDir::new("history-page");
    let h = History::open(dir.path()).unwrap();
    let texts = ["Добрый день.", "Ещё раз", "hello World", "ДОБРЫЙ вечер", "", "еще одно"];
    let ids: Vec<i64> = (0..).zip(texts).map(|(i, t)| add(&h, i * 1_000, t, false).0).collect();
    let page = |search: &str, limit| -> Vec<i64> {
      h.db.get_page(search, limit).unwrap().iter().map(|e| e.id).collect()
    };

    assert_eq!(page("", 100), ids);
    assert_eq!(page("", 2), ids[..2]);
    assert_eq!(page("добрый", 100), [ids[0], ids[3]]);
    assert_eq!(page("WORLD", 100), [ids[2]]);
    assert_eq!(page("ещё", 100), [ids[1], ids[5]]);
    assert!(page("%", 100).is_empty());
  }

  #[test]
  fn retention_by_count_and_age() {
    let dir = TempDir::new("history-retention");
    let h = History::open(dir.path()).unwrap();
    let ages = [DAY, 3 * DAY - 1, 3 * DAY + 1, 15 * DAY, 91 * DAY];
    let entries: Vec<(i64, PathBuf)> = ages.iter().map(|&a| add(&h, a, "text", false)).collect();
    let (busy, _) = add(&h, 400 * DAY, "", true);
    add(&h, 400 * DAY, "", false);

    assert_eq!(h.cleanup(Retention::Months3, NOW, true).unwrap(), 1, "nor one without text");
    assert!(!entries[4].1.exists());
    assert_eq!(h.cleanup(Retention::Days3, NOW, false).unwrap(), 3);
    assert_eq!(entry_ids(dir.path()), [entries[0].0, entries[1].0, busy]);
    assert_eq!(wav_count(&dir.path().join(RECORDINGS_DIR)), 3);

    // A busy one is neither removed nor counted; zero keeps one.
    assert_eq!(h.cleanup(Retention::Count(1), NOW, false).unwrap(), 1);
    assert_eq!(h.cleanup(Retention::Count(0), NOW, false).unwrap(), 0);
    assert_eq!(entry_ids(dir.path()), [entries[0].0, busy]);
  }

  /// A busy entry, recordings without an entry (with speech, without, unreadable), and an entry
  /// without text whose recording is gone.
  #[test]
  fn recovers_what_a_crash_left() {
    let dir = TempDir::new("history-recover");
    let h = History::open(dir.path()).unwrap();
    let (stuck, _) = add(&h, 0, "", true);
    let (gone, gone_wav) = add(&h, DAY, "", false);
    fs::remove_file(gone_wav).unwrap();

    // Killed while recording: no finalize, the header written with the samples.
    let (name, path) = h.recordings.new_file(NOW + 5);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&tone(1.5, 0.5)).unwrap();
    mem::forget(sink);
    let (_, silent) = h.recordings.new_file(NOW + 6);
    write_wav(&silent, &[0.0; 16_000]);
    let (_, broken) = h.recordings.new_file(NOW + 7);
    fs::write(&broken, b"RIFF....WAVE").unwrap();

    h.recover(NOW + 10).unwrap();
    assert!(!h.db.get_entry(stuck).unwrap().unwrap().busy);
    assert_eq!(h.db.get_entry(gone).unwrap(), None);
    let adopted = h.db.get_entries().unwrap().pop().unwrap();
    assert_eq!((adopted.audio_path.as_deref(), adopted.created_at), (Some(name.as_str()), NOW + 5));
    assert_eq!(adopted.duration_ms, 1_500);
    assert!(!silent.exists() && !broken.exists());
  }

  #[test]
  fn paths_stay_inside_the_recordings_directory() {
    let dir = TempDir::new("history-paths");
    let r = Recordings::open(dir.path()).unwrap();
    for good in ["handy-1.wav", "2026-10-09/1.wav"] {
      assert_eq!(r.path(good), Some(r.dir.join(good)), "{good}");
    }

    for bad in
      ["../history.db", "/etc/passwd", "..", "", "a/b.wav", "2026-10-09/../x", "2026-1-09/1.wav"]
    {
      assert_eq!(r.path(bad), None, "{bad}");
    }
  }
}
