// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation history in the data home: the entries in [`db`] (`history.db`, SQLite) and the
//! [`recordings`] they point to. [`History`] does what touches both.
//!
//! - It keeps what was transcribed, nothing else: a dictation that failed or that nothing was
//!   recognized in leaves its recording as the last failed one ([`Recordings::keep_failed`]), to
//!   transcribe again; a cancelled dictation and one without speech leave nothing.
//! - [`History::cleanup`] applies the retention: entries and their audio files go together, the
//!   file first, so a crash in between leaves a row whose file is already gone, never a file
//!   nothing points to.
//! - [`History::recover`] runs on start: empty day folders go, and it returns the recordings
//!   without an entry (the app died while recording or transcribing), for the session to
//!   transcribe.

pub mod db;
pub mod recordings;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use self::db::{Db, Entry, EntryAudio};
use self::recordings::Recordings;

pub const DB_FILE: &str = "history.db";

/// Now as the history keeps time: Unix ms.
pub fn now_ms() -> i64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

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

pub struct History {
  pub db: Db,
  pub recordings: Recordings,
}

impl History {
  /// Opens (creating, migrating) the history in `home`.
  pub fn open(home: &Path) -> Result<Self> {
    let recordings = Recordings::open(home)?;
    let db = Db::open(&home.join(DB_FILE))?;
    Ok(Self { db, recordings })
  }

  /// The entry's recording, if it has one that is still on disk.
  pub fn audio_file(&self, entry: &Entry) -> Option<PathBuf> {
    let path = entry.audio_path.as_deref().and_then(|name| self.recordings.path(name));
    path.filter(|p| p.is_file())
  }

  /// Deletes entry `id` and its recording, the file first: if it cannot be deleted, the entry
  /// stays. Returns false if there is no such entry.
  pub fn delete(&self, id: i64) -> Result<bool> {
    let Some(entry) = self.db.get_entry(id)? else { return Ok(false) };
    let audio = entry.audio_path.as_deref();
    let removed = audio.and_then(|name| self.recordings.remove(name)).transpose()?;
    self.db.delete_entry(id)?;
    if let Some(path) = &removed {
      self.recordings.prune(path);
    }

    Ok(true)
  }

  /// Applies `retention` at `now` (Unix ms): removes older entries with their audio. An entry
  /// whose file cannot be deleted stays, to be tried again next time. Returns how many entries
  /// were removed.
  pub fn cleanup(&self, retention: Retention, now: i64) -> Result<usize> {
    let doomed: Vec<EntryAudio> = match (retention, retention.max_age()) {
      (Retention::Unlimited, _) => return Ok(0),
      (Retention::Count(n), _) => self.db.get_beyond_newest(n.max(1))?,
      (_, Some(age)) => self.db.get_created_before(now - age.as_millis() as i64)?,
      (_, None) => unreachable!("every other retention has an age"),
    };

    let mut removed = Vec::with_capacity(doomed.len());
    let mut emptied = HashSet::new();
    for (id, audio) in doomed {
      match audio.as_deref().and_then(|name| self.recordings.remove(name)) {
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

    self.db.delete_entries(&removed)?;
    for path in &emptied {
      self.recordings.prune(path);
    }

    Ok(removed.len())
  }

  /// Fixes up what a crash left behind (see the module docs) and returns the recordings without
  /// an entry, oldest first.
  pub fn recover(&self) -> Result<Vec<String>> {
    if let Err(e) = self.recordings.prune_all() {
      warn!("{e:#}");
    }

    let known = self.db.get_audio_paths()?;
    let mut orphans: Vec<String> =
      self.recordings.list()?.into_iter().filter(|name| !known.contains(name)).collect();
    orphans.sort();
    if !orphans.is_empty() {
      info!("history: {} recording(s) without an entry", orphans.len());
    }

    Ok(orphans)
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;
  use crate::audio::wav::WavSink;
  use crate::history::db::NewEntry;
  use crate::testing::{TempDir, entry_ids, wav_count, write_wav};

  const DAY: i64 = 24 * 60 * 60 * 1000;
  /// A fixed "now" for the fake clock: 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry with a real WAV file, `age` ms before [`NOW`].
  fn add(h: &History, age: i64) -> (i64, PathBuf) {
    let (name, path) = h.recordings.new_file(NOW - age);
    write_wav(&path, &[0.1; 1_600]);

    let id = h
      .db
      .insert_entry(&NewEntry {
        created_at: NOW - age,
        duration_ms: 100,
        audio_path: Some(&name),
        text: "текст",
        ..NewEntry::default()
      })
      .unwrap();
    (id, path)
  }

  /// The files and folders at the top of the recordings directory in `home`, and how many `.wav`
  /// files are there at any depth.
  fn top(home: &Path) -> (Vec<String>, usize) {
    let dir = home.join(recordings::DIR);
    let mut names: Vec<String> =
      fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    (names, wav_count(&dir))
  }

  #[test]
  fn a_day_folder_goes_with_its_last_recording() {
    let dir = TempDir::new("history-prune");
    let h = History::open(dir.path()).unwrap();
    // 7 minutes past a quarter hour: the same day in every time zone.
    let at = 7 * 60_000;
    let (a, a_wav) = add(&h, -at);
    let (b, b_wav) = add(&h, -at - 1);
    let day = a_wav.parent().unwrap().to_owned();
    assert_eq!(b_wav.parent(), Some(day.as_path()));
    fs::write(day.join(".DS_Store"), b"Finder").unwrap();
    assert!(h.delete(a).unwrap());
    assert!(day.is_dir(), "one recording is left");

    // A new recording of that day, its folder made; the day's last entry goes before the file is
    // created, and the recording still gets its folder.
    let (_, new_wav) = h.recordings.new_file(NOW + at + 10);
    assert_eq!(new_wav.parent(), Some(day.as_path()));
    assert!(h.delete(b).unwrap());
    assert!(!day.exists(), "Finder's file does not keep it");
    WavSink::create(&new_wav).unwrap().finalize().unwrap();
    fs::remove_file(&new_wav).unwrap();
    fs::remove_dir(&day).unwrap();

    // Retention too; the recordings directory itself stays.
    for i in 0..3 {
      add(&h, 10 * DAY + i);
    }

    let (_, kept) = add(&h, 0);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 3);
    let kept_day = kept.parent().unwrap().file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(top(dir.path()), (vec![kept_day], 1));
    fs::remove_file(&kept).unwrap();

    // An empty day folder left by a crash goes on start.
    assert!(h.recover().unwrap().is_empty());
    assert_eq!(top(dir.path()), (vec![], 0));
  }

  #[test]
  fn retention_by_count_keeps_the_newest() {
    let dir = TempDir::new("history-count");
    let h = History::open(dir.path()).unwrap();
    let entries: Vec<(i64, PathBuf)> = (0..5).map(|i| add(&h, (5 - i) * 1_000)).collect();
    assert_eq!(h.cleanup(Retention::Unlimited, NOW).unwrap(), 0);
    assert_eq!(h.cleanup(Retention::Count(2), NOW).unwrap(), 3);

    assert_eq!(entry_ids(dir.path()), [entries[3].0, entries[4].0]);
    for (i, (_, path)) in entries.iter().enumerate() {
      assert_eq!(path.exists(), i >= 3, "{}", path.display());
    }

    assert_eq!(top(dir.path()).1, 2);
    assert_eq!(h.cleanup(Retention::Count(2), NOW).unwrap(), 0, "idempotent");
    // Zero keeps one: the retention never empties the history entirely.
    assert_eq!(h.cleanup(Retention::Count(0), NOW).unwrap(), 1);
  }

  #[test]
  fn retention_by_age_with_a_fake_clock() {
    let dir = TempDir::new("history-age");
    let h = History::open(dir.path()).unwrap();
    let ages = [DAY, 3 * DAY - 1, 3 * DAY + 1, 13 * DAY, 15 * DAY, 89 * DAY, 91 * DAY];
    let entries: Vec<(i64, PathBuf)> = ages.iter().map(|&a| add(&h, a)).collect();
    let left = || entry_ids(dir.path()).len();

    assert_eq!(h.cleanup(Retention::Months3, NOW).unwrap(), 1);
    assert_eq!(left(), 6);
    assert!(!entries[6].1.exists());
    assert_eq!(h.cleanup(Retention::Weeks2, NOW).unwrap(), 2);
    assert_eq!(left(), 4);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 2);
    assert_eq!(entry_ids(dir.path()), [entries[0].0, entries[1].0]);
    assert_eq!(top(dir.path()).1, 2);
    assert!(entries[1].1.exists() && !entries[2].1.exists());

    // Time passes: a day later the 3-day-old entry goes too.
    assert_eq!(h.cleanup(Retention::Days3, NOW + DAY).unwrap(), 1);
    assert_eq!(entry_ids(dir.path()), [entries[0].0]);
  }

  #[test]
  fn an_entry_whose_audio_is_already_gone_is_removed() {
    let dir = TempDir::new("history-gone");
    let h = History::open(dir.path()).unwrap();
    let (_, path) = add(&h, 10 * DAY);
    fs::remove_file(&path).unwrap();
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 1);
    assert!(entry_ids(dir.path()).is_empty());
  }

  /// Recordings without an entry are found, the last failed one is not.
  #[test]
  fn recovery_finds_recordings_without_an_entry() {
    let dir = TempDir::new("history-recover");
    let h = History::open(dir.path()).unwrap();
    add(&h, DAY);
    let (failed, path) = h.recordings.new_file(NOW);
    write_wav(&path, &[0.1; 1_600]);
    h.recordings.keep_failed(&failed).unwrap();

    // A recording the app was killed during: written, no entry.
    let (name, path) = h.recordings.new_file(NOW + 5);
    WavSink::create(&path).unwrap().write(&[0.2; 1_600]).unwrap();
    assert_eq!(h.recover().unwrap(), [name]);
  }

  #[test]
  fn delete_takes_the_recording() {
    let dir = TempDir::new("history-delete");
    let h = History::open(dir.path()).unwrap();
    let (id, path) = add(&h, 0);
    let (other, other_path) = add(&h, 1_000);
    assert!(h.delete(id).unwrap());
    assert!(!path.exists() && other_path.exists());
    assert_eq!(h.db.get_entry(id).unwrap(), None);
    assert_eq!(entry_ids(dir.path()), [other]);
    assert!(!h.delete(id).unwrap(), "already gone");

    // A recording that is gone already does not keep the entry.
    fs::remove_file(&other_path).unwrap();
    assert!(h.delete(other).unwrap());
    assert!(entry_ids(dir.path()).is_empty());
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
