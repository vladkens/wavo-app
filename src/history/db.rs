// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `history.db`: the entries, one per dictation kept.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// The error of an entry left `transcribing` by a crash ([`Db::mark_interrupted`]).
pub const STOPPED_TRANSCRIBING: &str = "Wavo stopped during the transcription";

// MARK: Migrations

fn migrate_v1(db: &Connection) -> Result<()> {
  let queries = vec![
    // Ids are never used again (`AUTOINCREMENT`): an id the History tab or `wavo-app ctl` holds
    // must not come back as another dictation.
    "CREATE TABLE entries (
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
    );",
    "CREATE INDEX entries_created_at ON entries (created_at);",
    "CREATE INDEX entries_audio_path ON entries (audio_path);",
  ];

  for qs in queries {
    db.execute(qs, [])?;
  }

  Ok(())
}

/// Runs the migrations the database has not had, each in its own transaction. A database from a
/// newer version is refused rather than written in a layout this version does not know.
fn migrate(db: &Connection) -> Result<()> {
  let migrations: Vec<fn(&Connection) -> Result<()>> = vec![migrate_v1];
  let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
  let known = migrations.len() as i64;
  if version > known {
    bail!("the database has schema {version}, newer than this Wavo knows ({known})");
  }

  for (idx, func) in migrations.iter().enumerate() {
    let mig_ver = idx as i64 + 1;
    if version < mig_ver {
      let tx = db.unchecked_transaction()?;
      func(&tx)?;
      tx.pragma_update(None, "user_version", mig_ver)?;
      tx.commit()?;
    }
  }

  Ok(())
}

// MARK: Models

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
  /// The recording, as [`Recordings`](super::recordings::Recordings) names it.
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

/// A new entry; the rest of [`Entry`] starts empty.
#[derive(Debug, Clone, Default)]
pub struct NewEntry<'a> {
  pub created_at: i64,
  pub duration_ms: i64,
  pub audio_path: Option<&'a str>,
  pub model: Option<&'a str>,
  pub language: Option<&'a str>,
  /// `None` is [`Status::Transcribing`].
  pub status: Option<Status>,
  pub error: Option<&'a str>,
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<Entry> {
  let status: String = row.get("status")?;
  Ok(Entry {
    id: row.get("id")?,
    created_at: row.get("created_at")?,
    duration_ms: row.get("duration_ms")?,
    audio_path: row.get("audio_path")?,
    model: row.get("model")?,
    language: row.get("language")?,
    text: row.get("text")?,
    status: Status::parse(&status).unwrap_or(Status::Failed),
    error: row.get("error")?,
    pasted: row.get("pasted")?,
    not_pasted: row.get("not_pasted")?,
  })
}

/// An entry's id and its recording.
pub type Victim = (i64, Option<String>);

fn victim_from_row(row: &Row<'_>) -> rusqlite::Result<Victim> {
  Ok((row.get("id")?, row.get("audio_path")?))
}

// MARK: Db

pub struct Db {
  db: Connection,
}

impl Db {
  /// Opens (creating, migrating) the database at `path`.
  pub fn open(path: &Path) -> Result<Self> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&db).with_context(|| format!("migrate {}", path.display()))?;
    Ok(Self { db })
  }

  /// Moves on every row this connection writes, and on every commit of another one (the
  /// background cleanup): a history window reloads when it does.
  pub fn changes(&self) -> u64 {
    let others: i64 = self.db.query_row("PRAGMA data_version", [], |r| r.get(0)).unwrap_or(0);
    self.db.total_changes() + others as u64
  }

  // MARK: Getters

  pub fn get_entry(&self, id: i64) -> Result<Option<Entry>> {
    let qs = "SELECT * FROM entries WHERE id = ?1;";
    let item = self.db.prepare_cached(qs)?.query_row([id], entry_from_row).optional()?;
    Ok(item)
  }

  /// The recordings entries name.
  pub fn get_audio_paths(&self) -> Result<HashSet<String>> {
    let qs = "SELECT audio_path FROM entries WHERE audio_path IS NOT NULL;";
    let mut stmt = self.db.prepare(qs)?;
    let items = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// The entries after the newest `keep`; entries still being transcribed are neither returned
  /// nor counted.
  pub fn get_beyond_newest(&self, keep: u32) -> Result<Vec<Victim>> {
    let qs = "
    SELECT id, audio_path FROM entries WHERE status != 'transcribing'
    ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1;
    ";

    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map([keep], victim_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// The entries recorded before `at` (Unix ms), but those still being transcribed.
  pub fn get_created_before(&self, at: i64) -> Result<Vec<Victim>> {
    let qs = "
    SELECT id, audio_path FROM entries WHERE status != 'transcribing' AND created_at < ?1;
    ";

    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map([at], victim_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  // MARK: Inserters

  pub fn insert_entry(&self, entry: &NewEntry<'_>) -> Result<i64> {
    let qs = "
    INSERT INTO entries (created_at, duration_ms, audio_path, model, language, status, error)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);
    ";

    let status = entry.status.unwrap_or(Status::Transcribing);
    self.db.prepare_cached(qs)?.execute(params![
      entry.created_at,
      entry.duration_ms,
      entry.audio_path,
      entry.model,
      entry.language,
      status.as_str(),
      entry.error,
    ])?;

    Ok(self.db.last_insert_rowid())
  }

  // MARK: Updaters

  /// Sets the outcome; `text` `None` keeps the text the entry has.
  pub fn update_outcome(
    &self,
    id: i64,
    status: Status,
    text: Option<&str>,
    error: Option<&str>,
  ) -> Result<()> {
    let qs = "UPDATE entries SET status = ?2, text = COALESCE(?3, text), error = ?4 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, status.as_str(), text, error])?;
    Ok(())
  }

  /// Marks an entry as being transcribed again, with `model` and `language`. Its text stays until
  /// the new result replaces it.
  pub fn mark_retranscribing(&self, id: i64, model: &str, language: Option<&str>) -> Result<()> {
    let qs = "
    UPDATE entries SET status = 'transcribing', error = NULL, model = ?2, language = ?3
    WHERE id = ?1;
    ";

    self.db.prepare_cached(qs)?.execute(params![id, model, language])?;
    Ok(())
  }

  /// Puts back `entry`'s status, model, language and error, as they were before
  /// [`mark_retranscribing`](Self::mark_retranscribing): the text it kept is the one they go with.
  pub fn update_entry(&self, entry: &Entry) -> Result<()> {
    let qs = "UPDATE entries SET status = ?2, model = ?3, language = ?4, error = ?5 WHERE id = ?1;";
    let status = entry.status.as_str();
    let args = params![entry.id, status, entry.model, entry.language, entry.error];
    self.db.prepare_cached(qs)?.execute(args)?;
    Ok(())
  }

  /// Whether the text was pasted, and if a paste was refused, why.
  pub fn mark_pasted(&self, id: i64, pasted: bool, not_pasted: Option<&str>) -> Result<()> {
    let qs = "UPDATE entries SET pasted = ?2, not_pasted = ?3 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, pasted, not_pasted])?;
    Ok(())
  }

  /// Entries left `transcribing` by a crash become `failed`; returns how many.
  pub fn mark_interrupted(&self) -> Result<usize> {
    let qs = "UPDATE entries SET status = 'failed', error = ?1 WHERE status = 'transcribing';";
    Ok(self.db.execute(qs, [STOPPED_TRANSCRIBING])?)
  }

  // MARK: Deleters

  pub fn delete_entry(&self, id: i64) -> Result<()> {
    let qs = "DELETE FROM entries WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute([id])?;
    Ok(())
  }

  /// Deletes the entries in one transaction.
  pub fn delete_entries(&self, ids: &[i64]) -> Result<()> {
    let qs = "DELETE FROM entries WHERE id = ?1;";
    let tx = self.db.unchecked_transaction()?;
    {
      let mut stmt = tx.prepare_cached(qs)?;
      for id in ids {
        stmt.execute([id])?;
      }
    }

    tx.commit()?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::TempDir;

  fn open(dir: &TempDir) -> Db {
    Db::open(&dir.path().join("history.db")).unwrap()
  }

  fn insert(db: &Db, status: Status) -> i64 {
    db.insert_entry(&NewEntry { status: Some(status), ..NewEntry::default() }).unwrap()
  }

  #[test]
  fn creates_and_migrates_once() {
    let dir = TempDir::new("db-migrate");
    let db = open(&dir);
    let version: i64 = db.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 1);
    let id = insert(&db, Status::Done);
    drop(db);

    // Reopening keeps the data and runs no migration again.
    let db = open(&dir);
    assert_eq!(db.get_entry(id).unwrap().map(|e| e.status), Some(Status::Done));
  }

  #[test]
  fn changes_move_with_every_write() {
    let dir = TempDir::new("db-changes");
    let db = open(&dir);
    let start = db.changes();
    let id = insert(&db, Status::Transcribing);
    db.update_outcome(id, Status::Done, Some("text"), None).unwrap();
    db.get_entry(id).unwrap();
    assert_eq!(db.changes(), start + 2, "reads change nothing");
    insert(&open(&dir), Status::Done);
    assert_ne!(db.changes(), start + 2, "another connection's write");
  }

  #[test]
  fn refuses_a_newer_schema() {
    let dir = TempDir::new("db-newer");
    open(&dir).db.pragma_update(None, "user_version", 99).unwrap();
    let err = format!("{:#}", Db::open(&dir.path().join("history.db")).err().unwrap());
    assert!(err.contains("newer"), "{err}");
  }

  #[test]
  fn ids_are_never_used_again() {
    let dir = TempDir::new("db-ids");
    let db = open(&dir);
    let first = insert(&db, Status::Done);
    let newest = insert(&db, Status::Done);
    db.delete_entry(newest).unwrap();
    let next = insert(&db, Status::Done);
    assert_eq!((first, newest, next), (1, 2, 3), "the deleted newest id is not used again");
  }

  #[test]
  fn statuses_read_back_and_an_unknown_one_is_failed() {
    for status in Status::ALL {
      assert_eq!(Status::parse(status.as_str()), Some(status));
    }

    let dir = TempDir::new("db-status");
    let db = open(&dir);
    let id = insert(&db, Status::Done);
    db.db.execute("UPDATE entries SET status = 'cancelled' WHERE id = ?1", [id]).unwrap();
    assert_eq!(db.get_entry(id).unwrap().unwrap().status, Status::Failed);
  }

  #[test]
  fn entry_lifecycle() {
    let dir = TempDir::new("db-entry");
    let db = open(&dir);
    let id = db
      .insert_entry(&NewEntry {
        created_at: 1_000,
        duration_ms: 4_200,
        audio_path: Some("2026-10-09/1000.wav"),
        model: Some("gigaam-v3-e2e-rnnt-Q8_0"),
        ..NewEntry::default()
      })
      .unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!(
      (entry.status, entry.text.as_str(), entry.pasted),
      (Status::Transcribing, "", false)
    );
    assert_eq!((entry.created_at, entry.duration_ms), (1_000, 4_200));
    assert_eq!(entry.audio_path.as_deref(), Some("2026-10-09/1000.wav"));

    db.update_outcome(id, Status::Done, Some("Привет"), None).unwrap();
    db.mark_pasted(id, false, Some("the focus moved")).unwrap();
    assert_eq!(db.get_entry(id).unwrap().unwrap().not_pasted.as_deref(), Some("the focus moved"));
    db.mark_pasted(id, true, None).unwrap();

    // Again with another model: the text stays until the new result; a failure keeps it.
    db.mark_retranscribing(id, "whisper", Some("ru")).unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.text.as_str()), (Status::Transcribing, "Привет"));
    assert_eq!((entry.model.as_deref(), entry.language.as_deref()), (Some("whisper"), Some("ru")));
    db.update_outcome(id, Status::Failed, None, Some("the engine crashed")).unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!(entry.status, Status::Failed);
    assert_eq!(
      (entry.text.as_str(), entry.error.as_deref()),
      ("Привет", Some("the engine crashed"))
    );
    assert!(entry.pasted && entry.not_pasted.is_none());
    assert_eq!(db.get_entry(id + 1).unwrap(), None);

    // Or the entry is put back as it was before.
    db.update_outcome(id, Status::Done, None, None).unwrap();
    let before = db.get_entry(id).unwrap().unwrap();
    db.mark_retranscribing(id, "parakeet", None).unwrap();
    db.update_entry(&before).unwrap();
    assert_eq!(db.get_entry(id).unwrap().unwrap(), before);
  }

  #[test]
  fn interrupted_transcriptions_are_marked_failed() {
    let dir = TempDir::new("db-interrupted");
    let db = open(&dir);
    let done = insert(&db, Status::Done);
    let stuck = insert(&db, Status::Transcribing);
    assert_eq!(db.mark_interrupted().unwrap(), 1);
    assert_eq!(db.get_entry(done).unwrap().unwrap().status, Status::Done);
    let stuck = db.get_entry(stuck).unwrap().unwrap();
    assert_eq!(
      (stuck.status, stuck.error.as_deref()),
      (Status::Failed, Some(STOPPED_TRANSCRIBING))
    );
    assert_eq!(db.mark_interrupted().unwrap(), 0);
  }
}
