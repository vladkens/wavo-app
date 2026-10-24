// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `history.db`: the entries, one per dictation transcribed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

/// How long a query waits for another connection's write.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

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
      text TEXT NOT NULL CHECK (text != ''),
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

/// A dictation transcribed: the history keeps nothing else (a failed dictation's recording is kept
/// on its own, [`Recordings::keep_failed`](super::recordings::Recordings::keep_failed)).
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
  /// Never empty.
  pub text: String,
  pub pasted: bool,
  /// Why a paste was refused (the focus moved, a password field…); `None` also without
  /// auto-paste.
  pub not_pasted: Option<String>,
}

/// A new entry; it is not pasted yet.
#[derive(Debug, Clone, Default)]
pub struct NewEntry<'a> {
  pub created_at: i64,
  pub duration_ms: i64,
  pub audio_path: Option<&'a str>,
  pub model: Option<&'a str>,
  pub language: Option<&'a str>,
  pub text: &'a str,
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<Entry> {
  Ok(Entry {
    id: row.get("id")?,
    created_at: row.get("created_at")?,
    duration_ms: row.get("duration_ms")?,
    audio_path: row.get("audio_path")?,
    model: row.get("model")?,
    language: row.get("language")?,
    text: row.get("text")?,
    pasted: row.get("pasted")?,
    not_pasted: row.get("not_pasted")?,
  })
}

/// An entry's id and its recording.
pub type EntryAudio = (i64, Option<String>);

fn entry_audio_from_row(row: &Row<'_>) -> rusqlite::Result<EntryAudio> {
  Ok((row.get("id")?, row.get("audio_path")?))
}

// MARK: Db

/// Opens the SQLite file at `path` read-only, any app's: it takes a shared lock and writes nothing,
/// not even a journal.
pub fn open_read_only(path: &Path) -> Result<Connection> {
  let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
  let db =
    Connection::open_with_flags(path, flags).with_context(|| format!("open {}", path.display()))?;
  db.busy_timeout(BUSY_TIMEOUT)?;
  Ok(db)
}

pub struct Db {
  db: Connection,
}

impl Db {
  /// Opens (creating, migrating) the database at `path`.
  pub fn open(path: &Path) -> Result<Self> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(BUSY_TIMEOUT)?;
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

  /// The entries after the newest `keep`.
  pub fn get_beyond_newest(&self, keep: u32) -> Result<Vec<EntryAudio>> {
    let qs =
      "SELECT id, audio_path FROM entries ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1;";
    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map([keep], entry_audio_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// The entries recorded before `at` (Unix ms).
  pub fn get_created_before(&self, at: i64) -> Result<Vec<EntryAudio>> {
    let qs = "SELECT id, audio_path FROM entries WHERE created_at < ?1;";
    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map([at], entry_audio_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// Every entry, oldest first.
  pub fn get_entries(&self) -> Result<Vec<Entry>> {
    let qs = "SELECT * FROM entries ORDER BY created_at, id;";
    let mut stmt = self.db.prepare(qs)?;
    let items = stmt.query_map([], entry_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// Every entry by when it was recorded and its text, with its recording: what an import matches
  /// entries by.
  pub fn get_keys(&self) -> Result<HashMap<(i64, String), Option<String>>> {
    let qs = "SELECT created_at, text, audio_path FROM entries;";
    let mut stmt = self.db.prepare(qs)?;
    let rows = stmt.query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?;
    let items = rows.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  // MARK: Inserters

  pub fn insert_entry(&self, entry: &NewEntry<'_>) -> Result<i64> {
    let qs = "
    INSERT INTO entries (created_at, duration_ms, audio_path, model, language, text)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6);
    ";

    self.db.prepare_cached(qs)?.execute(params![
      entry.created_at,
      entry.duration_ms,
      entry.audio_path,
      entry.model,
      entry.language,
      entry.text,
    ])?;

    Ok(self.db.last_insert_rowid())
  }

  /// Adds entries from another history, as they were, in one transaction; their ids are new.
  pub fn insert_imported(&self, entries: &[Entry]) -> Result<()> {
    let qs = "
    INSERT INTO entries (
      created_at, duration_ms, audio_path, model, language, text, pasted, not_pasted
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8);
    ";

    let tx = self.db.unchecked_transaction()?;
    {
      let mut stmt = tx.prepare_cached(qs)?;
      for e in entries {
        stmt.execute(params![
          e.created_at,
          e.duration_ms,
          e.audio_path,
          e.model,
          e.language,
          e.text,
          e.pasted,
          e.not_pasted,
        ])?;
      }
    }

    tx.commit()?;
    Ok(())
  }

  // MARK: Updater

  /// The entries naming one of `names` name no recording, in one transaction.
  pub fn update_without_audio<'a>(
    &self,
    names: impl IntoIterator<Item = &'a String>,
  ) -> Result<()> {
    let qs = "UPDATE entries SET audio_path = NULL WHERE audio_path = ?1;";
    let tx = self.db.unchecked_transaction()?;
    {
      let mut stmt = tx.prepare_cached(qs)?;
      for name in names {
        stmt.execute([name])?;
      }
    }

    tx.commit()?;
    Ok(())
  }

  /// The text of a new transcription, with the model and language it was made with; false if
  /// there is no such entry.
  pub fn update_text(
    &self,
    id: i64,
    text: &str,
    model: &str,
    language: Option<&str>,
  ) -> Result<bool> {
    let qs = "UPDATE entries SET text = ?2, model = ?3, language = ?4 WHERE id = ?1;";
    let changed = self.db.prepare_cached(qs)?.execute(params![id, text, model, language])?;
    Ok(changed > 0)
  }

  /// Whether the text was pasted, and if a paste was refused, why.
  pub fn mark_pasted(&self, id: i64, pasted: bool, not_pasted: Option<&str>) -> Result<()> {
    let qs = "UPDATE entries SET pasted = ?2, not_pasted = ?3 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, pasted, not_pasted])?;
    Ok(())
  }

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

  fn insert(db: &Db) -> i64 {
    db.insert_entry(&NewEntry { text: "текст", ..NewEntry::default() }).unwrap()
  }

  #[test]
  fn creates_and_migrates_once() {
    let dir = TempDir::new("db-migrate");
    let db = open(&dir);
    let version: i64 = db.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 1);
    let id = insert(&db);
    drop(db);

    // Reopening keeps the data and runs no migration again.
    let db = open(&dir);
    assert_eq!(db.get_entry(id).unwrap().map(|e| e.text), Some("текст".into()));
  }

  #[test]
  fn changes_move_with_every_write() {
    let dir = TempDir::new("db-changes");
    let db = open(&dir);
    let start = db.changes();
    let id = insert(&db);
    db.mark_pasted(id, true, None).unwrap();
    db.get_entry(id).unwrap();
    assert_eq!(db.changes(), start + 2, "reads change nothing");
    insert(&open(&dir));
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
    let first = insert(&db);
    let newest = insert(&db);
    db.delete_entry(newest).unwrap();
    let next = insert(&db);
    assert_eq!((first, newest, next), (1, 2, 3), "the deleted newest id is not used again");
  }

  #[test]
  fn an_entry_without_text_is_refused() {
    let dir = TempDir::new("db-no-text");
    let db = open(&dir);
    assert!(db.insert_entry(&NewEntry::default()).is_err());
    let id = insert(&db);
    assert!(db.update_text(id, "", "whisper", None).is_err());
    assert_eq!(db.get_entry(id).unwrap().unwrap().text, "текст");
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
        language: None,
        text: "Привет",
      })
      .unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.text.as_str(), entry.pasted), ("Привет", false));
    assert_eq!((entry.created_at, entry.duration_ms), (1_000, 4_200));
    assert_eq!(entry.audio_path.as_deref(), Some("2026-10-09/1000.wav"));

    db.mark_pasted(id, false, Some("the focus moved")).unwrap();
    assert_eq!(db.get_entry(id).unwrap().unwrap().not_pasted.as_deref(), Some("the focus moved"));
    db.mark_pasted(id, true, None).unwrap();

    // Again with another model: the new text with what made it.
    assert!(db.update_text(id, "Привет!", "whisper", Some("ru")).unwrap());
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!(entry.text, "Привет!");
    assert_eq!((entry.model.as_deref(), entry.language.as_deref()), (Some("whisper"), Some("ru")));
    assert!(entry.pasted && entry.not_pasted.is_none());
    assert_eq!(db.get_entry(id + 1).unwrap(), None);
    assert!(!db.update_text(id + 1, "x", "whisper", None).unwrap(), "no such entry");
  }

  #[test]
  fn imported_entries_are_found_by_time_and_text() {
    let dir = TempDir::new("db-imported");
    let db = open(&dir);
    let entry = Entry {
      id: 0,
      created_at: 1_000,
      duration_ms: 2_500,
      audio_path: Some("1000.wav".into()),
      model: Some("handy".into()),
      language: Some("en".into()),
      text: "hello".into(),
      pasted: true,
      not_pasted: None,
    };
    let other = Entry { created_at: 2_000, audio_path: None, ..entry.clone() };
    db.insert_imported(&[other, entry.clone()]).unwrap();
    assert_eq!(db.get_entry(2).unwrap(), Some(Entry { id: 2, ..entry.clone() }));
    let keys = db.get_keys().unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[&(1_000, "hello".into())].as_deref(), Some("1000.wav"));
    assert_eq!(keys[&(2_000, "hello".into())], None);
    assert_eq!(db.get_entries().unwrap()[0], Entry { id: 2, ..entry });
    db.update_without_audio(&["1000.wav".to_owned()]).unwrap();
    assert!(db.get_audio_paths().unwrap().is_empty());
  }
}
