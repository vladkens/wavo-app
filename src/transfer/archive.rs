// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A Wavo archive, to move the history to another computer: one uncompressed tar (recordings do
//! not compress), read and written as a stream, in this order:
//!
//! - `wavo-export.json`: the [`Manifest`], first, so anything else is refused at once;
//! - `settings.json`, if there are settings;
//! - `history.db`: a consistent copy of the history (`VACUUM INTO`, while the app may run);
//! - `recordings/<day>/<file>.wav`: the recordings of the entries, as they name them.
//!
//! An import takes the history's entries (see the [module](super) docs) and the
//! recordings they name; the settings only when asked.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Item, Progress, Report, Scratch, Target};
use crate::history::db::{self, Db};
use crate::history::recordings::{self, Recordings};
use crate::history::{DB_FILE, now_ms};
use crate::settings::{self, Settings};

/// The layout an archive has; one from a newer Wavo is refused.
pub const FORMAT: u32 = 1;
const MANIFEST: &str = "wavo-export.json";
/// The manifest is a few lines; anything this long is not one.
const MAX_MANIFEST: u64 = 64 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
  pub format: u32,
  /// The Wavo that wrote it.
  pub version: String,
  /// Unix ms.
  pub created_at: i64,
  /// Entries, and the recordings in the archive.
  pub entries: usize,
  pub recordings: usize,
}

/// What [`export`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exported {
  pub entries: usize,
  pub recordings: usize,
  /// The archive's size.
  pub bytes: u64,
}

/// What [`import`] did, or would do in a dry run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
  pub report: Report,
  /// The archive's settings replaced these.
  pub settings: bool,
}

/// Writes the settings, the history and its recordings in `home` into a new archive `file`,
/// through `<file>.part`: a cut export leaves no archive.
pub fn export(home: &Path, file: &Path) -> Result<Exported> {
  if file.exists() {
    bail!("{} exists", file.display());
  }

  let db = home.join(DB_FILE);
  if !db.is_file() {
    bail!("no history in {}", home.display());
  }

  // A copy as of now while the app may be writing; the app's database is only read.
  let scratch = Scratch::new()?;
  let snapshot = scratch.0.join(DB_FILE);
  let conn = db::read_only(&db)?;
  let target = snapshot.to_str().context("the temp dir's path is not UTF-8")?;
  conn.execute("VACUUM INTO ?1", [target]).with_context(|| format!("copy {}", db.display()))?;
  drop(conn);

  // The recordings of the entries that are on disk; the copy names no other.
  let copy = Db::open(&snapshot)?;
  let all = copy.get_entries()?;
  let entries = all.len();
  let on_disk: HashSet<String> = Recordings::open(home)?.list()?.into_iter().collect();
  let names: Vec<String> =
    all.into_iter().filter_map(|e| e.audio_path).filter(|n| on_disk.contains(n)).collect();
  let kept: HashSet<&str> = names.iter().map(String::as_str).collect();
  let mut left_out = copy.get_audio_paths()?;
  left_out.retain(|name| !kept.contains(name.as_str()));
  copy.update_without_audio(&left_out)?;
  drop(copy);

  let manifest = Manifest {
    format: FORMAT,
    version: env!("CARGO_PKG_VERSION").into(),
    created_at: now_ms(),
    entries,
    recordings: names.len(),
  };

  let dir = home.join(recordings::DIR);
  let mut written = 0;
  super::via_part(file, |part| -> Result<()> {
    let out = File::create(part).with_context(|| format!("create {}", part.display()))?;
    let mut tar = tar::Builder::new(BufWriter::new(out));
    let json = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(manifest.created_at as u64 / 1000);
    tar.append_data(&mut header, MANIFEST, json.as_slice())?;

    let settings = home.join(settings::FILE);
    if settings.is_file() {
      tar.append_path_with_name(&settings, settings::FILE)?;
    }

    tar.append_path_with_name(&snapshot, DB_FILE)?;

    let mut progress = Progress::new("writing recordings", names.len());
    for name in &names {
      // One deleted since the listing is left out (an import counts it as not copied).
      match File::open(dir.join(name)) {
        Ok(mut wav) => {
          tar.append_file(format!("{}/{name}", recordings::DIR), &mut wav)?;
          written += 1;
        }
        Err(e) => warn!("{name}: {e}; left out"),
      }

      progress.tick();
    }

    progress.finish();
    let out = tar.into_inner()?.into_inner().map_err(|e| e.into_error())?;
    out.sync_all()?;
    Ok(())
  })?;

  let bytes = fs::metadata(file)?.len();
  Ok(Exported { entries: manifest.entries, recordings: written, bytes })
}

/// Imports the archive `file` into `home` (see the [module](super) docs); `take_settings` replaces
/// the settings with the archive's. A dry run says what it would do and writes nothing.
pub fn import(home: &Path, file: &Path, take_settings: bool, dry_run: bool) -> Result<Imported> {
  let lock = if dry_run { super::Lock::DryRun } else { super::Lock::Take };
  let target = Target::open(home, lock)?;
  let scratch = Scratch::new()?;
  let reader = File::open(file).with_context(|| format!("open {}", file.display()))?;
  let mut archive = tar::Archive::new(BufReader::new(reader));
  let mut entries = archive.entries().with_context(|| format!("read {}", file.display()))?;
  let not_ours = || format!("{} is not an archive of `wavo-app export`", file.display());

  let mut first = entries.next().with_context(not_ours)?.with_context(not_ours)?;
  if first.path()?.to_str() != Some(MANIFEST) {
    bail!(not_ours());
  }

  let mut json = Vec::new();
  (&mut first).take(MAX_MANIFEST).read_to_end(&mut json)?;
  let manifest: Manifest = serde_json::from_slice(&json).with_context(not_ours)?;
  if manifest.format > FORMAT {
    bail!("{} is from a newer Wavo ({}): update this one first", file.display(), manifest.version);
  }

  let mut settings = None;
  // The recordings to copy, by their names in the archive, once its history is read.
  let mut copies: Option<HashMap<String, PathBuf>> = None;
  let mut report = Report::default();
  let mut progress = Progress::new("copying recordings", 0);
  for entry in entries {
    let mut entry = entry?;
    let name = entry.path()?.to_str().with_context(not_ours)?.to_owned();
    match name.as_str() {
      settings::FILE => {
        let path = scratch.0.join(settings::FILE);
        io::copy(&mut entry, &mut File::create(&path)?)?;
        let (taken, warning) = Settings::load(&path);
        // Before anything is written: settings this Wavo cannot use do not replace these.
        if let Some(w) = warning.filter(|_| take_settings) {
          bail!("the archive's settings: {w}; import without --settings");
        }

        settings = Some(taken);
      }
      DB_FILE => {
        let path = scratch.0.join(DB_FILE);
        io::copy(&mut entry, &mut File::create(&path)?)?;
        let plan = target.plan(read_entries(&path)?);
        report = plan.report(dry_run);
        if dry_run {
          copies = Some(HashMap::new());
          break; // the recordings are not needed to count them
        }

        let to_copy: HashMap<_, _> = target.add(plan)?.into_iter().collect();
        progress = Progress::new("copying recordings", to_copy.len());
        copies = Some(to_copy);
      }
      _ => {
        let Some(rec) = name.strip_prefix(&format!("{}/", recordings::DIR)) else {
          warn!("{name} in {}: not known, left", file.display());
          continue;
        };

        let Some(copies) = &mut copies else {
          bail!("{}: recordings before the history", not_ours())
        };

        let Some(dst) = copies.remove(rec) else { continue };
        let len = entry.size();
        report.count(super::write_recording(&dst, &mut entry, len), &dst);
        progress.tick();
      }
    }
  }

  progress.finish();

  let Some(left) = copies else { bail!("{}: no history in it", not_ours()) };
  // Recordings the entries name that are not in the archive.
  report.failed += left.len();

  if let Some(settings) = settings.as_ref().filter(|_| take_settings && !dry_run) {
    settings.save(&home.join(settings::FILE))?;
  }

  Ok(Imported { report, settings: take_settings && settings.is_some() })
}

/// The entries of the Wavo history database at `path` (a copy: it is migrated), each with the
/// recording it names.
fn read_entries(path: &Path) -> Result<Vec<Item<String>>> {
  let entries = Db::open(path)?.get_entries()?;
  let items = entries.into_iter().map(|mut entry| Item { audio: entry.audio_path.take(), entry });
  Ok(items.collect())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::history::History;
  use crate::history::db::NewEntry;
  use crate::testing::{TempDir, entry_ids, tone, wav_count, write_wav};

  /// 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry recorded `at` with `text`, with a recording of `seconds` if any; returns its
  /// recording's path.
  fn add(h: &History, at: i64, text: &str, seconds: Option<f64>) -> Option<PathBuf> {
    let (name, path) = h.recordings.new_file(at);
    let audio_path = seconds.map(|_| name.as_str());
    let entry =
      NewEntry { created_at: at, duration_ms: 1_000, audio_path, text, ..NewEntry::default() };
    h.db.insert_entry(&entry).unwrap();
    seconds.map(|s| {
      write_wav(&path, &tone(s, 0.1));
      path
    })
  }

  fn hotkey(home: &Path) -> String {
    Settings::load(&home.join(settings::FILE)).0.hotkey
  }

  fn save_hotkey(home: &Path, hotkey: &str) {
    Settings { hotkey: hotkey.into(), ..Settings::default() }
      .save(&home.join(settings::FILE))
      .unwrap();
  }

  #[test]
  fn moves_the_history_to_another_home() {
    let (from, to, out) =
      (TempDir::new("archive-from"), TempDir::new("archive-to"), TempDir::new("archive-out"));
    save_hotkey(from.path(), "ctrl+x");
    let h = History::open(from.path()).unwrap();
    let wav = add(&h, NOW, "hello", Some(0.5)).unwrap();
    add(&h, NOW + 1_000, "again", Some(0.25));
    let gone = add(&h, NOW + 3_000, "gone", Some(0.25)).unwrap();
    fs::remove_file(gone).unwrap();
    add(&h, NOW + 4_000, "kept", None);
    // The last failed recording is no entry's: it stays.
    let (name, path) = h.recordings.new_file(NOW + 5_000);
    write_wav(&path, &tone(0.25, 0.1));
    h.recordings.keep_failed(&name).unwrap();

    let file = out.path().join("wavo.tar");
    let exported = export(from.path(), &file).unwrap();
    assert_eq!((exported.entries, exported.recordings), (4, 2));
    assert_eq!(exported.bytes, fs::metadata(&file).unwrap().len());
    assert!(export(from.path(), &file).unwrap_err().to_string().contains("exists"));

    // The other home has its own settings and an entry.
    save_hotkey(to.path(), "cmd+y");
    let other = History::open(to.path()).unwrap();
    add(&other, NOW - 1, "theirs", Some(0.1));
    drop(other);

    let dry = import(to.path(), &file, true, true).unwrap();
    let expected = Report { entries: 4, added: 4, copied: 2, failed: 0 };
    assert_eq!(dry, Imported { report: expected, settings: true });
    assert_eq!((entry_ids(to.path()).len(), hotkey(to.path())), (1, "cmd+y".into()));

    let done = import(to.path(), &file, false, false).unwrap();
    assert_eq!(done, Imported { report: expected, settings: false });
    assert_eq!(hotkey(to.path()), "cmd+y", "the settings stay");
    let h = History::open(to.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    let got: Vec<_> = entries.iter().map(|e| (e.created_at, e.text.as_str())).collect();
    assert_eq!(
      got,
      [
        (NOW - 1, "theirs"),
        (NOW, "hello"),
        (NOW + 1_000, "again"),
        (NOW + 3_000, "gone"),
        (NOW + 4_000, "kept")
      ]
    );
    assert_eq!(fs::read(h.audio_file(&entries[1]).unwrap()).unwrap(), fs::read(wav).unwrap());
    assert_eq!((entries[3].audio_path.as_ref(), entries[4].audio_path.as_ref()), (None, None));
    assert_eq!(h.recordings.last_failed(), None, "the last failed recording stays behind");
    assert_eq!(wav_count(&to.path().join(recordings::DIR)), 3);

    // Again, with the settings: no entry twice.
    let again = import(to.path(), &file, true, false).unwrap();
    assert_eq!(
      again,
      Imported { report: Report { entries: 4, ..Report::default() }, settings: true }
    );
    assert_eq!((entry_ids(to.path()).len(), hotkey(to.path())), (5, "ctrl+x".into()));
  }

  /// A recording an archive cut short holds part of is not taken; the whole archive brings it.
  #[test]
  fn a_cut_archive_leaves_no_cut_recording() {
    let (from, to, out) = (
      TempDir::new("archive-cut-from"),
      TempDir::new("archive-cut-to"),
      TempDir::new("archive-cut"),
    );
    let h = History::open(from.path()).unwrap();
    add(&h, NOW, "one", Some(0.5));
    add(&h, NOW + 1_000, "two", Some(2.0));
    let (file, cut) = (out.path().join("wavo.tar"), out.path().join("cut.tar"));
    export(from.path(), &file).unwrap();
    let bytes = fs::read(&file).unwrap();
    fs::write(&cut, &bytes[..bytes.len() - 40_000]).unwrap(); // in the last recording

    let report = import(to.path(), &cut, false, false).map(|x| x.report);
    let h = History::open(to.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    assert_eq!(entries.len(), 2, "{report:?}");
    assert!(h.audio_file(&entries[0]).is_some() && h.audio_file(&entries[1]).is_none());

    let report = import(to.path(), &file, false, false).unwrap().report;
    assert_eq!(report, Report { entries: 2, added: 0, copied: 1, failed: 0 });
    assert!(h.audio_file(&entries[1]).is_some());
  }

  /// Settings this Wavo cannot use do not replace the person's, and nothing is imported.
  #[test]
  fn bad_settings_in_an_archive_are_not_taken() {
    let (from, to, out) = (
      TempDir::new("archive-bad-from"),
      TempDir::new("archive-bad-to"),
      TempDir::new("archive-bad"),
    );
    let h = History::open(from.path()).unwrap();
    add(&h, NOW, "hello", Some(0.1));
    fs::write(from.path().join(settings::FILE), b"{not json").unwrap();
    let file = out.path().join("wavo.tar");
    export(from.path(), &file).unwrap();

    save_hotkey(to.path(), "cmd+y");
    let err = format!("{:#}", import(to.path(), &file, true, false).unwrap_err());
    assert!(err.contains("without --settings"), "{err}");
    assert_eq!((hotkey(to.path()), entry_ids(to.path()).len()), ("cmd+y".into(), 0));
    assert_eq!(import(to.path(), &file, false, false).unwrap().report.added, 1);
    assert_eq!(hotkey(to.path()), "cmd+y");
  }

  #[test]
  fn refuses_what_it_cannot_take() {
    let (home, out) = (TempDir::new("archive-refuse"), TempDir::new("archive-refuse-out"));
    assert!(
      export(home.path(), &out.path().join("a.tar"))
        .unwrap_err()
        .to_string()
        .contains("no history")
    );

    let tar = |name: &str, manifest: &Manifest| {
      let path = out.path().join(name);
      let mut tar = tar::Builder::new(File::create(&path).unwrap());
      let json = serde_json::to_vec(manifest).unwrap();
      let mut header = tar::Header::new_gnu();
      header.set_size(json.len() as u64);
      tar.append_data(&mut header, MANIFEST, json.as_slice()).unwrap();
      tar.finish().unwrap();
      path
    };
    let manifest =
      Manifest { format: FORMAT, version: "0".into(), created_at: 0, entries: 0, recordings: 0 };
    let newer = tar("newer.tar", &Manifest { format: FORMAT + 1, ..manifest.clone() });
    let empty = tar("empty.tar", &manifest);
    let junk = out.path().join("junk.tar");
    fs::write(&junk, b"not a tar").unwrap();

    let err = |file: &Path| format!("{:#}", import(home.path(), file, false, true).unwrap_err());
    assert!(err(&newer).contains("newer Wavo"), "{}", err(&newer));
    assert!(err(&empty).contains("no history"), "{}", err(&empty));
    assert!(err(&junk).contains("not an archive"), "{}", err(&junk));
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0, "a dry run writes nothing");

    let crate::ctl::Claim::Server(_app) = crate::ctl::claim(home.path()).unwrap() else {
      panic!("the home is taken")
    };
    let err = import(home.path(), &empty, false, false).unwrap_err().to_string();
    assert!(err.contains("running"), "{err}");
  }
}
