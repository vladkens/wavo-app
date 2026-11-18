// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! File transcripts, in `history.db` beside the dictation history but no part of it: a transcript
//! stays until the person deletes it, whatever the history's retention, and on disk also while
//! dictations are kept in memory.
//!
//! - `transcripts`: a file's (later a meeting's), the model and language it runs with, how far it
//!   got. The file is the person's and never copied: its path, size and modified time tell later
//!   whether it is still the same one ([Source]).
//! - `segments`: its text in [Segment]s with their times; `speaker` stays `NULL` until speakers are
//!   told apart.
//!
//! The work goes a chunk of audio at a time: a chunk's segments and the time done go in one
//! transaction ([Transcripts::append]), so a quit or a crash leaves a transcript that goes on from
//! its `done_ms`.

use std::cell::OnceCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Row, params};

use crate::history::{DB_FILE, Db, fold};
use crate::segments::Segment;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
  /// Waits for its turn: the oldest queued or running one goes first.
  #[default]
  Queued,
  /// Being transcribed, or was when Wavo quit: it goes on at the next start.
  Running,
  /// The person stopped it: the text so far stays, and Continue goes on from there.
  Stopped,
  Done,
  /// Its `error` tells why; the text so far stays.
  Failed,
}

impl State {
  fn as_str(self) -> &'static str {
    match self {
      Self::Queued => "queued",
      Self::Running => "running",
      Self::Stopped => "stopped",
      Self::Done => "done",
      Self::Failed => "failed",
    }
  }

  fn parse(s: &str) -> Self {
    match s {
      "running" => Self::Running,
      "stopped" => Self::Stopped,
      "done" => Self::Done,
      "failed" => Self::Failed,
      _ => Self::Queued,
    }
  }

  /// Queued or running: the work will get to it.
  pub fn pending(self) -> bool {
    matches!(self, Self::Queued | Self::Running)
  }
}

/// The person's file as it was when a transcript was made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
  pub path: PathBuf,
  pub size: i64,
  /// Unix ms.
  pub modified: i64,
}

impl Source {
  /// The file at `path` as it is now.
  pub fn of(path: &Path) -> Result<Self> {
    let meta = fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    let since = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    let modified = since.map_or(0, |d| d.as_millis() as i64);
    Ok(Self { path: path.to_owned(), size: meta.len() as i64, modified })
  }

  /// The file's name, as the transcript is called.
  pub fn name(&self) -> String {
    self.path.file_name().unwrap_or(self.path.as_os_str()).to_string_lossy().into_owned()
  }
}

/// Whether the file is still the one a transcript was made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
  Same,
  /// Its size or modified time is another.
  Changed,
  Missing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
  pub id: i64,
  /// Unix ms.
  pub created_at: i64,
  pub name: String,
  pub path: PathBuf,
  pub size: i64,
  pub modified: i64,
  /// Known once its file is opened, if the file tells.
  pub duration_ms: Option<i64>,
  /// The model it runs with: wavo's name of it, or its file's path.
  pub model: String,
  /// `None`: told by the model.
  pub language: Option<String>,
  pub state: State,
  /// How far into the audio its segments go.
  pub done_ms: i64,
  pub error: Option<String>,
  /// The first words of its text.
  pub preview: String,
}

impl Transcript {
  fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
    Ok(Self {
      id: row.get("id")?,
      created_at: row.get("created_at")?,
      name: row.get("name")?,
      path: PathBuf::from(row.get::<_, String>("path")?),
      size: row.get("size")?,
      modified: row.get("modified")?,
      duration_ms: row.get("duration_ms")?,
      model: row.get("model")?,
      language: row.get("language")?,
      state: State::parse(&row.get::<_, String>("state")?),
      done_ms: row.get("done_ms")?,
      error: row.get("error")?,
      preview: row.get::<_, Option<String>>("preview")?.unwrap_or_default(),
    })
  }

  pub fn source(&self) -> Source {
    Source { path: self.path.clone(), size: self.size, modified: self.modified }
  }

  /// Whether its file is still there and the same.
  pub fn check(&self) -> Check {
    match Source::of(&self.path) {
      Ok(now) if now == self.source() => Check::Same,
      Ok(_) => Check::Changed,
      Err(_) => Check::Missing,
    }
  }
}

/// A transcript's columns, and the first words of its text.
const COLUMNS: &str = "
  t.*, (SELECT substr(group_concat(text, ' '), 1, 200) FROM (
    SELECT text FROM segments WHERE transcript_id = t.id ORDER BY start_ms, id LIMIT 8
  )) AS preview
";

/// The transcripts of a home, in its `history.db` ([DB_FILE]) from the first one added: until
/// then a home whose dictations are kept in memory has no database.
pub struct Transcripts {
  path: PathBuf,
  db: OnceCell<Db>,
}

impl Transcripts {
  /// The transcripts saved in `home`: always on disk, opened once the database is there.
  pub fn open(home: &Path) -> Self {
    Self { path: home.join(DB_FILE), db: OnceCell::new() }
  }

  /// Those of the database `db` opened (a copy for an export).
  pub fn of(db: Db) -> Self {
    Self { path: PathBuf::new(), db: OnceCell::from(db) }
  }

  /// Moves on every change, of this connection's or another one's; 0 until the database is open.
  pub fn changes(&self) -> u64 {
    self.db.get().map_or(0, Db::changes)
  }

  /// The database to read, `None` while there is none.
  fn read(&self) -> Result<Option<&Db>> {
    match self.db.get().is_some() || self.path.exists() {
      true => self.write().map(Some),
      false => Ok(None),
    }
  }

  /// The database to write, made when not there.
  fn write(&self) -> Result<&Db> {
    if let Some(db) = self.db.get() {
      return Ok(db);
    }

    let db = Db::open(&self.path)?;
    Ok(self.db.get_or_init(|| db))
  }

  // MARK: Getters

  pub fn get(&self, id: i64) -> Result<Option<Transcript>> {
    let Some(db) = self.read()? else { return Ok(None) };
    let qs = format!("SELECT {COLUMNS} FROM transcripts t WHERE id = ?1;");
    Ok(db.conn().prepare_cached(&qs)?.query_row([id], Transcript::from_row).optional()?)
  }

  /// Newest first, those whose name or text holds `search` in any case ([fold]; empty: all).
  pub fn list(&self, search: &str) -> Result<Vec<Transcript>> {
    let Some(db) = self.read()? else { return Ok(Vec::new()) };
    let qs = format!(
      "SELECT {COLUMNS} FROM transcripts t
      WHERE ?1 = '' OR instr(wavo_fold(name), ?1) > 0 OR EXISTS (
        SELECT 1 FROM segments s WHERE s.transcript_id = t.id AND instr(wavo_fold(s.text), ?1) > 0
      )
      ORDER BY created_at DESC, id DESC;"
    );

    let mut stmt = db.conn().prepare_cached(&qs)?;
    let rows = stmt.query_map([fold(search)], Transcript::from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
  }

  /// The one the work goes on with: the oldest queued or running.
  pub fn next(&self) -> Result<Option<Transcript>> {
    let Some(db) = self.read()? else { return Ok(None) };
    let qs = format!(
      "SELECT {COLUMNS} FROM transcripts t WHERE state IN ('queued', 'running')
      ORDER BY id LIMIT 1;"
    );

    Ok(db.conn().prepare_cached(&qs)?.query_row([], Transcript::from_row).optional()?)
  }

  /// Its segments in time order.
  pub fn segments(&self, id: i64) -> Result<Vec<Segment>> {
    let Some(db) = self.read()? else { return Ok(Vec::new()) };
    let qs = "
    SELECT start_ms, end_ms, speaker, text FROM segments WHERE transcript_id = ?1
    ORDER BY start_ms, id;
    ";

    let mut stmt = db.conn().prepare_cached(qs)?;
    let segment = |r: &Row| {
      Ok(Segment { start_ms: r.get(0)?, end_ms: r.get(1)?, speaker: r.get(2)?, text: r.get(3)? })
    };
    Ok(stmt.query_map([id], segment)?.collect::<rusqlite::Result<_>>()?)
  }

  // MARK: Inserters

  /// A new transcript of `source`, queued; its id.
  pub fn add(&self, source: &Source, model: &str, language: Option<&str>, at: i64) -> Result<i64> {
    let qs = "
    INSERT INTO transcripts (created_at, name, path, size, modified, model, language, state)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued');
    ";

    let path = source.path.to_string_lossy();
    let db = self.write()?.conn();
    let row = params![at, source.name(), path, source.size, source.modified, model, language];
    db.prepare_cached(qs)?.execute(row)?;
    Ok(db.last_insert_rowid())
  }

  /// A chunk of transcript `id` is done: its `segments`, and the audio up to `done_ms`.
  pub fn append(&self, id: i64, segments: &[Segment], done_ms: i64) -> Result<()> {
    let tx = self.write()?.conn().unchecked_transaction()?;
    let qs = "INSERT INTO segments (transcript_id, start_ms, end_ms, speaker, text)
      VALUES (?1, ?2, ?3, ?4, ?5);";
    for s in segments {
      tx.prepare_cached(qs)?.execute(params![id, s.start_ms, s.end_ms, s.speaker, s.text])?;
    }

    let qs = "UPDATE transcripts SET done_ms = ?2 WHERE id = ?1;";
    tx.prepare_cached(qs)?.execute(params![id, done_ms])?;
    tx.commit()?;
    Ok(())
  }

  // MARK: Updater

  /// Transcript `id` is being transcribed, its audio `duration_ms` long if known: anything past its
  /// `done_ms` (none should be) goes.
  pub fn start(&self, id: i64, duration_ms: Option<i64>) -> Result<()> {
    let tx = self.write()?.conn().unchecked_transaction()?;
    let qs = "DELETE FROM segments WHERE transcript_id = ?1
      AND start_ms >= (SELECT done_ms FROM transcripts WHERE id = ?1);";
    tx.prepare_cached(qs)?.execute([id])?;
    let qs = "UPDATE transcripts SET state = 'running', error = NULL,
      duration_ms = COALESCE(?2, duration_ms) WHERE id = ?1;";
    tx.prepare_cached(qs)?.execute(params![id, duration_ms])?;
    tx.commit()?;
    Ok(())
  }

  /// Transcript `id` is in `state`, for `error`; done, its duration is what was done.
  pub fn set_state(&self, id: i64, state: State, error: Option<&str>) -> Result<()> {
    let qs = "UPDATE transcripts SET state = ?2, error = ?3,
      duration_ms = CASE WHEN ?2 = 'done' THEN done_ms ELSE duration_ms END WHERE id = ?1;";
    self.write()?.conn().prepare_cached(qs)?.execute(params![id, state.as_str(), error])?;
    Ok(())
  }

  /// Transcript `id`'s file is `source` now (found again, maybe moved); its name stays.
  pub fn relocate(&self, id: i64, source: &Source) -> Result<()> {
    let qs = "UPDATE transcripts SET path = ?2, size = ?3, modified = ?4 WHERE id = ?1;";
    let path = source.path.to_string_lossy();
    let row = params![id, path, source.size, source.modified];
    self.write()?.conn().prepare_cached(qs)?.execute(row)?;
    Ok(())
  }

  /// Deletes transcripts `ids` with their text, in one transaction.
  pub fn delete(&self, ids: &[i64]) -> Result<()> {
    let tx = self.write()?.conn().unchecked_transaction()?;
    for id in ids {
      tx.prepare_cached("DELETE FROM segments WHERE transcript_id = ?1;")?.execute([id])?;
      tx.prepare_cached("DELETE FROM transcripts WHERE id = ?1;")?.execute([id])?;
    }

    tx.commit()?;
    Ok(())
  }

  /// Every transcript goes, and the pages they were on (an export's copy keeps none).
  pub fn clear(&self) -> Result<()> {
    self.write()?.conn().execute_batch("DELETE FROM segments; DELETE FROM transcripts; VACUUM;")?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::history::{Entry, History, Retention};
  use crate::testing::{TempDir, entry_ids};

  fn seg(start_ms: i64, text: &str) -> Segment {
    Segment { start_ms, end_ms: start_ms + 1000, speaker: None, text: text.into() }
  }

  fn file(dir: &Path, name: &str, bytes: &[u8]) -> Source {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    Source::of(&path).unwrap()
  }

  /// Read, a home without a database has no transcripts and still no database: the first one
  /// added makes it.
  #[test]
  fn the_database_is_made_by_the_first_transcript() {
    let dir = TempDir::new("transcripts-none");
    let t = Transcripts::open(dir.path());
    assert!(t.list("").unwrap().is_empty() && t.segments(1).unwrap().is_empty());
    assert!(t.next().unwrap().is_none() && t.get(1).unwrap().is_none());
    assert_eq!(t.changes(), 0);
    assert!(!dir.path().join(DB_FILE).exists());

    t.add(&file(dir.path(), "a.wav", b"a"), "m", None, 1).unwrap();
    assert!(dir.path().join(DB_FILE).exists());
    assert_ne!(t.changes(), 0);
  }

  #[test]
  fn added_run_in_chunks_and_listed() {
    let dir = TempDir::new("transcripts-run");
    let t = Transcripts::open(dir.path());
    let lecture = file(dir.path(), "Лекция 1.m4a", b"audio");
    let id = t.add(&lecture, "gigaam-v3", Some("ru"), 1_000).unwrap();
    let other = t.add(&file(dir.path(), "talk.mp3", b"x"), "parakeet", None, 2_000).unwrap();
    assert_eq!(t.next().unwrap().unwrap().id, id, "the oldest first");

    t.start(id, Some(120_000)).unwrap();
    t.append(id, &[seg(0, "Добрый день."), seg(1000, "Начнём.")], 60_000).unwrap();
    let got = t.get(id).unwrap().unwrap();
    assert_eq!((got.state, got.done_ms, got.duration_ms), (State::Running, 60_000, Some(120_000)));
    assert_eq!((got.name.as_str(), got.preview.as_str()), ("Лекция 1.m4a", "Добрый день. Начнём."));
    assert_eq!(got.check(), Check::Same);

    t.append(id, &[seg(60_000, "Ещё.")], 110_000).unwrap();
    t.set_state(id, State::Done, None).unwrap();
    let done = t.get(id).unwrap().unwrap();
    assert_eq!((done.state, done.duration_ms), (State::Done, Some(110_000)), "as long as done");
    assert_eq!(t.segments(id).unwrap().len(), 3);
    assert_eq!(t.next().unwrap().unwrap().id, other);

    let ids = |search: &str| -> Vec<i64> { t.list(search).unwrap().iter().map(|t| t.id).collect() };
    assert_eq!(ids(""), [other, id]);
    let reader = Transcripts::open(dir.path());
    assert_eq!(reader.list("").unwrap().len(), 2, "another connection reads them");
    assert_eq!(
      (ids("НАЧНЁМ"), ids("лекция"), ids("TALK"), ids("%")),
      (vec![id], vec![id], vec![other], vec![])
    );
  }

  /// A chunk's segments and its `done_ms` go together; any segment past `done_ms` all the same goes
  /// at the next start, so a resume writes none twice.
  #[test]
  fn a_start_drops_what_is_past_done() {
    let dir = TempDir::new("transcripts-resume");
    let t = Transcripts::open(dir.path());
    let id = t.add(&file(dir.path(), "a.wav", b"a"), "m", None, 1).unwrap();
    t.append(id, &[seg(0, "one"), seg(5_000, "two")], 4_000).unwrap();
    t.start(id, None).unwrap();
    let texts: Vec<String> = t.segments(id).unwrap().into_iter().map(|s| s.text).collect();
    assert_eq!(texts, ["one"]);
  }

  #[test]
  fn a_moved_or_changed_file_is_told() {
    let dir = TempDir::new("transcripts-check");
    let t = Transcripts::open(dir.path());
    let source = file(dir.path(), "a.wav", b"audio");
    let id = t.add(&source, "m", None, 1).unwrap();
    fs::write(&source.path, b"other audio").unwrap();
    assert_eq!(t.get(id).unwrap().unwrap().check(), Check::Changed);
    let moved = dir.path().join("b.wav");
    fs::rename(&source.path, &moved).unwrap();
    assert_eq!(t.get(id).unwrap().unwrap().check(), Check::Missing);

    t.relocate(id, &Source::of(&moved).unwrap()).unwrap();
    let found = t.get(id).unwrap().unwrap();
    assert_eq!((found.check(), found.name.as_str()), (Check::Same, "a.wav"), "named as it was");
  }

  #[test]
  fn deletes_several_with_their_text() {
    let dir = TempDir::new("transcripts-delete");
    let t = Transcripts::open(dir.path());
    let ids: Vec<i64> =
      (0..3).map(|i| t.add(&file(dir.path(), "a.wav", b"a"), "m", None, i).unwrap()).collect();
    for &id in &ids {
      t.append(id, &[seg(0, "text")], 1000).unwrap();
    }

    t.delete(&[ids[0], ids[2], 404]).unwrap();
    assert_eq!(t.list("").unwrap().iter().map(|t| t.id).collect::<Vec<_>>(), [ids[1]]);
    let left: i64 = t
      .write()
      .unwrap()
      .conn()
      .query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))
      .unwrap();
    assert_eq!(left, 1);
  }

  /// The dictation history's deletes, retention and memory leave transcripts alone.
  #[test]
  fn the_history_leaves_them_alone() {
    let dir = TempDir::new("transcripts-history");
    let t = Transcripts::open(dir.path());
    let id = t.add(&file(dir.path(), "a.wav", b"a"), "m", None, 1).unwrap();
    t.append(id, &[seg(0, "kept")], 1000).unwrap();

    let h = History::open(dir.path()).unwrap();
    let entry = Entry { created_at: 1, text: "dictated".into(), ..Entry::default() };
    h.db.insert(&[entry]).unwrap();
    h.cleanup(Retention::Count(1), i64::MAX / 2, false).unwrap();
    History::delete_saved(dir.path()).unwrap();
    assert!(entry_ids(dir.path()).is_empty());
    let memory = History::open_memory(dir.path()).unwrap();
    assert!(memory.db.get_page("", 10).unwrap().is_empty());
    assert_eq!(t.segments(id).unwrap(), [seg(0, "kept")]);
  }
}
