// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Moving the history between computers: `wavo-app export` writes an archive, `import <file>`
//! takes one (scripts/handy.py makes one of Handy's history). An uncompressed tar, read and
//! written as a stream, in this order:
//!
//! - `wavo-export.json`, the manifest, first: anything else is refused at once;
//! - `settings.json`, if there are settings;
//! - `history.db`, a consistent copy of the history (`VACUUM INTO`, while the app may run);
//! - `recordings/<name>`, the recordings of its entries, as they name them.
//!
//! An import adds the entries the history does not have: one is there when an entry there started
//! at the same Unix ms, so importing twice adds nothing. Entries without text come only with a
//! recording. The new entries go in first, in one transaction, naming the recordings they will
//! have; the recordings are copied after, each through a `.part` file, so the next import finishes
//! one cut short. It holds the app's single-instance lock: it refuses while Wavo runs.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

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

/// Writes the settings, the history and its recordings in `home` into a new archive `file`.
pub fn export(home: &Path, file: &Path) -> Result<Exported> {
  if file.exists() {
    bail!("{} exists", file.display());
  }

  let db = home.join(DB_FILE);
  if !db.is_file() {
    bail!("no history in {}", home.display());
  }

  let scratch = Scratch::new()?;
  let copy = scratch.0.join(DB_FILE);
  let conn = rusqlite::Connection::open(&db).with_context(|| format!("open {}", db.display()))?;
  conn.execute("VACUUM INTO ?1", [copy.to_str().unwrap()])?;
  drop(conn);

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
  let mut recordings = 0;
  via_part(file, |part| {
    let out = File::create(part).with_context(|| format!("create {}", part.display()))?;
    let mut tar = tar::Builder::new(BufWriter::with_capacity(BUFFER, out));
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

    tar.append_path_with_name(&copy, DB_FILE)?;
    for name in &names {
      // One deleted since the listing is left out: an import counts it as not copied.
      match File::open(dir.join(name)) {
        Ok(mut wav) => {
          tar.append_file(format!("{RECORDINGS_DIR}/{name}"), &mut wav)?;
          recordings += 1;
        }
        Err(e) => warn!("{name}: {e}; left out"),
      }
    }

    let out = tar.into_inner()?.into_inner().map_err(|e| e.into_error())?;
    out.sync_all()?;
    Ok(())
  })?;

  let bytes = fs::metadata(file)?.len();
  Ok(Exported { entries: entries - empty.len(), recordings, bytes })
}

// MARK: Import

/// Imports the archive `file` into `home`; `take_settings` replaces the settings with the
/// archive's. A dry run says what it would do and writes nothing.
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

  // A dry run reads a copy of the history (its WAL too: the app may be writing), and lists the
  // recordings: nothing of the home is created, migrated or written.
  let history = if dry_run { None } else { Some(History::open(home)?) };
  let ours = match &history {
    Some(h) => h.db.get_entries()?,
    None if home.join(DB_FILE).is_file() => {
      let copy = Scratch::new()?;
      for suffix in ["", "-wal", "-shm"] {
        let name = format!("{DB_FILE}{suffix}");
        if home.join(&name).is_file() {
          fs::copy(home.join(&name), copy.0.join(&name))?;
        }
      }

      Db::open(&copy.0.join(DB_FILE))?.get_entries()?
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
  let mut archive = tar::Archive::new(BufReader::with_capacity(BUFFER, reader));
  let mut items = archive.entries().with_context(|| format!("read {}", file.display()))?;
  let not_ours = || format!("{} is not an archive of `wavo-app export`", file.display());
  let mut first = items.next().with_context(not_ours)?.with_context(not_ours)?;
  if first.path()?.to_str() != Some(MANIFEST) {
    bail!(not_ours());
  }

  let manifest: Manifest = serde_json::from_reader(&mut first).with_context(not_ours)?;
  if manifest.format > FORMAT {
    bail!("{} is from a newer Wavo ({}): update this one first", file.display(), manifest.version);
  }

  let scratch = Scratch::new()?;
  let mut done = Imported::default();
  let mut settings = None;
  // The recordings to copy by their names in the archive, once its history is read.
  let mut copies: Option<HashMap<String, PathBuf>> = None;
  for item in items {
    let mut item = item?;
    let name = item.path()?.to_str().with_context(not_ours)?.to_owned();
    match name.as_str() {
      settings::FILE => {
        let mut bytes = Vec::new();
        item.read_to_end(&mut bytes)?;
        // Checked before anything is written; values this Wavo does not know come along.
        let object = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes);
        if take_settings && object.is_err() {
          bail!("the archive's settings are not readable: import without --settings");
        }

        settings = Some(bytes);
      }
      DB_FILE => {
        let path = scratch.0.join(DB_FILE);
        io::copy(&mut item, &mut File::create(&path)?)?;
        let plan = Plan::new(&starts, &on_disk, Db::open(&path)?.get_entries()?);
        (done.entries, done.added) = (plan.entries, plan.new.len());
        if dry_run {
          done.copied = plan.new.iter().filter(|(_, audio)| audio.is_some()).count();
          done.copied += plan.missing.len();
          done.settings = take_settings && settings.is_some();
          return Ok(done);
        }

        copies = Some(plan.add(history.as_ref().unwrap(), &starts)?);
      }
      _ => {
        let Some(rec) = name.strip_prefix(&format!("{RECORDINGS_DIR}/")) else { continue };
        let Some(copies) = &mut copies else {
          bail!("{}: recordings before the history", not_ours())
        };
        let Some(dst) = copies.remove(rec) else { continue };
        match write_recording(&dst, &mut item) {
          Ok(()) => done.copied += 1,
          Err(e) => {
            warn!("{}: {e:#}", dst.display());
            done.failed += 1;
          }
        }
      }
    }
  }

  let Some(left) = copies else { bail!("{}: no history in it", not_ours()) };
  done.failed += left.len(); // named by the entries, not in the archive

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

/// Copies a recording out of the archive through `<dst>.part`: fewer bytes than the archive says
/// (an archive cut short) are an error, not a recording.
fn write_recording(dst: &Path, src: &mut tar::Entry<impl Read>) -> Result<()> {
  let len = src.size();
  fs::create_dir_all(dst.parent().unwrap())?;
  via_part(dst, |part| match io::copy(src, &mut File::create(part)?)? {
    n if n == len => Ok(()),
    n => bail!("{n} of {len} bytes"),
  })
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

    let file = out.path().join("wavo.tar");
    let exported = export(from.path(), &file).unwrap();
    assert_eq!((exported.entries, exported.recordings), (3, 2));
    assert!(export(from.path(), &file).unwrap_err().to_string().contains("exists"));

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

  /// A recording an archive cut short holds part of is not taken; the whole archive brings it.
  #[test]
  fn a_cut_import_is_finished_by_the_next() {
    let (from, to, out) = (TempDir::new("cut-from"), TempDir::new("cut-to"), TempDir::new("cut"));
    let h = History::open(from.path()).unwrap();
    add(&h, NOW, "one", Some(0.5));
    add(&h, NOW + 1_000, "two", Some(2.0));
    let (file, cut) = (out.path().join("wavo.tar"), out.path().join("cut.tar"));
    export(from.path(), &file).unwrap();
    let bytes = fs::read(&file).unwrap();
    fs::write(&cut, &bytes[..bytes.len() - 40_000]).unwrap(); // in the last recording

    let _ = import(to.path(), &cut, false, false);
    let h = History::open(to.path()).unwrap();
    let entries = h.db.get_entries().unwrap();
    assert!(h.audio_file(&entries[0]).is_some() && h.audio_file(&entries[1]).is_none());

    let done = import(to.path(), &file, false, false).unwrap();
    assert_eq!((done.added, done.copied), (0, 1));
    assert!(h.audio_file(&entries[1]).is_some());
  }

  #[test]
  fn refuses_a_newer_archive_and_a_running_wavo() {
    let (home, out) = (TempDir::new("tr-refuse"), TempDir::new("tr-refuse-out"));
    let path = out.path().join("newer.tar");
    let mut tar = tar::Builder::new(File::create(&path).unwrap());
    let json = br#"{"format": 2, "version": "9.0", "created_at": 0}"#;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    tar.append_data(&mut header, MANIFEST, &json[..]).unwrap();
    tar.finish().unwrap();

    let err = format!("{:#}", import(home.path(), &path, false, true).unwrap_err());
    assert!(err.contains("newer Wavo"), "{err}");
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0, "a dry run writes nothing");

    let Claim::Server(_app) = ctl::claim(home.path()).unwrap() else { panic!("taken") };
    let err = import(home.path(), &path, false, false).unwrap_err().to_string();
    assert!(err.contains("running"), "{err}");
  }
}
