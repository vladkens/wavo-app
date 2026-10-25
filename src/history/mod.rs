// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation history in the data home: the entries in [`db`] (`history.db`, SQLite) and the
//! [`recordings`] they point to. [`History`] does what touches both.
//!
//! - An entry is a recording with speech, its text possibly empty: the text can always be made
//!   again from the audio. A dictation that is cancelled, has no speech or whose recording is not
//!   readable leaves nothing: no entry, no recording ([`Recordings::discard`]).
//! - [`History::cleanup`] applies the retention: entries and their audio files go together, the
//!   file first, so a crash in between leaves a row whose file is already gone, never a file
//!   nothing points to. Entries being transcribed are never removed.
//! - [`History::recover`] runs on start: the trash is emptied, entries left being transcribed by a
//!   crash are not any more (their text is empty or the one before), entries without text whose
//!   recording is gone go, recordings without an entry (the app died while recording or
//!   transcribing) become entries without text if they are readable and have speech, else they go,
//!   and empty day folders go. Nothing is transcribed then.

pub mod db;
pub mod recordings;

use std::collections::HashSet;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use self::db::{Db, Entry, EntryAudio, NewEntry};
use self::recordings::Recordings;
use crate::audio::vad::Clip;
use crate::audio::wav;

pub const DB_FILE: &str = "history.db";

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
  /// The calendar day: year, month, day.
  pub fn date(self) -> (i32, u32, u32) {
    (self.year, self.month, self.day)
  }

  /// `October`.
  pub fn month_name(self) -> &'static str {
    MONTHS[(self.month as usize).clamp(1, 12) - 1]
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

  // SAFETY: as above, Windows' C runtime's version.
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
  /// Entries left being transcribed.
  pub interrupted: usize,
  /// Entries without text whose recording is gone, deleted.
  pub dropped: usize,
  /// Recordings without an entry, now entries without text.
  pub adopted: usize,
  /// Recordings without an entry that are not readable or have no speech, deleted.
  pub discarded: usize,
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
    self.audio_path(entry.audio_path.as_deref())
  }

  fn audio_path(&self, name: Option<&str>) -> Option<PathBuf> {
    name.and_then(|name| self.recordings.path(name)).filter(|p| p.is_file())
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

  /// Applies `retention` at `now` (Unix ms): removes older entries with their audio. Entries
  /// without text stay when `spare`, and do not count: their audio is all there is of them,
  /// and a model may still make text of it. An entry whose file cannot be deleted stays, to be
  /// tried again next time. Returns how many entries were removed.
  pub fn cleanup(&self, retention: Retention, now: i64, spare: bool) -> Result<usize> {
    let doomed: Vec<EntryAudio> = match (retention, retention.max_age()) {
      (Retention::Unlimited, _) => return Ok(0),
      (Retention::Count(n), _) => self.db.get_beyond_newest(n.max(1), spare)?,
      (_, Some(age)) => self.db.get_created_before(now - age.as_millis() as i64, spare)?,
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

  /// Fixes up what a crash left behind (see the module docs); `now` stamps recordings whose start
  /// time is unknown.
  pub fn recover(&self, now: i64) -> Result<Recovered> {
    if let Err(e) = self.recordings.empty_trash() {
      warn!("{e:#}");
    }

    let interrupted = self.db.mark_interrupted()?;
    let gone: Vec<i64> = (self.db.get_untranscribed()?.into_iter())
      .filter(|(_, audio)| self.audio_path(audio.as_deref()).is_none())
      .map(|(id, _)| id)
      .collect();
    self.db.delete_entries(&gone)?;

    let known = self.db.get_audio_paths()?;
    let mut orphans: Vec<String> =
      self.recordings.list()?.into_iter().filter(|name| !known.contains(name)).collect();
    orphans.sort();

    let mut recovered = Recovered { interrupted, dropped: gone.len(), ..Recovered::default() };
    for name in &orphans {
      let clip =
        self.recordings.path(name).and_then(|path| wav::read(&path).ok()).map(Clip::analyze);
      let Some(clip) = clip.filter(Clip::has_speech) else {
        if let Err(e) = self.recordings.discard(name) {
          warn!("{e:#}");
        }

        recovered.discarded += 1;
        continue;
      };

      self.db.insert_entry(&NewEntry {
        created_at: recordings::started_at(name).unwrap_or(now),
        duration_ms: clip.duration_ms(),
        audio_path: Some(name),
        ..NewEntry::default()
      })?;
      recovered.adopted += 1;
    }

    if let Err(e) = self.recordings.prune_all() {
      warn!("{e:#}");
    }

    if recovered != Recovered::default() {
      info!("history recovered: {recovered:?}");
    }

    Ok(recovered)
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;
  use crate::audio::wav::WavSink;
  use crate::testing::{TempDir, entry_ids, tone, wav_count, write_wav};

  const DAY: i64 = 24 * 60 * 60 * 1000;
  /// A fixed "now" for the fake clock: 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry with `text` and a real WAV file, `age` ms before [`NOW`].
  fn add(h: &History, age: i64, text: &str) -> (i64, PathBuf) {
    add_with(h, age, NewEntry { text, ..NewEntry::default() })
  }

  /// [`add`] with the fields of `entry` but its time and recording.
  fn add_with(h: &History, age: i64, entry: NewEntry<'_>) -> (i64, PathBuf) {
    let (name, path) = h.recordings.new_file(NOW - age);
    write_wav(&path, &[0.1; 1_600]);
    let entry =
      NewEntry { created_at: NOW - age, duration_ms: 100, audio_path: Some(&name), ..entry };
    (h.db.insert_entry(&entry).unwrap(), path)
  }

  fn busy(h: &History, age: i64) -> (i64, PathBuf) {
    add_with(h, age, NewEntry { busy: true, ..NewEntry::default() })
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
    let (a, a_wav) = add(&h, -at, "a");
    let (b, b_wav) = add(&h, -at - 1, "b");
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
      add(&h, 10 * DAY + i, "old");
    }

    let (_, kept) = add(&h, 0, "new");
    assert_eq!(h.cleanup(Retention::Days3, NOW, false).unwrap(), 3);
    let kept_day = kept.parent().unwrap().file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(top(dir.path()), (vec![kept_day, "trash".into()], 1));
    fs::remove_file(&kept).unwrap();

    // An empty day folder left by a crash goes on start, as does the trash.
    assert_eq!(h.recover(NOW).unwrap(), Recovered::default());
    assert_eq!(top(dir.path()), (vec![], 0));
  }

  #[test]
  fn retention_by_count_keeps_the_newest() {
    let dir = TempDir::new("history-count");
    let h = History::open(dir.path()).unwrap();
    let entries: Vec<(i64, PathBuf)> = (0..5).map(|i| add(&h, (5 - i) * 1_000, "")).collect();
    // One being transcribed is neither removed nor counted.
    let (busy, _) = busy(&h, 10 * DAY);
    assert_eq!(h.cleanup(Retention::Unlimited, NOW, false).unwrap(), 0);
    assert_eq!(h.cleanup(Retention::Count(2), NOW, false).unwrap(), 3);

    assert_eq!(entry_ids(dir.path()), [entries[3].0, entries[4].0, busy]);
    for (i, (_, path)) in entries.iter().enumerate() {
      assert_eq!(path.exists(), i >= 3, "{}", path.display());
    }

    assert_eq!(top(dir.path()).1, 3);
    assert_eq!(h.cleanup(Retention::Count(2), NOW, false).unwrap(), 0, "idempotent");
    // Zero keeps one: the retention never empties the history entirely.
    assert_eq!(h.cleanup(Retention::Count(0), NOW, false).unwrap(), 1);
  }

  #[test]
  fn retention_by_age_with_a_fake_clock() {
    let dir = TempDir::new("history-age");
    let h = History::open(dir.path()).unwrap();
    let ages = [DAY, 3 * DAY - 1, 3 * DAY + 1, 13 * DAY, 15 * DAY, 89 * DAY, 91 * DAY];
    let entries: Vec<(i64, PathBuf)> = ages.iter().map(|&a| add(&h, a, "text")).collect();
    let (busy, busy_wav) = busy(&h, 400 * DAY);
    let left = || entry_ids(dir.path()).len() - 1;

    assert_eq!(h.cleanup(Retention::Months3, NOW, false).unwrap(), 1);
    assert_eq!(left(), 6);
    assert!(!entries[6].1.exists());
    assert_eq!(h.cleanup(Retention::Weeks2, NOW, false).unwrap(), 2);
    assert_eq!(left(), 4);
    assert_eq!(h.cleanup(Retention::Days3, NOW, false).unwrap(), 2);
    assert_eq!(entry_ids(dir.path()), [entries[0].0, entries[1].0, busy]);
    assert_eq!(top(dir.path()).1, 3);
    assert!(entries[1].1.exists() && !entries[2].1.exists() && busy_wav.exists());

    // Time passes: a day later the 3-day-old entry goes too.
    assert_eq!(h.cleanup(Retention::Days3, NOW + DAY, false).unwrap(), 1);
    assert_eq!(entry_ids(dir.path()), [entries[0].0, busy]);
  }

  /// While no model can transcribe, entries without text outlive the retention: their audio is all
  /// there is of them.
  #[test]
  fn retention_spares_entries_without_text_while_there_is_no_model() {
    let dir = TempDir::new("history-spare");
    let h = History::open(dir.path()).unwrap();
    let (text, _) = add(&h, 10 * DAY, "text");
    let (untranscribed, wav) = add(&h, 10 * DAY, "");
    assert_eq!(h.cleanup(Retention::Days3, NOW, true).unwrap(), 1);
    assert_eq!(entry_ids(dir.path()), [untranscribed]);
    assert!(wav.exists() && text > 0);
    assert_eq!(h.cleanup(Retention::Count(1), NOW, true).unwrap(), 0);
    assert_eq!(h.cleanup(Retention::Days3, NOW, false).unwrap(), 1);
    assert!(entry_ids(dir.path()).is_empty());
  }

  #[test]
  fn an_entry_whose_audio_is_already_gone_is_removed() {
    let dir = TempDir::new("history-gone");
    let h = History::open(dir.path()).unwrap();
    let (_, path) = add(&h, 10 * DAY, "text");
    fs::remove_file(&path).unwrap();
    assert_eq!(h.cleanup(Retention::Days3, NOW, false).unwrap(), 1);
    assert!(entry_ids(dir.path()).is_empty());
  }

  /// What a crash leaves: an entry being transcribed, recordings without an entry (with speech,
  /// without, unreadable), a file in the trash; and entries whose recording is gone.
  #[test]
  fn recovers_what_a_crash_left() {
    let dir = TempDir::new("history-recover");
    let h = History::open(dir.path()).unwrap();
    let (text, _) = add(&h, DAY, "text");
    let (stuck, _) = busy(&h, 0);
    let (gone, gone_wav) = add(&h, 2 * DAY, "");
    let (text_gone, text_gone_wav) = add(&h, 3 * DAY, "text");
    fs::remove_file(gone_wav).unwrap();
    fs::remove_file(text_gone_wav).unwrap();

    // A recording the app was killed during: 1.5 s written, no entry. A killed app leaves the
    // header at the last whole second; a drop fixes it up to 1.5 s.
    let (name, path) = h.recordings.new_file(NOW + 5);
    let mut sink = WavSink::create(&path).unwrap();
    sink.write(&tone(1.5, 0.5)).unwrap();
    drop(sink);
    let (_, silent) = h.recordings.new_file(NOW + 6);
    write_wav(&silent, &[0.0; 16_000]);
    let (_, broken) = h.recordings.new_file(NOW + 7);
    fs::write(&broken, b"RIFF....WAVE").unwrap();
    let trashed = dir.path().join(recordings::DIR).join("trash/1.wav");
    fs::create_dir_all(trashed.parent().unwrap()).unwrap();
    fs::write(&trashed, b"RIFF").unwrap();

    let recovered = Recovered { interrupted: 1, dropped: 1, adopted: 1, discarded: 2 };
    assert_eq!(h.recover(NOW + 10).unwrap(), recovered);
    assert!(!h.db.get_entry(stuck).unwrap().unwrap().busy);
    assert_eq!(h.db.get_entry(gone).unwrap(), None, "no text, no recording");
    assert_eq!(h.db.get_entry(text_gone).unwrap().unwrap().text, "text", "its text stays");
    let adopted = h.db.get_entry(*entry_ids(dir.path()).last().unwrap()).unwrap().unwrap();
    assert_eq!(adopted.audio_path.as_deref(), Some(name.as_str()));
    assert_eq!((adopted.created_at, adopted.text.as_str()), (NOW + 5, ""));
    assert_eq!((adopted.tried_model, adopted.busy), (None, false));
    assert!((1_000..=1_500).contains(&adopted.duration_ms), "{}", adopted.duration_ms);
    assert!(!silent.exists() && !broken.exists() && !trashed.exists());
    assert_eq!(entry_ids(dir.path()), [text, stuck, text_gone, adopted.id]);
    assert_eq!(h.recover(NOW + 20).unwrap(), Recovered::default(), "nothing left to fix");
  }

  #[test]
  fn delete_takes_the_recording() {
    let dir = TempDir::new("history-delete");
    let h = History::open(dir.path()).unwrap();
    let (id, path) = add(&h, 0, "text");
    let (other, other_path) = add(&h, 1_000, "");
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
