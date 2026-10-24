// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `history.db`: the entries, one per recording with speech. An entry's text may be empty (no
//! model yet, nothing recognized, an engine error, a crash during the transcription): its
//! recording can be transcribed again. No statuses nor errors are kept: those are the session's,
//! shown once.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::fold;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
  pub id: i64,
  /// When the recording started, Unix time in ms.
  pub created_at: i64,
  pub duration_ms: i64,
  /// The recording, as [`Recordings`](super::recordings::Recordings) names it.
  pub audio_path: Option<String>,
  /// Empty until a transcription makes some: "no text".
  pub text: String,
  /// The model the text came from: model file name without extension.
  pub model: Option<String>,
  /// The language the text was asked in; `None` is detected by the model.
  pub language: Option<String>,
  /// The model that last transcribed the recording, whatever came of it ("no text ·
  /// <tried_model>"): an entry without text is transcribed again by itself only by another one.
  pub tried_model: Option<String>,
  /// Being transcribed: the retention leaves it alone. Cleared on start (a crash left it).
  pub busy: bool,
  /// The text a transcription replaced, with its model and language, for one undo
  /// ([`Db::update_undo`]).
  pub prev_text: Option<String>,
  pub prev_model: Option<String>,
  pub prev_language: Option<String>,
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
  pub text: &'a str,
  pub model: Option<&'a str>,
  pub language: Option<&'a str>,
  pub tried_model: Option<&'a str>,
  pub busy: bool,
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<Entry> {
  Ok(Entry {
    id: row.get("id")?,
    created_at: row.get("created_at")?,
    duration_ms: row.get("duration_ms")?,
    audio_path: row.get("audio_path")?,
    text: row.get("text")?,
    model: row.get("model")?,
    language: row.get("language")?,
    tried_model: row.get("tried_model")?,
    busy: row.get("busy")?,
    prev_text: row.get("prev_text")?,
    prev_model: row.get("prev_model")?,
    prev_language: row.get("prev_language")?,
    pasted: row.get("pasted")?,
    not_pasted: row.get("not_pasted")?,
  })
}

/// The entries without text the retention leaves alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spare {
  Nothing,
  /// All of them: no model can transcribe them.
  All,
  /// Those this model has not tried: the session is transcribing them by itself.
  UntriedBy(String),
}

impl Spare {
  /// `?2` and `?3` of the retention's queries: an entry without text goes only if neither is
  /// spared nor untried by `?3`.
  fn params(&self) -> (bool, Option<&str>) {
    match self {
      Spare::Nothing => (false, None),
      Spare::All => (true, None),
      Spare::UntriedBy(model) => (false, Some(model)),
    }
  }
}

/// An entry's id and its recording.
pub type EntryAudio = (i64, Option<String>);

fn entry_audio_from_row(row: &Row<'_>) -> rusqlite::Result<EntryAudio> {
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
    // What a search compares the text by.
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    db.create_scalar_function("wavo_fold", 1, flags, |ctx| Ok(fold(&ctx.get::<String>(0)?)))?;
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

  /// The newest `limit` entries, newest first by recording time, whose text holds `search` in any
  /// case ([`fold`]; empty: all, also those without text).
  pub fn get_page(&self, search: &str, limit: usize) -> Result<Vec<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE ?1 = '' OR instr(wavo_fold(text), ?1) > 0
    ORDER BY created_at DESC, id DESC LIMIT ?2;
    ";

    let mut stmt = self.db.prepare_cached(qs)?;
    let items = stmt.query_map(params![fold(search), limit as i64], entry_from_row)?;
    Ok(items.collect::<rusqlite::Result<_>>()?)
  }

  /// The recordings entries name.
  pub fn get_audio_paths(&self) -> Result<HashSet<String>> {
    let qs = "SELECT audio_path FROM entries WHERE audio_path IS NOT NULL;";
    let mut stmt = self.db.prepare(qs)?;
    let items = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// The entries without text.
  pub fn get_untranscribed(&self) -> Result<Vec<EntryAudio>> {
    let qs = "SELECT id, audio_path FROM entries WHERE text = '';";
    let mut stmt = self.db.prepare(qs)?;
    let items = stmt.query_map([], entry_audio_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(items)
  }

  /// The newest entry without text that `model` has not tried and nothing transcribes now.
  pub fn get_untried(&self, model: &str) -> Result<Option<Entry>> {
    let qs = "
    SELECT * FROM entries
    WHERE text = '' AND busy = 0 AND audio_path IS NOT NULL
      AND (tried_model IS NULL OR tried_model != ?1)
    ORDER BY created_at DESC, id DESC LIMIT 1;
    ";

    let item = self.db.prepare_cached(qs)?.query_row([model], entry_from_row).optional()?;
    Ok(item)
  }

  /// The entries after the newest `keep`. Entries being transcribed, and those `spare` spares, are
  /// neither returned nor counted.
  pub fn get_beyond_newest(&self, keep: u32, spare: &Spare) -> Result<Vec<EntryAudio>> {
    let qs = "
    SELECT id, audio_path FROM entries
    WHERE busy = 0 AND (text != '' OR (?2 = 0 AND (?3 IS NULL OR tried_model IS ?3)))
    ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1;
    ";

    let (all, untried_by) = spare.params();
    let mut stmt = self.db.prepare_cached(qs)?;
    let rows = stmt.query_map(params![keep, all, untried_by], entry_audio_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
  }

  /// The entries recorded before `at` (Unix ms), but those being transcribed and those `spare`
  /// spares.
  pub fn get_created_before(&self, at: i64, spare: &Spare) -> Result<Vec<EntryAudio>> {
    let qs = "
    SELECT id, audio_path FROM entries
    WHERE busy = 0 AND (text != '' OR (?2 = 0 AND (?3 IS NULL OR tried_model IS ?3)))
      AND created_at < ?1;
    ";

    let (all, untried_by) = spare.params();
    let mut stmt = self.db.prepare_cached(qs)?;
    let rows = stmt.query_map(params![at, all, untried_by], entry_audio_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
  }

  // MARK: Inserters

  pub fn insert_entry(&self, entry: &NewEntry<'_>) -> Result<i64> {
    let qs = "
    INSERT INTO entries
      (created_at, duration_ms, audio_path, text, model, language, tried_model, busy)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8);
    ";

    self.db.prepare_cached(qs)?.execute(params![
      entry.created_at,
      entry.duration_ms,
      entry.audio_path,
      entry.text,
      entry.model,
      entry.language,
      entry.tried_model,
      entry.busy,
    ])?;

    Ok(self.db.last_insert_rowid())
  }

  // MARK: Updater

  /// Marks entry `id` as being transcribed by `model`.
  pub fn mark_busy(&self, id: i64, model: &str) -> Result<()> {
    let qs = "UPDATE entries SET busy = 1, tried_model = ?2 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, model])?;
    Ok(())
  }

  /// The transcription of entry `id` ended without text: it keeps what it had.
  pub fn mark_idle(&self, id: i64) -> Result<()> {
    let qs = "UPDATE entries SET busy = 0 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute([id])?;
    Ok(())
  }

  /// The model of entry `id`'s transcription did not load: it did not try the entry, which keeps
  /// `tried_model` from before.
  pub fn update_untried(&self, id: i64, tried_model: Option<&str>) -> Result<()> {
    let qs = "UPDATE entries SET busy = 0, tried_model = ?2 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, tried_model])?;
    Ok(())
  }

  /// Entry `id`'s transcription made `text` (not empty) with `model` in `language`: it replaces the
  /// text, which is kept for one undo if there was any.
  pub fn update_text(
    &self,
    id: i64,
    text: &str,
    model: &str,
    language: Option<&str>,
  ) -> Result<()> {
    let qs = "
    UPDATE entries SET
      prev_text = CASE WHEN text = '' THEN prev_text ELSE text END,
      prev_model = CASE WHEN text = '' THEN prev_model ELSE model END,
      prev_language = CASE WHEN text = '' THEN prev_language ELSE language END,
      text = ?2, model = ?3, language = ?4, busy = 0
    WHERE id = ?1;
    ";

    self.db.prepare_cached(qs)?.execute(params![id, text, model, language])?;
    Ok(())
  }

  /// Puts back the text the last transcription of entry `id` replaced, with its model and
  /// language; the replacing one takes their place, so a second undo brings it back. Returns false
  /// if there is none.
  pub fn update_undo(&self, id: i64) -> Result<bool> {
    let qs = "
    UPDATE entries SET
      text = prev_text, model = prev_model, language = prev_language,
      prev_text = text, prev_model = model, prev_language = language
    WHERE id = ?1 AND prev_text IS NOT NULL;
    ";

    Ok(self.db.prepare_cached(qs)?.execute([id])? > 0)
  }

  /// Whether the text was pasted, and if a paste was refused, why.
  pub fn mark_pasted(&self, id: i64, pasted: bool, not_pasted: Option<&str>) -> Result<()> {
    let qs = "UPDATE entries SET pasted = ?2, not_pasted = ?3 WHERE id = ?1;";
    self.db.prepare_cached(qs)?.execute(params![id, pasted, not_pasted])?;
    Ok(())
  }

  /// Entries left being transcribed by a crash are not any more; returns how many.
  pub fn mark_interrupted(&self) -> Result<usize> {
    let qs = "UPDATE entries SET busy = 0 WHERE busy = 1;";
    Ok(self.db.execute(qs, [])?)
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

  fn insert(db: &Db, text: &str) -> i64 {
    let audio = Some("2026-10-09/1000.wav");
    db.insert_entry(&NewEntry { text, audio_path: audio, ..NewEntry::default() }).unwrap()
  }

  #[test]
  fn creates_and_migrates_once() {
    let dir = TempDir::new("db-migrate");
    let db = open(&dir);
    let version: i64 = db.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 1);
    let id = insert(&db, "text");
    drop(db);

    // Reopening keeps the data and runs no migration again.
    let db = open(&dir);
    assert_eq!(db.get_entry(id).unwrap().map(|e| e.text), Some("text".into()));
  }

  #[test]
  fn changes_move_with_every_write() {
    let dir = TempDir::new("db-changes");
    let db = open(&dir);
    let start = db.changes();
    let id = insert(&db, "");
    db.update_text(id, "text", "parakeet", None).unwrap();
    db.get_entry(id).unwrap();
    assert_eq!(db.changes(), start + 2, "reads change nothing");
    insert(&open(&dir), "");
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
    let first = insert(&db, "a");
    let newest = insert(&db, "b");
    db.delete_entry(newest).unwrap();
    let next = insert(&db, "c");
    assert_eq!((first, newest, next), (1, 2, 3), "the deleted newest id is not used again");
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
        tried_model: Some("gigaam-v3-e2e-rnnt-Q8_0"),
        busy: true,
        ..NewEntry::default()
      })
      .unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.text.as_str(), entry.busy, entry.pasted), ("", true, false));
    assert_eq!((entry.created_at, entry.duration_ms), (1_000, 4_200));
    assert_eq!(entry.audio_path.as_deref(), Some("2026-10-09/1000.wav"));

    // The first text: nothing to undo.
    db.update_text(id, "Привет", "gigaam-v3-e2e-rnnt-Q8_0", Some("ru")).unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.text.as_str(), entry.busy, entry.prev_text), ("Привет", false, None));
    assert!(!db.update_undo(id).unwrap());
    db.mark_pasted(id, false, Some("the focus moved")).unwrap();
    assert_eq!(db.get_entry(id).unwrap().unwrap().not_pasted.as_deref(), Some("the focus moved"));
    db.mark_pasted(id, true, None).unwrap();

    // Again with another model: a run without text keeps the text, and says who tried.
    db.mark_busy(id, "whisper").unwrap();
    db.mark_idle(id).unwrap();
    db.mark_busy(id, "broken").unwrap();
    db.update_untried(id, Some("whisper")).unwrap();
    let entry = db.get_entry(id).unwrap().unwrap();
    assert_eq!(
      (entry.text.as_str(), entry.model.as_deref()),
      ("Привет", Some("gigaam-v3-e2e-rnnt-Q8_0"))
    );
    assert_eq!((entry.tried_model.as_deref(), entry.busy), (Some("whisper"), false));

    // New text replaces it, which an undo brings back, and a second undo the new one again.
    db.mark_busy(id, "whisper").unwrap();
    db.update_text(id, "Pree-vet", "whisper", None).unwrap();
    let replaced = db.get_entry(id).unwrap().unwrap();
    assert_eq!(
      (replaced.text.as_str(), replaced.prev_text.as_deref()),
      ("Pree-vet", Some("Привет"))
    );
    assert!(db.update_undo(id).unwrap());
    let undone = db.get_entry(id).unwrap().unwrap();
    assert_eq!(
      (undone.text.as_str(), undone.model.as_deref()),
      ("Привет", Some("gigaam-v3-e2e-rnnt-Q8_0"))
    );
    assert_eq!(
      (undone.language.as_deref(), undone.prev_text.as_deref()),
      (Some("ru"), Some("Pree-vet"))
    );
    assert!(undone.pasted && undone.not_pasted.is_none());
    assert!(db.update_undo(id).unwrap());
    assert_eq!(db.get_entry(id).unwrap().unwrap(), replaced);
    assert_eq!(db.get_entry(id + 1).unwrap(), None);
  }

  /// Entries without text another model has not tried, newest first, none being transcribed.
  #[test]
  fn untried_entries_for_a_model() {
    let dir = TempDir::new("db-untried");
    let db = open(&dir);
    let entry = |created_at, text, tried_model| {
      let audio = Some("1.wav");
      let new =
        NewEntry { created_at, text, tried_model, audio_path: audio, ..NewEntry::default() };
      db.insert_entry(&new).unwrap()
    };

    let old = entry(1, "", None);
    let tried = entry(2, "", Some("parakeet"));
    entry(3, "text", None);
    let newest = entry(4, "", Some("whisper"));
    let untried = |model| db.get_untried(model).unwrap().map(|e| e.id);
    assert_eq!(untried("parakeet"), Some(newest));
    assert_eq!(untried("whisper"), Some(tried));
    db.mark_busy(tried, "gigaam").unwrap();
    assert_eq!(untried("whisper"), Some(old), "not one being transcribed");
    assert_eq!(db.mark_interrupted().unwrap(), 1);
    assert_eq!(untried("whisper"), Some(tried), "a crash left it");
    assert_eq!(db.get_untranscribed().unwrap().len(), 3);
  }

  /// The retention's picks: never an entry being transcribed, and entries without text only when
  /// they are not spared.
  #[test]
  fn retention_picks_spare_busy_and_untranscribed_entries() {
    let dir = TempDir::new("db-retention");
    let db = open(&dir);
    let new = |created_at, text, tried_model, busy| NewEntry {
      created_at,
      text,
      tried_model,
      busy,
      ..NewEntry::default()
    };
    let ids: Vec<i64> = [
      new(1, "a", None, false),
      new(2, "", None, false),
      new(3, "", Some("whisper"), false),
      new(4, "b", None, true),
      new(5, "c", None, false),
    ]
    .iter()
    .map(|e| db.insert_entry(e).unwrap())
    .collect();
    let picked = |items: Vec<EntryAudio>| items.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    let before = |spare| picked(db.get_created_before(5, &spare).unwrap());
    assert_eq!(before(Spare::Nothing), [ids[0], ids[1], ids[2]]);
    assert_eq!(before(Spare::All), [ids[0]]);
    assert_eq!(before(Spare::UntriedBy("whisper".into())), [ids[0], ids[2]]);
    let beyond = |spare| picked(db.get_beyond_newest(1, &spare).unwrap());
    assert_eq!(beyond(Spare::Nothing), [ids[2], ids[1], ids[0]]);
    assert_eq!(beyond(Spare::All), [ids[0]]);
  }

  #[test]
  fn pages_are_newest_first_and_searched_in_any_case() {
    let dir = TempDir::new("db-page");
    let db = open(&dir);
    let texts = ["Добрый день.", "Ещё раз", "hello World", "ДОБРЫЙ вечер", "", "еще одно"];
    let ids: Vec<i64> = (texts.iter().enumerate())
      .map(|(i, text)| {
        let entry = NewEntry { created_at: 10_000 - i as i64 * 1_000, text, ..NewEntry::default() };
        db.insert_entry(&entry).unwrap()
      })
      .collect();

    let page = |search: &str, limit| -> Vec<i64> {
      db.get_page(search, limit).unwrap().iter().map(|e| e.id).collect()
    };
    assert_eq!(page("", 100), ids, "the one without text too");
    assert_eq!(page("", 2), ids[..2]);
    // Case in Cyrillic and Latin, `ё` as `е`; a quote or a percent sign is just text.
    assert_eq!(page("добрый", 100), [ids[0], ids[3]]);
    assert_eq!(page("WORLD", 100), [ids[2]]);
    assert_eq!(page("ещё", 100), [ids[1], ids[5]]);
    assert!(page("%", 100).is_empty() && page("'", 100).is_empty());
  }
}
