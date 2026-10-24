// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Moving the history between computers ([`archive`]: `wavo-app export`, `import <file>`) and
//! taking it over from another dictation app ([`handy`]: `wavo-app import handy`).
//!
//! An import is the same whatever the source:
//!
//! - Only entries with text come over; one is in the history already when an entry there started at
//!   the same time (Unix ms: one dictation's, whatever its text became when transcribed again), so
//!   importing twice adds nothing.
//! - The new entries go in first, in one transaction, each naming the recording it will have; their
//!   recordings are copied after, each through a `.part` file renamed into place. An import cut
//!   short leaves entries whose recording is not there yet: the next one finds them by their start
//!   and copies what is missing, to the names they hold.
//! - It holds the app's single-instance lock ([`ctl::claim`]): it refuses while Wavo runs, and Wavo
//!   does not start while it imports. A dry run reads the history read-only and takes no lock; the
//!   running app, which holds the lock, imports as it is ([`Lock::Held`]).

/// How an import goes with the single-instance lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
  /// A dry run: no lock, nothing written.
  DryRun,
  /// It takes the lock (the command line), refused while Wavo runs.
  Take,
  /// The caller holds it: the running app.
  Held,
}

pub mod archive;
pub mod handy;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::ctl::{self, Claim};
use crate::history::db::{Db, Entry};
use crate::history::recordings::{self, Recordings};
use crate::history::{DB_FILE, History};

/// An entry of another history, and where its recording is there.
struct Item<S> {
  /// Its id and recording are not used: the recording's name in this history is given on import.
  entry: Entry,
  audio: Option<S>,
}

/// What an import did, or would do in a dry run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
  /// Entries with text in the source.
  pub entries: usize,
  /// Those that were not in the history.
  pub added: usize,
  /// Recordings copied: the added entries' and those an import cut short left out.
  pub copied: usize,
  /// Recordings that could not be copied (or were not in the source): their entries are without
  /// one; the next import tries again.
  pub failed: usize,
}

impl Report {
  /// Counts the copy of recording `dst`.
  fn count(&mut self, copy: io::Result<()>, dst: &Path) {
    match copy {
      Ok(()) => self.copied += 1,
      Err(e) => {
        warn!("{}: {e}", dst.display());
        self.failed += 1;
      }
    }
  }
}

/// What [`plan`] makes of a source's items against the history.
struct Plan<S> {
  entries: usize,
  /// Not in the history yet.
  new: Vec<Item<S>>,
  /// Recordings of entries imported before that are not on disk, and the names they hold.
  missing: Vec<(S, String)>,
}

impl<S> Plan<S> {
  /// The recordings an import copies.
  fn copies(&self) -> usize {
    self.new.iter().filter(|x| x.audio.is_some()).count() + self.missing.len()
  }

  /// The report before any copy; a dry run's counts the copies it would make.
  fn report(&self, dry_run: bool) -> Report {
    let copied = if dry_run { self.copies() } else { 0 };
    Report { entries: self.entries, added: self.new.len(), copied, failed: 0 }
  }
}

/// Splits `items` into the new ones and the recordings missing for those already in the history,
/// whose entries are `starts` ([`Db::get_starts`]), its recordings on disk `on_disk`. An item
/// without text, or one starting when one before it did, is left out; a recording two items name
/// goes with the first.
fn plan<S: Eq + Hash + Clone>(
  starts: &HashMap<i64, Option<String>>,
  items: Vec<Item<S>>,
  on_disk: &HashSet<String>,
) -> Plan<S> {
  let (mut seen, mut audio_seen) = (HashSet::new(), HashSet::new());
  let mut plan = Plan { entries: 0, new: Vec::new(), missing: Vec::new() };
  for Item { entry, audio } in items {
    if entry.text.trim().is_empty() || !seen.insert(entry.created_at) {
      continue;
    }

    plan.entries += 1;
    let audio = audio.filter(|a| audio_seen.insert(a.clone()));
    match (starts.get(&entry.created_at), audio) {
      (None, audio) => plan.new.push(Item { entry, audio }),
      (Some(Some(name)), Some(audio)) if !on_disk.contains(name) => {
        plan.missing.push((audio, name.clone()))
      }
      (Some(_), _) => {}
    }
  }

  plan
}

/// The history an import goes into, under the app's single-instance lock (see the module docs);
/// a dry run only reads it, if there is one, without the lock.
struct Target {
  /// `None` in a dry run.
  history: Option<History>,
  /// What [`plan`] needs of it.
  starts: HashMap<i64, Option<String>>,
  on_disk: HashSet<String>,
  _lock: Option<ctl::Server>,
}

impl Target {
  fn open(home: &Path, lock: Lock) -> Result<Self> {
    if lock == Lock::DryRun {
      let db = home.join(DB_FILE);
      let starts = match db.is_file() {
        true => Db::open_read_only(&db)?.get_starts()?,
        false => HashMap::new(),
      };
      let on_disk = match home.join(recordings::DIR).is_dir() {
        true => Recordings::open(home)?.list()?.into_iter().collect(),
        false => HashSet::new(),
      };
      return Ok(Self { history: None, starts, on_disk, _lock: None });
    }

    fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    let lock = match lock {
      Lock::Held => None,
      _ => match ctl::claim(home)? {
        Claim::Server(lock) => Some(lock),
        Claim::Running => bail!("Wavo is running for {}: quit it first", home.display()),
      },
    };

    let history = History::open(home)?;
    let starts = history.db.get_starts()?;
    let on_disk = history.recordings.list()?.into_iter().collect();
    Ok(Self { history: Some(history), starts, on_disk, _lock: lock })
  }

  fn plan<S: Eq + Hash + Clone>(&self, items: Vec<Item<S>>) -> Plan<S> {
    plan(&self.starts, items, &self.on_disk)
  }

  /// Adds the planned entries in one transaction and returns the recordings to copy, each with the
  /// path it goes to (its day folder made).
  fn add<S>(&self, plan: Plan<S>) -> Result<Vec<(S, PathBuf)>> {
    let history = self.history.as_ref().context("a dry run adds nothing")?;
    let mut taken = history.db.get_audio_paths()?;
    let mut entries = Vec::with_capacity(plan.new.len());
    let mut copies = Vec::with_capacity(plan.copies());
    let mut progress = Progress::new("adding entries", plan.new.len());
    for Item { mut entry, audio } in plan.new {
      if let Some(audio) = audio {
        let (name, path) = history.recordings.new_file_besides(entry.created_at, &taken);
        taken.insert(name.clone());
        entry.audio_path = Some(name);
        copies.push((audio, path));
      }

      entries.push(entry);
      progress.tick();
    }

    history.db.insert_imported(&entries)?;
    progress.finish();

    // Their day folders may have gone empty since (a start of Wavo prunes them).
    for (audio, name) in plan.missing {
      let Some(path) = history.recordings.path(&name) else { continue };
      if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
      }

      copies.push((audio, path));
    }

    Ok(copies)
  }
}

/// Makes the file `dst` with `write` through `<dst>.part` renamed into place, so a file cut short
/// is never taken for it; a `.part` an earlier one left is replaced.
fn via_part<E: From<io::Error>>(
  dst: &Path,
  write: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<(), E> {
  let part = part_of(dst);
  let _ = fs::remove_file(&part);
  let done = write(&part).and_then(|()| Ok(fs::rename(&part, dst)?));
  if done.is_err() {
    let _ = fs::remove_file(&part);
  }

  done
}

/// Writes the `len` bytes of `src` into recording `dst` (see [`via_part`]); fewer (an archive cut
/// short) are an error, not a recording. Not synced: a full sync per file would take minutes for a
/// big history.
fn write_recording(dst: &Path, src: &mut impl Read, len: u64) -> io::Result<()> {
  via_part(dst, |part| match io::copy(src, &mut File::create(part)?)? {
    n if n == len => Ok(()),
    n => Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{n} of {len} bytes"))),
  })
}

/// Copies the file `src` into recording `dst` (see [`via_part`]); on APFS it is a clone, taking no
/// space.
fn copy_recording(dst: &Path, src: &Path) -> io::Result<()> {
  via_part(dst, |part| fs::copy(src, part).map(drop))
}

fn part_of(path: &Path) -> PathBuf {
  let mut name = path.file_name().unwrap_or_default().to_owned();
  name.push(".part");
  path.with_file_name(name)
}

/// A directory for an import's or export's own files, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
  fn new() -> Result<Self> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wavo-transfer-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(Self(dir))
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// `n of total` on one line of stderr, redrawn a few times a second while it is a terminal; the
/// last count stays.
struct Progress {
  what: &'static str,
  total: usize,
  done: usize,
  shown: Option<Instant>,
  tty: bool,
}

impl Progress {
  const EVERY: Duration = Duration::from_millis(100);

  fn new(what: &'static str, total: usize) -> Self {
    Self { what, total, done: 0, shown: None, tty: io::stderr().is_terminal() }
  }

  fn tick(&mut self) {
    self.done += 1;
    if self.tty && self.shown.is_none_or(|t| t.elapsed() >= Self::EVERY) {
      self.shown = Some(Instant::now());
      eprint!("\r{} {}/{}", self.what, self.done, self.total);
    }
  }

  fn finish(self) {
    if self.tty && self.total > 0 {
      eprintln!("\r{} {}/{}", self.what, self.done, self.total);
    }
  }
}

/// `bytes` as a person reads a size: `12.3 GB`.
pub fn size(bytes: u64) -> String {
  let units = ["B", "KB", "MB", "GB", "TB"];
  let mut n = bytes as f64;
  let mut unit = 0;
  while n >= 1000.0 && unit < units.len() - 1 {
    n /= 1000.0;
    unit += 1;
  }

  if unit == 0 { format!("{bytes} B") } else { format!("{n:.1} {}", units[unit]) }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn item(at: i64, text: &str, audio: Option<&'static str>) -> Item<&'static str> {
    let entry = Entry {
      id: 0,
      created_at: at,
      duration_ms: 0,
      audio_path: None,
      model: None,
      language: None,
      text: text.into(),
      pasted: false,
      not_pasted: None,
    };
    Item { entry, audio }
  }

  #[test]
  fn a_plan_takes_new_entries_and_missing_recordings() {
    let starts =
      HashMap::from([(1, Some("1.wav".to_owned())), (2, Some("2.wav".to_owned())), (3, None)]);
    let items = vec![
      item(1, "a", Some("src-1")),  // there, its recording too
      item(2, "b", Some("src-2")),  // there, its recording missing
      item(3, "c", Some("src-3")),  // there without a recording: left so
      item(1, "a2", Some("src-4")), // the same start, another text: the same dictation
      item(4, "d", None),           // new, no recording
      item(4, "d", None),           // the source's twice
      item(5, "  ", Some("src-5")), // no text
      item(6, "e", Some("src-6")),  // new
      item(7, "f", Some("src-6")),  // new, naming a recording another one did
    ];
    let plan = plan(&starts, items, &HashSet::from(["1.wav".to_owned()]));
    assert_eq!(plan.entries, 6);
    let new: Vec<_> = plan.new.iter().map(|x| (x.entry.created_at, x.audio)).collect();
    assert_eq!(new, [(4, None), (6, Some("src-6")), (7, None)]);
    assert_eq!(plan.missing, [("src-2", "2.wav".to_owned())]);
    assert_eq!(plan.copies(), 2);
  }

  #[test]
  fn sizes_read_as_a_person_reads_them() {
    assert_eq!(size(512), "512 B");
    assert_eq!(size(1_500), "1.5 KB");
    assert_eq!(size(12_300_000_000), "12.3 GB");
  }
}
