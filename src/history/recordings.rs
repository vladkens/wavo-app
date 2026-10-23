// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The recordings on disk: `recordings/YYYY-MM-DD/<unix ms>.wav` (16 kHz mono 16-bit, the whole
//! recording without VAD trimming), in a folder per local date of the recording's start: one flat
//! folder of a year's dictations is more than Finder copes with. A day folder goes with its last
//! recording.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::{fs, mem};

use anyhow::{Context, Result, bail};

pub const DIR: &str = "recordings";

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
      if !path.exists() {
        return (name, path);
      }

      n += 1;
    }
  }

  /// Recording `name`'s path. Anything but a plain file name, in a day folder or not, is refused,
  /// so a damaged row cannot point outside.
  pub fn path(&self, name: &str) -> Option<PathBuf> {
    let plain = |name: &str| Path::new(name).file_name().is_some_and(|n| n == name);
    let inside = match name.split_once('/') {
      Some((day, file)) => is_day(day) && plain(file),
      None => plain(name),
    };
    inside.then(|| self.dir.join(name))
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

  /// How long recording `name` is by its header, 0 if it cannot be read.
  pub fn duration_ms(&self, name: &str) -> i64 {
    let Some(path) = self.path(name) else { return 0 };
    hound::WavReader::open(path)
      .map(|r| i64::from(r.duration()) * 1000 / i64::from(r.spec().sample_rate.max(1)))
      .unwrap_or(0)
  }
}

/// Start time (Unix ms) in recording `name`: `[YYYY-MM-DD/]<unix ms>[-<n>].wav`.
pub fn started_at(name: &str) -> Option<i64> {
  let file = name.rsplit('/').next()?;
  file.strip_suffix(".wav")?.split('-').next()?.parse().ok()
}

/// The day folder of a recording started at `ms` (Unix ms): its local date (the C runtime's time
/// zone), `YYYY-MM-DD`.
fn day_of(ms: i64) -> Option<String> {
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
  (1970..=9999).contains(&year).then(|| format!("{year:04}-{:02}-{:02}", tm.tm_mon + 1, tm.tm_mday))
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
