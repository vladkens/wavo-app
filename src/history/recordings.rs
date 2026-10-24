// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The recordings on disk: `recordings/YYYY-MM-DD/<unix ms>.wav` (16 kHz mono 16-bit, the whole
//! recording without VAD trimming), in a folder per local date of the recording's start: one flat
//! folder of a year's dictations is more than Finder copes with. A day folder goes with its last
//! recording.
//!
//! A dictation that fails leaves no entry; its recording is kept as the last failed one, in
//! `recordings/last-failed/`, to transcribe again: one recording, the newest failure's.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const DIR: &str = "recordings";
/// The folder of the last failed recording: not a day folder, so no entry names it.
const LAST_FAILED: &str = "last-failed";

pub struct Recordings {
  dir: PathBuf,
}

impl Recordings {
  /// The recordings directory in `home`, created if missing.
  pub fn open(home: &Path) -> Result<Self> {
    let dir = home.join(DIR);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(Self { dir })
  }

  /// Where a new recording started at `created_at` (Unix ms) goes, in its day folder (created
  /// here): its name as an entry keeps it and its full path. Never an existing file.
  pub fn new_file(&self, created_at: i64) -> (String, PathBuf) {
    self.new_file_besides(created_at, &HashSet::new())
  }

  /// [`new_file`](Self::new_file), not named `taken` either: names entries hold before their
  /// files are there (an import).
  pub fn new_file_besides(&self, created_at: i64, taken: &HashSet<String>) -> (String, PathBuf) {
    let day = day_of(created_at);
    if let Some(day) = &day {
      // A failure shows when the recording is created.
      let _ = fs::create_dir_all(self.dir.join(day));
    }

    let mut n = 0;
    loop {
      let file = if n == 0 { format!("{created_at}.wav") } else { format!("{created_at}-{n}.wav") };
      let name = day.as_ref().map_or_else(|| file.clone(), |day| format!("{day}/{file}"));
      let path = self.dir.join(&name);
      if !taken.contains(&name) && !path.exists() {
        return (name, path);
      }

      n += 1;
    }
  }

  /// Recording `name`'s path. Anything but a plain file name, in a day folder or not, is refused,
  /// so a damaged row cannot point outside.
  pub fn path(&self, name: &str) -> Option<PathBuf> {
    is_name(name).then(|| self.dir.join(name))
  }

  /// Deletes recording `name` (one gone already is no error) and returns where it was, leaving its
  /// day folder to [`prune`](Self::prune). `None` if the name is refused.
  pub fn remove(&self, name: &str) -> Option<Result<PathBuf>> {
    let path = self.path(name)?;
    match fs::remove_file(&path) {
      Ok(()) => Some(Ok(path)),
      Err(e) if e.kind() == ErrorKind::NotFound => Some(Ok(path)),
      Err(e) => Some(Err(e).with_context(|| format!("delete {}", path.display()))),
    }
  }

  /// Deletes recording `name` with its day folder if that is left empty.
  pub fn discard(&self, name: &str) -> Result<()> {
    let Some(removed) = self.remove(name) else { bail!("not a recording: {name:?}") };
    self.prune(&removed?);
    Ok(())
  }

  /// Removes the day folder of recording `path` if no recording is left in it.
  pub fn prune(&self, path: &Path) {
    match path.parent() {
      Some(dir) if dir != self.dir => prune_day(dir),
      _ => {}
    }
  }

  /// Removes every day folder without a recording (left by a crash).
  pub fn prune_all(&self) -> Result<()> {
    let dirs = fs::read_dir(&self.dir).with_context(|| format!("read {}", self.dir.display()))?;
    for e in dirs.flatten() {
      let day = e.file_name().to_str().is_some_and(is_day);
      if day && e.file_type().is_ok_and(|t| t.is_dir()) {
        prune_day(&e.path());
      }
    }

    Ok(())
  }

  /// Every recording, named as an entry names it.
  pub fn list(&self) -> Result<Vec<String>> {
    let wavs = |dir: &Path| -> Result<Vec<String>> {
      Ok(
        fs::read_dir(dir)
          .with_context(|| format!("read {}", dir.display()))?
          .flatten()
          .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
          .filter_map(|e| e.file_name().into_string().ok())
          .filter(|name| name.ends_with(".wav"))
          .collect(),
      )
    };

    let mut found = Vec::new();
    for e in
      fs::read_dir(&self.dir).with_context(|| format!("read {}", self.dir.display()))?.flatten()
    {
      let (Ok(name), Ok(kind)) = (e.file_name().into_string(), e.file_type()) else { continue };
      if kind.is_file() && name.ends_with(".wav") {
        found.push(name);
      } else if kind.is_dir() && is_day(&name) {
        found.extend(wavs(&e.path())?.into_iter().map(|file| format!("{name}/{file}")));
      }
    }

    Ok(found)
  }

  /// The last failed recording ([`keep_failed`](Self::keep_failed)) and when it started (Unix ms).
  pub fn last_failed(&self) -> Option<(PathBuf, i64)> {
    let files = fs::read_dir(self.dir.join(LAST_FAILED)).ok()?.flatten();
    let started = |e: fs::DirEntry| Some((started_at(e.file_name().to_str()?)?, e.path()));
    let (at, path) = files.filter_map(started).max()?;
    Some((path, at))
  }

  /// Keeps recording `name` as the last failed one, in place of the one before, unless that one
  /// is newer: the older of the two goes. Returns when the one kept started.
  pub fn keep_failed(&self, name: &str) -> Result<i64> {
    let Some(path) = self.path(name) else { bail!("not a recording: {name:?}") };
    let Some(at) = started_at(name) else { bail!("not a recording: {name:?}") };
    if let Some((_, newer)) = self.last_failed().filter(|&(_, kept)| kept > at) {
      self.discard(name)?;
      return Ok(newer);
    }

    let dir = self.dir.join(LAST_FAILED);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = path.file_name().expect("a recording's path names a file");
    let kept = dir.join(file);
    fs::rename(&path, &kept).with_context(|| format!("move {}", path.display()))?;
    self.prune(&path);
    for e in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?.flatten() {
      if e.path() != kept {
        let _ = fs::remove_file(e.path());
      }
    }

    Ok(at)
  }

  /// Moves the last failed recording back into its day folder, where an entry can name it, and
  /// returns its name there; `None` if there is none.
  pub fn restore_failed(&self) -> Result<Option<String>> {
    let Some((path, at)) = self.last_failed() else { return Ok(None) };
    let (name, to) = self.new_file(at);
    fs::rename(&path, &to).with_context(|| format!("move {}", path.display()))?;
    Ok(Some(name))
  }
}

/// Whether `name` is a recording's name, in a day folder or not: nothing that could point outside
/// the recordings directory.
pub fn is_name(name: &str) -> bool {
  match name.split_once('/') {
    Some((day, file)) => is_day(day) && is_plain(file),
    None => is_plain(name),
  }
}

/// Whether `name` is a plain file name: no folder, no `..`.
pub fn is_plain(name: &str) -> bool {
  Path::new(name).file_name().is_some_and(|n| n == name)
}

/// Start time (Unix ms) in recording `name`: `[YYYY-MM-DD/]<unix ms>[-<n>].wav`.
pub fn started_at(name: &str) -> Option<i64> {
  let file = name.rsplit('/').next()?;
  file.strip_suffix(".wav")?.split('-').next()?.parse().ok()
}

/// The day folder of a recording started at `ms` (Unix ms): its local date, `YYYY-MM-DD`.
pub fn day_of(ms: i64) -> Option<String> {
  let (year, month, day) = super::local_time(ms)?.date();
  Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// `YYYY-MM-DD`: the name of a day folder.
fn is_day(name: &str) -> bool {
  name.len() == 10
    && name
      .bytes()
      .enumerate()
      .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
}

/// Removes day folder `dir` if nothing but Finder's `.DS_Store` is in it.
fn prune_day(dir: &Path) {
  let Ok(entries) = fs::read_dir(dir) else { return };
  let mut ds_store = None;
  for e in entries.flatten() {
    if e.file_name() != ".DS_Store" {
      return;
    }

    ds_store = Some(e.path());
  }

  if let Some(file) = ds_store {
    let _ = fs::remove_file(file);
  }

  let _ = fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::TempDir;

  /// A fixed "now": 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  #[test]
  fn new_files_go_into_day_folders_and_never_over_a_recording() {
    let dir = TempDir::new("recordings-new");
    let r = Recordings::open(dir.path()).unwrap();
    let (name, path) = r.new_file(NOW);
    let day = day_of(NOW).unwrap();
    assert_eq!(name, format!("{day}/{NOW}.wav"));
    assert_eq!(path, dir.path().join(DIR).join(&day).join(format!("{NOW}.wav")));
    assert_eq!(r.path(&name), Some(path.clone()));
    fs::write(&path, b"").unwrap();
    assert_eq!(r.new_file(NOW).0, format!("{day}/{NOW}-1.wav"));
    let taken = HashSet::from([format!("{day}/{NOW}-1.wav")]);
    assert_eq!(r.new_file_besides(NOW, &taken).0, format!("{day}/{NOW}-2.wav"));
    assert_eq!(r.list().unwrap(), [name]);
  }

  #[test]
  fn paths_stay_inside_the_recordings_directory() {
    let dir = TempDir::new("recordings-paths");
    let r = Recordings::open(dir.path()).unwrap();
    for good in ["1.wav", "2026-10-09/1.wav", "2026-10-09/1-1.wav"] {
      assert_eq!(r.path(good), Some(r.dir.join(good)), "{good}");
    }

    for bad in [
      "../history.db",
      "/etc/passwd",
      "..",
      ".",
      "a/b.wav",
      "",
      "2026-10-09/../../history.db",
      "2026-10-09/..",
      "2026-10-09/",
      "2026-10-09/a/b.wav",
      "/2026-10-09/1.wav",
      "../2026-10-09/1.wav",
      "2026-1-09/1.wav",
      "2026-10-0x/1.wav",
      "2026/10/09/1.wav",
      "2026-10-09//1.wav",
    ] {
      assert_eq!(r.path(bad), None, "{bad}");
    }
  }

  #[test]
  fn day_folders_are_local_dates() {
    // 2026-10-09 00:00 UTC is the 8th or the 9th in any time zone.
    let day = day_of(NOW).unwrap();
    assert!(["2026-10-08", "2026-10-09"].contains(&day.as_str()), "{day}");
    assert!(is_day(&day));
    assert_eq!(day_of(NOW + 1), Some(day));
    assert_eq!(day_of(i64::MAX), None);
    for bad in ["2026-10-9", "2026-10-090", "2026_10_09", "２０２６-10-09", "abcd-ef-gh"] {
      assert!(!is_day(bad), "{bad}");
    }

    assert_eq!(started_at("1791504000000.wav"), Some(NOW));
    assert_eq!(started_at("2026-10-09/1791504000000-2.wav"), Some(NOW));
    assert_eq!((started_at("notes.wav"), started_at("1791504000000.txt")), (None, None));
  }

  /// One failed recording is kept, the newest; restored, it is a recording like any other.
  #[test]
  fn the_newest_failed_recording_is_kept() {
    let dir = TempDir::new("recordings-failed");
    let r = Recordings::open(dir.path()).unwrap();
    let record = |at: i64| {
      let (name, path) = r.new_file(at);
      fs::write(&path, b"RIFF").unwrap();
      name
    };

    assert_eq!(r.last_failed(), None);
    let (old, new) = (record(NOW), record(NOW + 1));
    assert_eq!(r.keep_failed(&new).unwrap(), NOW + 1);
    assert_eq!(r.keep_failed(&old).unwrap(), NOW + 1, "an older one goes");
    let kept = r.dir.join(LAST_FAILED).join(format!("{}.wav", NOW + 1));
    assert_eq!(r.last_failed(), Some((kept, NOW + 1)));
    assert!(r.list().unwrap().is_empty(), "no entry's recording, and no day folder left");
    assert_eq!(fs::read_dir(&r.dir).unwrap().count(), 1);

    let newest = record(NOW + 2);
    assert_eq!(r.keep_failed(&newest).unwrap(), NOW + 2, "a newer one takes its place");
    assert_eq!(r.last_failed().map(|(_, at)| at), Some(NOW + 2));
    assert_eq!(fs::read_dir(r.dir.join(LAST_FAILED)).unwrap().count(), 1, "the one before goes");
    assert!(r.keep_failed("../history.db").is_err());

    let name = r.restore_failed().unwrap().unwrap();
    assert_eq!((started_at(&name), r.list().unwrap()), (Some(NOW + 2), vec![name]));
    assert_eq!((r.last_failed(), r.restore_failed().unwrap()), (None, None));
  }

  #[test]
  fn a_discarded_recording_takes_its_day_folder() {
    let dir = TempDir::new("recordings-discard");
    let r = Recordings::open(dir.path()).unwrap();
    let (name, path) = r.new_file(NOW);
    fs::write(&path, b"RIFF").unwrap();
    fs::write(path.with_file_name(".DS_Store"), b"Finder").unwrap();
    r.discard(&name).unwrap();
    assert!(!path.exists() && !path.parent().unwrap().exists(), "Finder's file does not keep it");
    r.discard(&name).unwrap(); // gone already
    assert!(r.discard("../history.db").is_err());

    // A recording that was never made does not keep the folder made for it either; an empty one
    // left behind stays through a listing and goes with the explicit prune.
    let (name, path) = r.new_file(NOW);
    r.prune(&r.path(&name).unwrap());
    assert!(!path.parent().unwrap().exists());
    r.new_file(NOW);
    assert!(r.list().unwrap().is_empty());
    assert!(path.parent().unwrap().exists(), "a listing deletes nothing");
    fs::create_dir(r.dir.join("notes")).unwrap();
    r.prune_all().unwrap();
    assert_eq!(fs::read_dir(&r.dir).unwrap().count(), 1, "only the day folder goes");
  }
}
