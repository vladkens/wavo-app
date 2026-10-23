// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation history in the data home: the entries in [`db`] (`history.db`, SQLite) and the
//! [`recordings`] they point to. [`History`] does what touches both.
//!
//! - It keeps what was transcribed (`done`, `partial`), what failed (`failed`: the audio is the
//!   only copy of the dictation, and it can still be transcribed) and speech no text came of
//!   (`no_text`, `no_model`: to try again). A cancelled dictation and one without speech leave
//!   nothing: no entry, no recording ([`Recordings::discard`]).
//! - [`History::cleanup`] applies the retention: entries and their audio files go together, the
//!   file first, so a crash in between leaves a row whose file is already gone, never a file
//!   nothing points to. Entries still being transcribed are never removed.
//! - [`History::recover`] runs on start: entries left `transcribing` by a crash become `failed`,
//!   and recordings without an entry (the app died while recording) get a `failed` entry, so their
//!   audio can be transcribed again and the retention covers them.

pub mod db;
pub mod recordings;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use self::db::{Db, Entry, NewEntry, Status, Victim};
use self::recordings::Recordings;

pub const DB_FILE: &str = "history.db";

/// The error of an entry [`History::recover`] made for a recording without one.
pub const STOPPED_RECORDING: &str =
  "Wavo stopped during the recording; the audio up to then is kept";

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

/// What [`History::recover`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovered {
  /// Entries left `transcribing`, now `failed`.
  pub failed: usize,
  /// Recordings without an entry, now `failed` entries.
  pub adopted: usize,
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
    let victims: Vec<Victim> = match (retention, retention.max_age()) {
      (Retention::Unlimited, _) => return Ok(0),
      (Retention::Count(n), _) => self.db.get_beyond_newest(n.max(1))?,
      (_, Some(age)) => self.db.get_created_before(now - age.as_millis() as i64)?,
      (_, None) => unreachable!("every other retention has an age"),
    };

    let mut removed = Vec::with_capacity(victims.len());
    let mut emptied = HashSet::new();
    for (id, audio) in victims {
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

  /// Fixes up what a crash left behind (see the module docs); `now` stamps recordings whose start
  /// time is unknown.
  pub fn recover(&self, now: i64) -> Result<Recovered> {
    let failed = self.db.mark_interrupted()?;

    let known = self.db.get_audio_paths()?;
    let mut orphans: Vec<String> =
      self.recordings.list()?.into_iter().filter(|name| !known.contains(name)).collect();
    orphans.sort();

    for name in &orphans {
      self.db.insert_entry(&NewEntry {
        created_at: recordings::started_at(name).unwrap_or(now),
        duration_ms: self.recordings.duration_ms(name),
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
}

#[cfg(test)]
mod tests {
  use std::fs;

  use rusqlite::Connection;

  use super::*;
  use crate::audio::wav::WavSink;
  use crate::history::db::STOPPED_TRANSCRIBING;
  use crate::testing::TempDir;

  const DAY: i64 = 24 * 60 * 60 * 1000;
  /// A fixed "now" for the fake clock: 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry with a real WAV file, `age` ms before [`NOW`].
  fn add(h: &History, age: i64, status: Status) -> (i64, PathBuf) {
    let (name, path) = h.recordings.new_file(NOW - age);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&[0.1; 1_600]).unwrap();
    sink.finalize().unwrap();

    let id = h
      .db
      .insert_entry(&NewEntry {
        created_at: NOW - age,
        duration_ms: 100,
        audio_path: Some(&name),
        status: Some(status),
        ..NewEntry::default()
      })
      .unwrap();
    (id, path)
  }

  /// Every entry's id in the history in `home`, in order.
  fn ids(home: &Path) -> Vec<i64> {
    let db = Connection::open(home.join(DB_FILE)).unwrap();
    let mut stmt = db.prepare("SELECT id FROM entries ORDER BY id").unwrap();
    stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
  }

  /// The files and folders at the top of the recordings directory in `home`, and how many `.wav`
  /// files are there at any depth.
  fn top(home: &Path) -> (Vec<String>, usize) {
    fn wavs(dir: &Path) -> usize {
      fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| match e.path() {
          path if path.is_dir() => wavs(&path),
          path => usize::from(path.extension().is_some_and(|x| x == "wav")),
        })
        .sum()
    }

    let dir = home.join(recordings::DIR);
    let mut names: Vec<String> =
      fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    (names, wavs(&dir))
  }

  /// Speech nothing was recognized in stays, with its recording, through a restart's recovery.
  #[test]
  fn nothing_recognized_is_kept_with_its_recording() {
    let dir = TempDir::new("history-no-text");
    let h = History::open(dir.path()).unwrap();
    let (id, wav) = add(&h, 0, Status::Transcribing);
    let note = "parakeet-tdt-0.6b-v2 understands English only";
    h.db.update_outcome(id, Status::NoText, None, Some(note)).unwrap();
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());

    let entry = h.db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.text.as_str()), (Status::NoText, ""));
    assert_eq!((entry.error.as_deref(), h.audio_file(&entry)), (Some(note), Some(wav)));
  }

  /// Speech recorded without a model stays, with its recording, through a restart's recovery.
  #[test]
  fn no_model_is_kept_with_its_recording() {
    let dir = TempDir::new("history-no-model");
    let h = History::open(dir.path()).unwrap();
    let (id, wav) = add(&h, 0, Status::NoModel);
    let why = "parakeet-tdt-0.6b-v3 is not on disk";
    h.db.update_outcome(id, Status::NoModel, None, Some(why)).unwrap();
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());

    let entry = h.db.get_entry(id).unwrap().unwrap();
    assert_eq!((entry.status, entry.error.as_deref()), (Status::NoModel, Some(why)));
    assert_eq!(h.audio_file(&entry), Some(wav));
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
    let (_, new_wav) = h.recordings.new_file(NOW + at + 10);
    assert_eq!(new_wav.parent(), Some(day.as_path()));
    assert!(h.delete(b).unwrap());
    assert!(!day.exists(), "Finder's file does not keep it");
    WavSink::create(&new_wav).unwrap().finalize().unwrap();
    fs::remove_file(&new_wav).unwrap();
    fs::remove_dir(&day).unwrap();

    // Retention too; the recordings directory itself stays.
    for i in 0..3 {
      add(&h, 10 * DAY + i, Status::Done);
    }

    let (_, kept) = add(&h, 0, Status::Done);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 3);
    let kept_day = kept.parent().unwrap().file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(top(dir.path()), (vec![kept_day], 1));
    fs::remove_file(&kept).unwrap();

    // An empty day folder left by a crash goes on start.
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());
    assert_eq!(top(dir.path()), (vec![], 0));
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

    assert_eq!(ids(dir.path()), [entries[3].0, entries[4].0, busy]);
    for (i, (_, path)) in entries.iter().enumerate() {
      assert_eq!(path.exists(), i >= 3, "{}", path.display());
    }

    assert_eq!(top(dir.path()).1, 3);
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
    let left = || ids(dir.path()).len() - 1;

    assert_eq!(h.cleanup(Retention::Months3, NOW).unwrap(), 1);
    assert_eq!(left(), 6);
    assert!(!entries[6].1.exists());
    assert_eq!(h.cleanup(Retention::Weeks2, NOW).unwrap(), 2);
    assert_eq!(left(), 4);
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 2);
    assert_eq!(ids(dir.path()), [entries[0].0, entries[1].0, busy]);
    assert_eq!(top(dir.path()).1, 3);
    assert!(entries[1].1.exists() && !entries[2].1.exists() && busy_wav.exists());

    // Time passes: a day later the 3-day-old entry goes too.
    assert_eq!(h.cleanup(Retention::Days3, NOW + DAY).unwrap(), 1);
    assert_eq!(ids(dir.path()), [entries[0].0, busy]);
  }

  #[test]
  fn an_entry_whose_audio_is_already_gone_is_removed() {
    let dir = TempDir::new("history-gone");
    let h = History::open(dir.path()).unwrap();
    let (_, path) = add(&h, 10 * DAY, Status::Done);
    fs::remove_file(&path).unwrap();
    assert_eq!(h.cleanup(Retention::Days3, NOW).unwrap(), 1);
    assert!(ids(dir.path()).is_empty());
  }

  #[test]
  fn recovers_interrupted_transcriptions_and_orphan_recordings() {
    let dir = TempDir::new("history-recover");
    let h = History::open(dir.path()).unwrap();
    let (done, _) = add(&h, DAY, Status::Done);
    let (stuck, _) = add(&h, 0, Status::Transcribing);

    // A recording the app was killed during: 1.5 s written, no entry. A killed app leaves the
    // header at the last whole second; a drop fixes it up to 1.5 s.
    let (name, path) = h.recordings.new_file(NOW + 5);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&[0.2; 24_000]).unwrap();
    drop(sink);
    assert_eq!(h.recover(NOW + 10).unwrap(), Recovered { failed: 1, adopted: 1 });

    assert_eq!(h.db.get_entry(done).unwrap().unwrap().status, Status::Done);
    let stuck = h.db.get_entry(stuck).unwrap().unwrap();
    assert_eq!(
      (stuck.status, stuck.error.as_deref()),
      (Status::Failed, Some(STOPPED_TRANSCRIBING))
    );

    let adopted = h.db.get_entry(*ids(dir.path()).last().unwrap()).unwrap().unwrap();
    assert_eq!(adopted.audio_path.as_deref(), Some(name.as_str()));
    assert_eq!((adopted.created_at, adopted.status), (NOW + 5, Status::Failed));
    assert_eq!(adopted.error.as_deref(), Some(STOPPED_RECORDING));
    assert!((1_000..=1_500).contains(&adopted.duration_ms), "{}", adopted.duration_ms);
    assert_eq!(h.recover(NOW + 20).unwrap(), Recovered::default(), "nothing left to fix");
  }

  #[test]
  fn delete_takes_the_recording() {
    let dir = TempDir::new("history-delete");
    let h = History::open(dir.path()).unwrap();
    let (id, path) = add(&h, 0, Status::Done);
    let (other, other_path) = add(&h, 1_000, Status::Done);
    assert!(h.delete(id).unwrap());
    assert!(!path.exists() && other_path.exists());
    assert_eq!(h.db.get_entry(id).unwrap(), None);
    assert_eq!(ids(dir.path()), [other]);
    assert!(!h.delete(id).unwrap(), "already gone");

    // A recording that is gone already does not keep the entry.
    fs::remove_file(&other_path).unwrap();
    assert!(h.delete(other).unwrap());
    assert!(ids(dir.path()).is_empty());
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
