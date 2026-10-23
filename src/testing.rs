// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Test helpers: temp dirs, speech fixtures, real models and fake ones in a cache, a fake engine
//! and faults to inject, the app binary and WER.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::{env, fs};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::audio::vad::Clip;
use crate::audio::{SAMPLE_RATE, wav};
use crate::engine::model::{Loader, SpeechModel};

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

/// A `target/fixtures/<name>.wav` from `make fixtures`, with its reference transcript.
pub struct Fixture {
  pub name: String,
  pub pcm: Vec<f32>,
  pub text: String,
}

impl Fixture {
  pub fn seconds(&self) -> f64 {
    self.pcm.len() as f64 / SAMPLE_RATE as f64
  }

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

/// The `wavo-app` binary next to this test binary (`target/<profile>/deps/..`). Unit tests cannot
/// run `main`, so `make test-models` builds it first.
pub fn app_exe() -> PathBuf {
  let test_exe = env::current_exe().unwrap();
  let dir = test_exe.parent().and_then(Path::parent).unwrap();
  let exe = dir.join(format!("wavo-app{}", env::consts::EXE_SUFFIX));
  assert!(exe.is_file(), "{} is missing: `cargo build --release` first", exe.display());
  exe
}

/// A stand-in for wavo, in the process as wavo is: it loads any file (on device `fake`) unless the
/// file is gone or starts with `BROKEN` (a load that fails, as wavo's does), and answers every
/// piece with its text, trimmed. Clones count the loads, the models in memory and the runs
/// together.
#[derive(Clone)]
pub struct FakeModels {
  text: String,
  max_audio_ms: Option<u32>,
  loads: Arc<AtomicUsize>,
  live: Arc<AtomicUsize>,
  /// The samples of each run, in order.
  runs: Arc<Mutex<Vec<usize>>>,
}

impl FakeModels {
  /// Models that take any length in one run.
  pub fn new(text: &str) -> Self {
    Self {
      text: text.trim().to_owned(),
      max_audio_ms: None,
      loads: Arc::default(),
      live: Arc::default(),
      runs: Arc::default(),
    }
  }

  /// Models trained on audio of up to `ms`.
  pub fn max_audio_ms(self, ms: u32) -> Self {
    Self { max_audio_ms: Some(ms), ..self }
  }

  /// Models loaded so far.
  pub fn loads(&self) -> usize {
    self.loads.load(Ordering::SeqCst)
  }

  /// Models in memory now: loaded and not dropped yet.
  pub fn live(&self) -> usize {
    self.live.load(Ordering::SeqCst)
  }

  /// How many samples each run so far had, in order.
  pub fn runs(&self) -> Vec<usize> {
    self.runs.lock().unwrap().clone()
  }
}

impl Loader for FakeModels {
  fn load(&self, path: &Path) -> anyhow::Result<Box<dyn SpeechModel>> {
    if fs::read(path)?.starts_with(b"BROKEN") {
      anyhow::bail!("not a speech model");
    }

    self.loads.fetch_add(1, Ordering::SeqCst);
    self.live.fetch_add(1, Ordering::SeqCst);
    Ok(Box::new(FakeModel { models: self.clone() }))
  }
}

struct FakeModel {
  models: FakeModels,
}

impl SpeechModel for FakeModel {
  fn device(&self) -> String {
    "fake".into()
  }

  fn max_audio_ms(&self) -> Option<u32> {
    self.models.max_audio_ms
  }

  fn transcribe(&self, pcm: &[f32], _language: Option<&str>) -> anyhow::Result<String> {
    self.models.runs.lock().unwrap().push(pcm.len());
    Ok(self.models.text.clone())
  }
}

impl Drop for FakeModel {
  fn drop(&mut self) {
    self.models.live.fetch_sub(1, Ordering::SeqCst);
  }
}

/// What a load or a run does instead of its own work, injected with [`Faults`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
  /// Panics, as a bug in wavo or wgpu would.
  Panic,
  /// Fails with this message, as a bad request does.
  Fail(&'static str),
  /// Fails as wavo does when the GPU device is lost.
  GpuLost,
  /// Blocks until [`Faults::release`], then does its work: a wedged GPU, or a run held while the
  /// test looks.
  Hang,
}

/// Faults for the loads and runs of the [`Loader`] that [`Faults::wrap`] wraps, taken in order.
/// Clones share them.
#[derive(Clone)]
pub struct Faults(Arc<FaultState>);

struct FaultState {
  loads: Mutex<VecDeque<Fault>>,
  runs: Mutex<VecDeque<Fault>>,
  /// A hang waits on the receiver until [`Faults::release`] replaces the pair, dropping the
  /// sender.
  gate: Mutex<(Sender<()>, Receiver<()>)>,
  hanging: AtomicUsize,
}

impl Faults {
  pub fn new() -> Self {
    Self(Arc::new(FaultState {
      loads: Mutex::default(),
      runs: Mutex::default(),
      gate: Mutex::new(bounded(0)),
      hanging: AtomicUsize::new(0),
    }))
  }

  /// The next load does `fault`.
  pub fn load(&self, fault: Fault) {
    self.0.loads.lock().unwrap().push_back(fault);
  }

  /// The next run does `fault`.
  pub fn run(&self, fault: Fault) {
    self.0.runs.lock().unwrap().push_back(fault);
  }

  /// Ends every [`Fault::Hang`] in progress.
  pub fn release(&self) {
    *self.0.gate.lock().unwrap_or_else(PoisonError::into_inner) = bounded(0);
  }

  /// Loads and runs held in a [`Fault::Hang`] now.
  pub fn hanging(&self) -> usize {
    self.0.hanging.load(Ordering::SeqCst)
  }

  pub fn wrap(&self, loader: impl Loader + 'static) -> Arc<dyn Loader> {
    Arc::new(Faulty { inner: Box::new(loader), faults: self.clone() })
  }

  fn take(&self, queue: &Mutex<VecDeque<Fault>>, what: &str) -> anyhow::Result<()> {
    let fault = queue.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
    match fault {
      Some(Fault::Panic) => panic!("injected panic in {what}"),
      Some(Fault::Fail(message)) => anyhow::bail!(message),
      Some(Fault::GpuLost) => return Err(wavo::Error::Gpu("device lost".into()).into()),
      Some(Fault::Hang) => {
        let gate = self.0.gate.lock().unwrap().1.clone();
        self.0.hanging.fetch_add(1, Ordering::SeqCst);
        let _ = gate.recv();
        self.0.hanging.fetch_sub(1, Ordering::SeqCst);
      }
      None => {}
    }

    Ok(())
  }
}

struct Faulty {
  inner: Box<dyn Loader>,
  faults: Faults,
}

impl Loader for Faulty {
  fn load(&self, path: &Path) -> anyhow::Result<Box<dyn SpeechModel>> {
    self.faults.take(&self.faults.0.loads, "the load")?;
    let inner = self.inner.load(path)?;
    Ok(Box::new(FaultyModel { inner, faults: self.faults.clone() }))
  }
}

struct FaultyModel {
  inner: Box<dyn SpeechModel>,
  faults: Faults,
}

impl SpeechModel for FaultyModel {
  fn device(&self) -> String {
    self.inner.device()
  }

  fn max_audio_ms(&self) -> Option<u32> {
    self.inner.max_audio_ms()
  }

  fn transcribe(&self, pcm: &[f32], language: Option<&str>) -> anyhow::Result<String> {
    self.faults.take(&self.faults.0.runs, "a run")?;
    self.inner.transcribe(pcm, language)
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

/// For each paragraph of `reference` (separated by blank lines), the share of its words that the
/// best word alignment with `hypothesis` matches exactly.
pub fn paragraph_recall(reference: &str, hypothesis: &str) -> Vec<f64> {
  let paragraphs: Vec<Vec<String>> =
    reference.split("\n\n").map(words).filter(|p| !p.is_empty()).collect();
  let r: Vec<&String> = paragraphs.iter().flatten().collect();
  let h = words(hypothesis);

  // d[i][j] = distance(r[..i], h[..j]); then walk back to find the matched reference words.
  let mut d = vec![vec![0usize; h.len() + 1]; r.len() + 1];
  for (i, row) in d.iter_mut().enumerate() {
    row[0] = i;
  }

  for (j, cell) in d[0].iter_mut().enumerate() {
    *cell = j;
  }

  for i in 1..=r.len() {
    for j in 1..=h.len() {
      let substitute = d[i - 1][j - 1] + usize::from(*r[i - 1] != h[j - 1]);
      d[i][j] = substitute.min(d[i - 1][j] + 1).min(d[i][j - 1] + 1);
    }
  }

  let mut matched = vec![false; r.len()];
  let (mut i, mut j) = (r.len(), h.len());
  while i > 0 && j > 0 {
    if *r[i - 1] == h[j - 1] && d[i][j] == d[i - 1][j - 1] {
      matched[i - 1] = true;
      (i, j) = (i - 1, j - 1);
    } else if d[i][j] == d[i - 1][j - 1] + 1 {
      (i, j) = (i - 1, j - 1);
    } else if d[i][j] == d[i - 1][j] + 1 {
      i -= 1;
    } else {
      j -= 1;
    }
  }

  let mut start = 0;
  paragraphs
    .iter()
    .map(|p| {
      let found = matched[start..start + p.len()].iter().filter(|&&m| m).count();
      start += p.len();
      found as f64 / p.len() as f64
    })
    .collect()
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

  #[test]
  fn paragraph_recall_counts_matched_words_per_paragraph() {
    let reference = "раз два три четыре\n\nпять шесть семь восемь";
    assert_eq!(
      paragraph_recall(reference, "Раз, два, три, четыре. Пять, шесть, семь, восемь."),
      [1.0, 1.0]
    );
    assert_eq!(paragraph_recall(reference, "раз два три четыре"), [1.0, 0.0]);
    assert_eq!(paragraph_recall(reference, "раз два x четыре пять шесть"), [0.75, 0.5]);
    assert_eq!(paragraph_recall(reference, ""), [0.0, 0.0]);
  }
}
