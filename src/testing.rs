// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Test helpers: temp dirs, polling, audio and history files, speech fixtures, real models and
//! fake ones in a cache, a fake engine, and WER.

use std::f32::consts::TAU;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use crossbeam_channel::{Receiver, Sender, bounded};
use rusqlite::Connection;

use crate::audio::vad::Clip;
use crate::audio::wav::WavSink;
use crate::audio::{SAMPLE_RATE, wav};
use crate::engine::{Loader, SpeechModel};
use crate::history::DB_FILE;

/// A fresh directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
  pub fn new(name: &str) -> Self {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("wavo-test-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    Self(path)
  }

  pub fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// What `f` returns once it returns something; it is asked every few ms, for up to `timeout`.
pub fn poll<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
  let deadline = Instant::now() + timeout;
  loop {
    if let Some(v) = f() {
      return v;
    }

    assert!(Instant::now() < deadline, "no {what} within {timeout:?}");
    thread::sleep(Duration::from_millis(5));
  }
}

/// `seconds` of a 440 Hz sine of amplitude `a`, 16 kHz.
pub fn tone(seconds: f64, a: f32) -> Vec<f32> {
  let n = (seconds * f64::from(SAMPLE_RATE)) as usize;
  (0..n).map(|i| a * (TAU * 440.0 * i as f32 / SAMPLE_RATE as f32).sin()).collect()
}

/// `pcm` (16 kHz mono) into a new WAV file at `path`, as recordings are written.
pub fn write_wav(path: &Path, pcm: &[f32]) {
  let mut sink = WavSink::create(path).unwrap();
  sink.write(pcm).unwrap();
  sink.finalize().unwrap();
}

/// `.wav` files in `dir`, at any depth.
pub fn wav_count(dir: &Path) -> usize {
  let files = fs::read_dir(dir).unwrap().flatten();
  files
    .map(|e| match e.path() {
      path if path.is_dir() => wav_count(&path),
      path => usize::from(path.extension().is_some_and(|x| x == "wav")),
    })
    .sum()
}

/// Every entry's id in the history in `home`, in order.
pub fn entry_ids(home: &Path) -> Vec<i64> {
  let db = Connection::open(home.join(DB_FILE)).unwrap();
  let mut stmt = db.prepare("SELECT id FROM entries ORDER BY id").unwrap();
  stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
}

/// A `target/fixtures/<name>.wav` from `make fixtures`, with its reference transcript.
pub struct Fixture {
  pub name: String,
  pub pcm: Vec<f32>,
  pub text: String,
}

impl Fixture {
  /// What the engine gets for this recording (see [`Clip::engine_input`]).
  pub fn input(&self) -> Clip {
    let clip = Clip::analyze(self.pcm.clone());
    clip.engine_input().unwrap_or_else(|| panic!("{}: no speech found", self.name))
  }
}

pub fn fixture(name: &str) -> Fixture {
  let text = fs::read_to_string(fixtures_dir().join(format!("{name}.txt"))).unwrap();
  Fixture { name: name.into(), pcm: fixture_pcm(name), text }
}

/// Audio of a fixture, also of those without a transcript (`silence`, `noise`).
pub fn fixture_pcm(name: &str) -> Vec<f32> {
  wav::read(&fixture_wav(name)).unwrap_or_else(|e| panic!("{e:#} (run `make fixtures`)"))
}

pub fn fixture_wav(name: &str) -> PathBuf {
  fixtures_dir().join(format!("{name}.wav"))
}

fn fixtures_dir() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fixtures")
}

/// Russian test model: `$WAVO_TEST_MODEL_RU`, else gigaam-v3 from the Hugging Face cache.
pub fn model_ru() -> PathBuf {
  test_model("WAVO_TEST_MODEL_RU", "gigaam-v3")
}

/// Multilingual test model: `$WAVO_TEST_MODEL_EN`, else whisper-turbo from the Hugging Face cache.
pub fn model_en() -> PathBuf {
  test_model("WAVO_TEST_MODEL_EN", "whisper-turbo")
}

fn test_model(var: &str, name: &str) -> PathBuf {
  if let Some(path) = env::var_os(var) {
    return path.into();
  }

  let found = wavo::hub::Hub::new().find(name);
  found.unwrap_or_else(|| panic!("{name} is not in the Hugging Face cache; set {var}"))
}

/// wavo's model `name` in the Hugging Face cache at `hub`, as `hf` lays it out, a file the fake
/// engine loads; its path.
pub fn download(hub: &wavo::hub::Hub, name: &str) -> PathBuf {
  let m = wavo::hub::lookup(name).unwrap();
  let repo = hub.root().join(format!("models--{}", m.repo().replace('/', "--")));
  fs::create_dir_all(repo.join("refs")).unwrap();
  fs::write(repo.join("refs/main"), "c1").unwrap();
  let file = repo.join("snapshots/c1").join(m.file());
  fs::create_dir_all(file.parent().unwrap()).unwrap();
  fs::write(&file, b"GGUF").unwrap();
  file
}

/// A stand-in for wavo: it loads any file but one that is gone or starts with `BROKEN`, cuts the
/// audio with [`wavo::split`] at its window and answers each piece with its text. Clones share the
/// counters.
#[derive(Clone)]
pub struct FakeModels {
  text: String,
  window_ms: u32,
  loads: Arc<AtomicUsize>,
  live: Arc<AtomicUsize>,
  /// The samples of each piece run, in order.
  runs: Arc<Mutex<Vec<usize>>>,
  panic_next: Arc<AtomicBool>,
  gate: Arc<Mutex<Option<Receiver<()>>>>,
}

impl FakeModels {
  pub fn new(text: &str) -> Self {
    Self {
      text: text.trim().to_owned(),
      window_ms: u32::MAX,
      loads: Arc::default(),
      live: Arc::default(),
      runs: Arc::default(),
      panic_next: Arc::default(),
      gate: Arc::default(),
    }
  }

  pub fn window_ms(self, ms: u32) -> Self {
    Self { window_ms: ms, ..self }
  }

  pub fn loads(&self) -> usize {
    self.loads.load(Ordering::SeqCst)
  }

  /// Models in memory now.
  pub fn live(&self) -> usize {
    self.live.load(Ordering::SeqCst)
  }

  pub fn runs(&self) -> Vec<usize> {
    self.runs.lock().unwrap().clone()
  }

  /// The next run panics, as a bug in wavo would.
  pub fn panic_next(&self) {
    self.panic_next.store(true, Ordering::SeqCst);
  }

  /// Every piece waits until the sender is dropped.
  pub fn hold(&self) -> Sender<()> {
    let (tx, rx) = bounded(0);
    *self.gate.lock().unwrap() = Some(rx);
    tx
  }
}

impl Loader for FakeModels {
  fn load(&self, path: &Path) -> anyhow::Result<Box<dyn SpeechModel>> {
    if fs::read(path)?.starts_with(b"BROKEN") {
      anyhow::bail!("not a speech model");
    }

    self.loads.fetch_add(1, Ordering::SeqCst);
    self.live.fetch_add(1, Ordering::SeqCst);
    Ok(Box::new(FakeModel(self.clone())))
  }
}

struct FakeModel(FakeModels);

impl SpeechModel for FakeModel {
  fn device(&self) -> String {
    "fake".into()
  }

  fn transcribe(
    &self,
    pcm: &[f32],
    _language: Option<&str>,
    on_piece: &mut dyn FnMut(usize, usize) -> ControlFlow<()>,
  ) -> anyhow::Result<String> {
    let m = &self.0;
    assert!(!m.panic_next.swap(false, Ordering::SeqCst), "injected panic in a run");
    let pieces = wavo::split(pcm, m.window_ms);
    let count = pieces.len();
    for (index, range) in pieces.into_iter().enumerate() {
      let gate = m.gate.lock().unwrap().clone();
      if let Some(gate) = gate {
        let _ = gate.recv();
      }

      m.runs.lock().unwrap().push(range.len());
      if on_piece(index, count).is_break() {
        return Err(wavo::Error::Cancelled.into());
      }
    }

    Ok(vec![m.text.as_str(); count].join(" ").trim().to_owned())
  }
}

impl Drop for FakeModel {
  fn drop(&mut self) {
    self.0.live.fetch_sub(1, Ordering::SeqCst);
  }
}

/// Word error rate of `hypothesis` against `reference`: word-level Levenshtein distance divided by
/// the reference length, after [`words`] normalization.
pub fn wer(reference: &str, hypothesis: &str) -> f64 {
  let (r, h) = (words(reference), words(hypothesis));
  if r.is_empty() {
    return if h.is_empty() { 0.0 } else { 1.0 };
  }

  // One row of the edit-distance matrix at a time: prev[j] = distance(r[..i], h[..j]).
  let mut prev: Vec<usize> = (0..=h.len()).collect();
  for (i, rw) in r.iter().enumerate() {
    let mut cur = vec![i + 1; h.len() + 1];
    for (j, hw) in h.iter().enumerate() {
      let substitute = prev[j] + usize::from(rw != hw);
      cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
    }

    prev = cur;
  }

  prev[h.len()] as f64 / r.len() as f64
}

/// Lowercase words without punctuation; `ё` counts as `е`, hyphens and apostrophes split words.
pub fn words(text: &str) -> Vec<String> {
  text
    .to_lowercase()
    .replace('ё', "е")
    .split(|c: char| !c.is_alphanumeric())
    .filter(|w| !w.is_empty())
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn words_normalize_case_punctuation_and_yo() {
    assert_eq!(words("Ёлка, ЗЕЛЁНАЯ — что-то!"), ["елка", "зеленая", "что", "то"]);
    assert_eq!(words("  It's   fine.\n"), ["it", "s", "fine"]);
    assert!(words(" … ").is_empty());
  }

  #[test]
  fn wer_counts_substitutions_insertions_deletions() {
    assert_eq!(wer("раз два три четыре", "Раз, два, три, четыре."), 0.0);
    assert_eq!(wer("a b c d", "a x c d"), 0.25);
    assert_eq!(wer("a b c d", "a b d"), 0.25);
    assert_eq!(wer("a b c d", "a b c c d"), 0.25);
    assert_eq!(wer("a b", ""), 1.0);
    assert_eq!(wer("a b", "x y z w"), 2.0);
    assert_eq!(wer("", ""), 0.0);
  }
}
