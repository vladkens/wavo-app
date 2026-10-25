// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Moving the history between computers ([`archive`]: `wavo-app export`, `import <file>`) and
//! taking it over from another dictation app ([`handy`]: `wavo-app import handy`).
//!
//! An import is the same whatever the source:
//!
//! - Every entry with a recording comes over, its text or not (text can be made again from the
//!   audio), and every entry with text; one with neither is left. One is in the history already
//!   when an entry there started at the same time (Unix ms: one dictation's, whatever its text
//!   became when transcribed again), so importing twice adds nothing. None comes being transcribed.
//! - The new entries go in first, in one transaction, each naming the recording it will have; their
//!   recordings are copied after, each through a `.part` file renamed into place. An import cut
//!   short leaves entries whose recording is not there yet: the next one finds them by their start
//!   and copies what is missing, to the names they hold.
//! - The new entries are busy until their recordings are copied: the retention and the session
//!   leave them alone meanwhile (a crash leaves them to the next start's recovery).
//! - It holds the app's single-instance lock ([`ctl::claim`]): it refuses while Wavo runs, and Wavo
//!   does not start while it imports. A dry run reads the history read-only and takes no lock; the
//!   running app, which holds the lock, imports beside its session ([`Lock::Held`]). One import at
//!   a time goes into a history ([`Running`]).

pub mod archive;
pub mod handy;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::ctl::{self, Claim};
use crate::history::db::{Db, Entry};
use crate::history::recordings::{self, Recordings};
use crate::history::{DB_FILE, History};

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

impl Lock {
  fn new(dry_run: bool) -> Self {
    if dry_run { Lock::DryRun } else { Lock::Take }
  }
}

/// An entry of another history, and where its recording is there.
struct Item<S> {
  /// Its id and recording are not used: the recording's name in this history is given on import.
  entry: Entry,
  audio: Option<S>,
}

/// What an import did, or would do in a dry run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
  /// Entries in the source: with a recording or text.
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
/// whose entries are `starts` ([`Db::get_starts`]), its recordings on disk `on_disk`. A recording
/// two items name goes with the first; an item with neither text nor a recording then, or one
/// starting when one before it did, is left out.
fn plan<S: Eq + Hash + Clone>(
  starts: &HashMap<i64, Option<String>>,
  items: Vec<Item<S>>,
  on_disk: &HashSet<String>,
) -> Plan<S> {
  let (mut seen, mut audio_seen) = (HashSet::new(), HashSet::new());
  let mut plan = Plan { entries: 0, new: Vec::new(), missing: Vec::new() };
  for Item { entry, audio } in items {
    if seen.contains(&entry.created_at) {
      continue;
    }

    let audio = audio.filter(|a| audio_seen.insert(a.clone()));
    if entry.text.trim().is_empty() && audio.is_none() {
      continue;
    }

    seen.insert(entry.created_at);

    plan.entries += 1;
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
  /// The entries added, busy until the import ends.
  added: Vec<i64>,
  _lock: Option<ctl::Server>,
  _running: Option<Running>,
}

/// The data homes this process imports into, each by one import at a time.
static RUNNING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// An import into a home, until dropped.
struct Running(PathBuf);

impl Running {
  fn start(home: &Path) -> Result<Self> {
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if running.iter().any(|h| h == home) {
      bail!("an import into {} is running already", home.display());
    }

    running.push(home.to_owned());
    Ok(Self(home.to_owned()))
  }
}

impl Drop for Running {
  fn drop(&mut self) {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).retain(|h| *h != self.0);
  }
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
      return Ok(Self {
        history: None,
        starts,
        on_disk,
        added: Vec::new(),
        _lock: None,
        _running: None,
      });
    }

    let running = Running::start(home)?;
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
    let (history, added) = (Some(history), Vec::new());
    Ok(Self { history, starts, on_disk, added, _lock: lock, _running: Some(running) })
  }

  fn plan<S: Eq + Hash + Clone>(&self, items: Vec<Item<S>>) -> Plan<S> {
    plan(&self.starts, items, &self.on_disk)
  }

  /// Adds the planned entries in one transaction, busy until the import ends, and returns the
  /// recordings to copy, each with the path it goes to (its day folder made).
  fn add<S>(&mut self, plan: Plan<S>) -> Result<Vec<(S, PathBuf)>> {
    let history = self.history.as_ref().context("a dry run adds nothing")?;
    let mut taken: HashSet<String> = self.starts.values().flatten().cloned().collect();
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

    self.added = history.db.insert_imported(&entries)?;
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

impl Drop for Target {
  /// The import ended, done or not: its entries are not busy any more.
  fn drop(&mut self) {
    if let Some(history) = &self.history
      && let Err(e) = history.db.mark_imported(&self.added)
    {
      warn!("history: {e:#}");
    }
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

pub fn part_of(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
  use super::*;

  fn item(at: i64, text: &str, audio: Option<&'static str>) -> Item<&'static str> {
    Item { entry: Entry { created_at: at, text: text.into(), ..Entry::default() }, audio }
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
      item(5, "  ", Some("src-5")), // new, no text: its recording has it
      item(6, "e", Some("src-6")),  // new
      item(7, "f", Some("src-6")),  // new, naming a recording another one did
      item(8, "", None),            // neither text nor a recording
      item(9, "", Some("src-6")),   // no text, and its recording is another one's
    ];
    let plan = plan(&starts, items, &HashSet::from(["1.wav".to_owned()]));
    assert_eq!(plan.entries, 7);
    let new: Vec<_> = plan.new.iter().map(|x| (x.entry.created_at, x.audio)).collect();
    assert_eq!(new, [(4, None), (5, Some("src-5")), (6, Some("src-6")), (7, None)]);
    assert_eq!(plan.missing, [("src-2", "2.wav".to_owned())]);
    assert_eq!(plan.copies(), 3);
  }
}
