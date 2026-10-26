// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Moving the history between computers: Export in the settings window or `wavo-app export`
//! writes an archive; Import or `wavo-app import <file>` takes one (scripts/handy.py makes one of
//! Handy's history). A zip, stored without compression
//! (recordings do not compress), in this order:
//!
//! - `wavo-export.json`, the manifest: an archive without it is refused;
//! - `settings.json`, if there are settings;
//! - `history.db`, a consistent copy of the history (`VACUUM INTO`, while the app may run);
//! - `recordings/<name>`, the recordings of its entries, as they name them.
//!
//! An import adds the entries the history does not have: one is there when an entry there started
//! at the same Unix ms, so importing twice adds nothing. Entries without text come only with a
//! recording. The new entries go in first, in one transaction, naming the recordings they will
//! have; the recordings are copied after, each through a `.part` file, so the next import finishes
//! one cut short. The command holds the app's single-instance lock: it refuses while Wavo runs.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;
use zip::read::ZipFile;
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::ctl::{self, Claim};
use crate::history::{DB_FILE, Db, Entry, History, RECORDINGS_DIR, Recordings, now_ms};
use crate::settings;

/// The archive's layout; one from a newer Wavo is refused.
const FORMAT: u32 = 1;
const MANIFEST: &str = "wavo-export.json";
/// Recordings go through in big pieces, not 8 KB ones.
const BUFFER: usize = 1 << 20;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
  format: u32,
  /// The Wavo that wrote it.
  version: String,
  /// Unix ms.
  created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exported {
  pub entries: usize,
  pub recordings: usize,
  /// The archive's size.
  pub bytes: u64,
}

/// What an import did, or would do in a dry run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Imported {
  /// Entries in the archive: with a recording or text.
  pub entries: usize,
  /// Those that were not in the history.
  pub added: usize,
  /// Recordings copied: the added entries' and those an import cut short left out.
  pub copied: usize,
  /// Recordings that could not be copied or were not in the archive; an import again copies them.
  pub failed: usize,
  /// The archive's settings replaced these.
  pub settings: bool,
}

// MARK: Export

/// Writes the settings, the history and its recordings in `home` into a new archive `file`;
/// `progress` hears the bytes of them written of the total after each file.
pub fn export(home: &Path, file: &Path, mut progress: impl FnMut(u64, u64)) -> Result<Exported> {
  if file.exists() {
    bail!("{} exists", file.display());
  }

  let db = home.join(DB_FILE);
  if !db.is_file() {
    bail!("no history in {}", home.display());
  }

  let scratch = Scratch::new()?;
  let copy = scratch.0.join(DB_FILE);
  snapshot(&db, &copy)?;

  // The copy names only the recordings on disk, and keeps no entry with neither text nor one.
  let copy_db = Db::open(&copy)?;
  let on_disk: HashSet<String> = Recordings::open(home)?.list()?.into_iter().collect();
  let (mut names, mut missing, mut empty, mut entries) = (Vec::new(), Vec::new(), Vec::new(), 0);
  for e in copy_db.get_entries()? {
    match e.audio_path {
      Some(name) if on_disk.contains(&name) => names.push(name),
      audio => {
        missing.extend(audio);
        if e.text.trim().is_empty() {
          empty.push(e.id);
        }
      }
    }

    entries += 1;
  }

  copy_db.update_without_audio(&missing)?;
  copy_db.delete_entries(&empty)?;
  drop(copy_db);

  let manifest =
    Manifest { format: FORMAT, version: env!("CARGO_PKG_VERSION").into(), created_at: now_ms() };
  let dir = home.join(RECORDINGS_DIR);
  let settings = home.join(settings::FILE);
  let size = |path: &Path| fs::metadata(path).map_or(0, |m| m.len());
  let sizes: Vec<u64> = names.iter().map(|name| size(&dir.join(name))).collect();
  let head = size(&settings) + size(&copy);
  let total = head + sizes.iter().sum::<u64>();
  let mut recordings = 0;
  via_part(file, |part| {
    let out = File::create(part).with_context(|| format!("create {}", part.display()))?;
    // Streamed: an entry's sizes follow its data. A seek back to its header, a flush and two
    // seeks a recording, makes the export three times slower.
    let mut zip = ZipWriter::new_stream(BufWriter::with_capacity(BUFFER, out));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file(MANIFEST, stored)?;
    serde_json::to_writer_pretty(&mut zip, &manifest)?;
    if settings.is_file() {
      zip.start_file(settings::FILE, stored)?;
      io::copy(&mut File::open(&settings)?, &mut zip)?;
    }

    zip.start_file(DB_FILE, stored)?;
    io::copy(&mut File::open(&copy)?, &mut zip)?;
    let mut done = head;
    progress(done, total);
    for (name, bytes) in names.iter().zip(sizes) {
      // One deleted since the listing is left out: an import counts it as not copied.
      match File::open(dir.join(name)) {
        Ok(mut wav) => {
          zip.start_file(format!("{RECORDINGS_DIR}/{name}"), stored)?;
          io::copy(&mut wav, &mut zip)?;
          recordings += 1;
        }
        Err(e) => warn!("{name}: {e}; left out"),
      }

      done += bytes;
      progress(done, total);
    }

    let out = zip.finish()?.into_inner().into_inner().map_err(|e| e.into_error())?;
    out.sync_all()?;
    Ok(())
  })?;

  let bytes = fs::metadata(file)?.len();
  Ok(Exported { entries: entries - empty.len(), recordings, bytes })
}

// MARK: Import

/// Imports the archive `file` into `home`, unless Wavo runs for it (the app imports with
/// [import_in_app]); `take_settings` replaces the settings with the archive's. A dry run says what
/// it would do and writes nothing.
pub fn import(home: &Path, file: &Path, take_settings: bool, dry_run: bool) -> Result<Imported> {
  let _lock = match dry_run {
    true => None,
    false => {
      fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
      match ctl::claim(home)? {
        Claim::Server(lock) => Some(lock),
        Claim::Running => bail!("Wavo is running for {}: quit it first", home.display()),
      }
    }
  };

  import_into(home, file, take_settings, dry_run, |_, _| {})
}

/// [import] by the app, which holds the lock: the history without the settings; `progress` hears
/// how far through the archive its recordings are, in bytes.
pub fn import_in_app(home: &Path, file: &Path, progress: impl FnMut(u64, u64)) -> Result<Imported> {
  import_into(home, file, false, false, progress)
}

fn import_into(
  home: &Path,
  file: &Path,
  take_settings: bool,
  dry_run: bool,
  mut progress: impl FnMut(u64, u64),
) -> Result<Imported> {
  // A dry run reads a copy of the history and lists the recordings: nothing of the home is
  // created or migrated.
  let history = if dry_run { None } else { Some(History::open(home)?) };
  let ours = match &history {
    Some(h) => h.db.get_entries()?,
    None if home.join(DB_FILE).is_file() => {
      let scratch = Scratch::new()?;
      let copy = scratch.0.join(DB_FILE);
      snapshot(&home.join(DB_FILE), &copy)?;
      Db::open(&copy)?.get_entries()?
    }
    None => Vec::new(),
  };
  let starts: HashMap<i64, Option<String>> =
    ours.into_iter().map(|e| (e.created_at, e.audio_path)).collect();
  let on_disk = match &history {
    Some(h) => h.recordings.list()?,
    None if home.join(RECORDINGS_DIR).is_dir() => Recordings::open(home)?.list()?,
    None => Vec::new(),
  };
  let on_disk: HashSet<String> = on_disk.into_iter().collect();

  let reader = File::open(file).with_context(|| format!("open {}", file.display()))?;
  let not_ours = || format!("{} is not an archive of `wavo-app export`", file.display());
  let mut archive = ZipArchive::new(BufReader::new(reader)).with_context(not_ours)?;
  let manifest = archive.by_name(MANIFEST).with_context(not_ours)?;
  let manifest: Manifest = serde_json::from_reader(manifest).with_context(not_ours)?;
  if manifest.format > FORMAT {
    bail!("{} is from a newer Wavo ({}): update this one first", file.display(), manifest.version);
  }

  let settings = match archive.by_name(settings::FILE) {
    Ok(mut item) => {
      let mut bytes = Vec::new();
      item.read_to_end(&mut bytes)?;
      // Checked before anything is written; values this Wavo does not know come along.
      let object = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes);
      if take_settings && object.is_err() {
        bail!("the archive's settings are not readable: import without --settings");
      }

      Some(bytes)
    }
    Err(ZipError::FileNotFound) => None,
    Err(e) => return Err(e).with_context(not_ours),
  };

  let scratch = Scratch::new()?;
  let path = scratch.0.join(DB_FILE);
  let mut db =
    archive.by_name(DB_FILE).with_context(|| format!("{}: no history in it", not_ours()))?;
  io::copy(&mut db, &mut File::create(&path)?)?;
  drop(db);

  let plan = Plan::new(&starts, &on_disk, Db::open(&path)?.get_entries()?);
  let mut done = Imported { entries: plan.entries, added: plan.new.len(), ..Imported::default() };
  if dry_run {
    done.copied = plan.new.iter().filter(|(_, audio)| audio.is_some()).count();
    done.copied += plan.missing.len();
    done.settings = take_settings && settings.is_some();
    return Ok(done);
  }

  // In the archive's order, read front to back.
  let mut copies = plan.add(history.as_ref().unwrap(), &starts)?;
  let (prefix, len) = (format!("{RECORDINGS_DIR}/"), fs::metadata(file)?.len());
  for i in 0..archive.len() {
    let dst = match archive.name_for_index(i) {
      Some(Ok(name)) => name.strip_prefix(&prefix).and_then(|rec| copies.remove(rec)),
      _ => None,
    };
    let Some(dst) = dst else { continue };
    let copied = archive.by_index(i).map_err(anyhow::Error::from).and_then(|mut item| {
      write_recording(&dst, &mut item)?;
      Ok(item.header_start() + item.compressed_size())
    });
    match copied {
      Ok(end) => {
        done.copied += 1;
        progress(end, len);
      }
      Err(e) => {
        warn!("{}: {e:#}", dst.display());
        done.failed += 1;
      }
    }
  }

  done.failed += copies.len(); // named by the entries, not in the archive

  if let Some(bytes) = settings.filter(|_| take_settings) {
    fs::write(home.join(settings::FILE), bytes)?;
    done.settings = true;
  }

  Ok(done)
}

/// What an import makes of another history's entries against this one's `starts` and recordings
/// `on_disk`.
struct Plan {
  /// With a recording or text.
  entries: usize,
  /// Not in the history yet, each with its recording's name in the archive.
  new: Vec<(Entry, Option<String>)>,
  /// Recordings an import cut short did not copy: their names in the archive and here.
  missing: Vec<(String, String)>,
}

impl Plan {
  fn new(
    starts: &HashMap<i64, Option<String>>,
    on_disk: &HashSet<String>,
    theirs: Vec<Entry>,
  ) -> Self {
    let mut plan = Self { entries: 0, new: Vec::new(), missing: Vec::new() };
    for mut e in theirs {
      let audio = e.audio_path.take();
      if e.text.trim().is_empty() && audio.is_none() {
        continue;
      }

      plan.entries += 1;
      match (starts.get(&e.created_at), audio) {
        (None, audio) => plan.new.push((Entry { busy: false, ..e }, audio)),
        (Some(Some(name)), Some(audio)) if !on_disk.contains(name) => {
          plan.missing.push((audio, name.clone()))
        }
        _ => {}
      }
    }

    plan
  }

  /// Adds the new entries in one transaction; returns the recordings to copy, by their names in
  /// the archive, with the paths they go to.
  fn add(
    self,
    h: &History,
    starts: &HashMap<i64, Option<String>>,
  ) -> Result<HashMap<String, PathBuf>> {
    // Names the entries hold whose files are not there yet (an import cut short) are taken too.
    let mut taken: HashSet<String> = starts.values().flatten().cloned().collect();
    let mut copies = HashMap::new();
    let mut entries = Vec::with_capacity(self.new.len());
    for (mut e, audio) in self.new {
      if let Some(audio) = audio {
        let (name, path) = h.recordings.new_file_besides(e.created_at, &taken);
        taken.insert(name.clone());
        e.audio_path = Some(name);
        copies.insert(audio, path);
      }

      entries.push(e);
    }

    h.db.insert(&entries)?;
    for (audio, name) in self.missing {
      copies.extend(h.recordings.path(&name).map(|path| (audio, path)));
    }

    Ok(copies)
  }
}

/// Copies a recording out of the archive through `<dst>.part`: one that does not match its CRC
/// (zip checks it) is an error, not a recording.
fn write_recording(dst: &Path, src: &mut ZipFile<'_, impl Read>) -> Result<()> {
  fs::create_dir_all(dst.parent().unwrap())?;
  via_part(dst, |part| Ok(io::copy(src, &mut File::create(part)?).map(|_| ())?))
}

// MARK: Files

/// Makes `dst` with `write` through `<dst>.part` renamed into place: a file cut short is never
/// taken for it.
fn via_part(dst: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
  let part = part_of(dst);
  let done = write(&part).and_then(|()| Ok(fs::rename(&part, dst)?));
  if done.is_err() {
    let _ = fs::remove_file(&part);
  }

  done
}

pub fn part_of(path: &Path) -> PathBuf {
  let mut name = path.file_name().unwrap_or_default().to_owned();
  name.push(".part");
  path.with_file_name(name)
}

/// A temp directory for an import's or export's own files, removed on drop.
/// A consistent copy of the database `db` (`VACUUM INTO`) while the app may be writing it.
fn snapshot(db: &Path, copy: &Path) -> Result<()> {
  let conn = rusqlite::Connection::open(db).with_context(|| format!("open {}", db.display()))?;
  conn.execute("VACUUM INTO ?1", [copy.to_str().unwrap()])?;
  Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
  fn new() -> Result<Self> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wavo-transfer-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(Self(dir))
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use super::*;
  use crate::testing::{TempDir, entry_ids, tone, wav_count, write_wav};

  /// 2026-10-09 00:00 UTC.
  const NOW: i64 = 1_791_504_000_000;

  /// An entry recorded `at` with `text`, and a recording of `seconds` if any; its path.
  fn add(h: &History, at: i64, text: &str, seconds: Option<f64>) -> PathBuf {
    let (name, path) = h.recordings.new_file(at);
    let audio_path = seconds.map(|_| name);
    let entry = Entry { created_at: at, audio_path, text: text.into(), ..Entry::default() };
    h.db.insert(&[entry]).unwrap();
    if let Some(s) = seconds {
      write_wav(&path, &tone(s, 0.1));
    }

    path
  }

  fn hotkey(home: &Path) -> String {
    settings::Settings::load(&home.join(settings::FILE)).0.hotkey
  }

  #[test]
  fn moves_the_history_to_another_home() {
    let (from, to, out) = (TempDir::new("tr-from"), TempDir::new("tr-to"), TempDir::new("tr-out"));
    fs::write(from.path().join(settings::FILE), r#"{"hotkey": "ctrl+x", "newer": 1}"#).unwrap();
    let h = History::open(from.path()).unwrap();
    let wav = add(&h, NOW, "hello", Some(0.5));
    add(&h, NOW + 1_000, "", Some(0.25)); // no text yet: comes with its recording
    fs::remove_file(add(&h, NOW + 2_000, "gone", Some(0.25))).unwrap(); // its text comes
    fs::remove_file(add(&h, NOW + 3_000, "", Some(0.25))).unwrap(); // nothing of it is left

    let file = out.path().join("wavo.zip");
    let mut last = (0, 0);
    let exported = export(from.path(), &file, |done, total| last = (done, total)).unwrap();
    assert_eq!((exported.entries, exported.recordings), (3, 2));
    assert!(last.1 > 0 && last.0 == last.1, "progress ends at the total: {last:?}");
    assert!(export(from.path(), &file, |_, _| {}).unwrap_err().to_string().contains("exists"));

    add(&History::open(to.path()).unwrap(), NOW - 1, "theirs", Some(0.1));
    let expected = Imported { entries: 3, added: 3, copied: 2, failed: 0, settings: true };
    assert_eq!(import(to.path(), &file, true, true).unwrap(), expected, "a dry run");
    assert_eq!(entry_ids(to.path()).len(), 1);

    let done = import(to.path(), &file, false, false).unwrap();
    assert_eq!(done, Imported { settings: false, ..expected });
    let h = History::open(to.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    let got: Vec<_> = entries.iter().map(|e| (e.created_at, e.text.as_str())).collect();
    assert_eq!(
      got,
      [(NOW - 1, "theirs"), (NOW, "hello"), (NOW + 1_000, ""), (NOW + 2_000, "gone")]
    );
    assert_eq!(fs::read(h.audio_file(&entries[1]).unwrap()).unwrap(), fs::read(wav).unwrap());
    assert_eq!(entries[3].audio_path, None);
    assert_eq!(wav_count(&to.path().join(RECORDINGS_DIR)), 3);

    // Again, with the settings as they are: no entry twice.
    let again = import(to.path(), &file, true, false).unwrap();
    assert_eq!(again, Imported { entries: 3, settings: true, ..Imported::default() });
    assert_eq!((entry_ids(to.path()).len(), hotkey(to.path())), (4, "ctrl+x".into()));
    assert!(fs::read_to_string(to.path().join(settings::FILE)).unwrap().contains("newer"));
  }

  /// An archive cut short is refused whole; a recording an import cut short did not copy comes
  /// with the next.
  #[test]
  fn a_cut_import_is_finished_by_the_next() {
    let (from, to, out) = (TempDir::new("cut-from"), TempDir::new("cut-to"), TempDir::new("cut"));
    let h = History::open(from.path()).unwrap();
    add(&h, NOW, "one", Some(0.5));
    add(&h, NOW + 1_000, "two", Some(2.0));
    let (file, cut) = (out.path().join("wavo.zip"), out.path().join("cut.zip"));
    export(from.path(), &file, |_, _| {}).unwrap();
    let bytes = fs::read(&file).unwrap();
    fs::write(&cut, &bytes[..bytes.len() - 100]).unwrap(); // its index, at the end
    assert!(import(to.path(), &cut, false, false).is_err());
    assert!(entry_ids(to.path()).is_empty());

    import(to.path(), &file, false, false).unwrap();
    let h = History::open(to.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    fs::remove_file(h.audio_file(&entries[1]).unwrap()).unwrap();
    let done = import(to.path(), &file, false, false).unwrap();
    assert_eq!((done.added, done.copied), (0, 1));
    assert!(h.audio_file(&entries[1]).is_some());
  }

  /// The app imports beside the lock it holds, its progress going through the archive.
  #[test]
  fn the_app_imports_while_it_runs() {
    let (from, to, out) = (TempDir::new("app-from"), TempDir::new("app-to"), TempDir::new("app"));
    add(&History::open(from.path()).unwrap(), NOW, "one", Some(0.5));
    let file = out.path().join("wavo.zip");
    export(from.path(), &file, |_, _| {}).unwrap();

    let Claim::Server(_app) = ctl::claim(to.path()).unwrap() else { panic!("taken") };
    let mut last = (0, 0);
    let done = import_in_app(to.path(), &file, |done, total| last = (done, total)).unwrap();
    assert_eq!((done.added, done.copied), (1, 1));
    assert!(last.0 > 0 && last.0 <= last.1, "{last:?}");
  }

  #[test]
  fn refuses_a_newer_archive_and_a_running_wavo() {
    let (home, out) = (TempDir::new("tr-refuse"), TempDir::new("tr-refuse-out"));
    let path = out.path().join("newer.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    zip.start_file(MANIFEST, SimpleFileOptions::default()).unwrap();
    zip.write_all(br#"{"format": 2, "version": "9.0", "created_at": 0}"#).unwrap();
    zip.finish().unwrap();

    let err = format!("{:#}", import(home.path(), &path, false, true).unwrap_err());
    assert!(err.contains("newer Wavo"), "{err}");
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0, "a dry run writes nothing");

    let Claim::Server(_app) = ctl::claim(home.path()).unwrap() else { panic!("taken") };
    let err = import(home.path(), &path, false, false).unwrap_err().to_string();
    assert!(err.contains("running"), "{err}");
  }
}
