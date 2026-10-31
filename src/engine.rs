// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Speech engine: one "engine" thread owns the model. It loads it on demand (or on a prewarm), runs
//! the jobs one at a time, dictations first, and drops the model when idle. A panic in wavo fails
//! its job, the model goes, and the next job loads a fresh one; a native crash ends the app.
//!
//! A model for the Neural Engine loads on a thread of its own: macOS purges Core ML's compiled
//! cache when the disk runs low, and the load then compiles again for 10–14 s. A job doesn't wait
//! for that: it runs on the GPU meanwhile, and the Neural Engine's model takes over between jobs.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossbeam_channel::{
  Receiver, RecvTimeoutError, Sender, after, bounded, never, select, unbounded,
};
use tracing::{debug, info, warn};
use wavo::Backend;

// MARK: Models

pub trait Loader: Send + Sync {
  /// `backend`: [Backend::Auto] takes the Neural Engine when the file is prepared for it, else the
  /// GPU; [Backend::NeuralEngine] fails without it.
  fn load(&self, path: &Path, backend: Backend) -> Result<Model>;

  /// The file is prepared for the Neural Engine of this Mac: its load goes on a thread of its own.
  fn ane_ready(&self, _path: &Path) -> bool {
    false
  }

  /// Prepares the file for the Neural Engine in the background, after those asked for before;
  /// `done` once it is.
  fn prepare(&self, _path: &Path, _done: Done) {}
}

/// Lives on the engine thread; the Neural Engine's is loaded on a thread of its own.
pub trait SpeechModel {
  /// `Apple M2 (Metal, integrated)`
  fn device(&self) -> String;

  /// Text of 16 kHz mono audio, cut at pauses into pieces of the model's window. `on_piece(index,
  /// count)` follows each piece; a break stops the run there with an error.
  fn transcribe(
    &self,
    pcm: &[f32],
    language: Option<&str>,
    on_piece: &mut dyn FnMut(usize, usize) -> ControlFlow<()>,
  ) -> Result<String>;
}

pub type Model = Box<dyn SpeechModel + Send>;

pub struct Wavo;

impl Loader for Wavo {
  fn load(&self, path: &Path, backend: Backend) -> Result<Model> {
    Ok(Box::new(wavo::Model::load_with(path, backend)?))
  }

  fn ane_ready(&self, path: &Path) -> bool {
    has_neural_engine() && wavo::Model::prepared(path) == Some(true)
  }

  fn prepare(&self, path: &Path, done: Done) {
    if has_neural_engine() && wavo::Model::prepared(path) == Some(false) {
      PREPARING.add(path, done);
    }
  }
}

// MARK: Neural Engine

/// This Mac has a Neural Engine wavo can use: Apple silicon on macOS 15 or later. Off macOS there
/// is none, and the GPUs are not woken up to find out.
pub fn has_neural_engine() -> bool {
  static HAS: OnceLock<bool> = OnceLock::new();
  *HAS.get_or_init(|| {
    let devices = || wavo::devices().unwrap_or_default();
    cfg!(target_os = "macos") && devices().iter().any(|d| d.kind == wavo::DeviceKind::NeuralEngine)
  })
}

/// Called once a file is prepared for the Neural Engine.
pub type Done = Box<dyn FnOnce() + Send>;

/// Parakeet and GigaAM files to prepare for the Neural Engine, in order, on a thread of their own,
/// one at a time: the system compiles each once per app binary and macOS version (10–30 s), and
/// two compiles at once take as long together and twice the memory. The Core ML model itself is
/// in wavo's cache, shared with the wavo CLI; until it is prepared a model runs on the GPU.
struct Preparing {
  /// The first one is being prepared.
  queue: Mutex<VecDeque<(PathBuf, Done)>>,
  /// Prepares a file: whether it was prepared now.
  run: fn(&Path) -> Result<bool>,
}

static PREPARING: Preparing = Preparing { queue: Mutex::new(VecDeque::new()), run: prepare_file };

fn prepare_file(path: &Path) -> Result<bool> {
  Ok(wavo::Model::prepare(path, |_| ControlFlow::Continue(()))?)
}

impl Preparing {
  /// Queues `path` unless it is queued already; `done` once it is prepared.
  fn add(&'static self, path: &Path, done: Done) {
    let mut queue = self.queue.lock().unwrap();
    if queue.iter().any(|(p, _)| p == path) {
      return;
    }

    queue.push_back((path.to_owned(), done));
    // The thread at work takes it after the ones before.
    if queue.len() > 1 {
      return;
    }

    drop(queue);
    let spawn = thread::Builder::new().name("ane-prepare".into()).spawn(move || self.work());
    if let Err(e) = spawn {
      warn!("the Neural Engine's preparation did not start: {e}");
      self.queue.lock().unwrap().clear();
    }
  }

  /// Prepares the queued files one after another, until none is left.
  fn work(&self) {
    loop {
      let Some(path) = self.queue.lock().unwrap().front().map(|(p, _)| p.clone()) else { return };
      let started = Instant::now();
      // A panic in wavo fails this file alone: the queue goes on.
      let prepared = panic::catch_unwind(AssertUnwindSafe(|| (self.run)(&path)));
      let prepared = prepared.unwrap_or_else(|p| Err(anyhow!("panicked: {}", panic_message(&*p))));
      // Taken off and looked at under one lock: a file queued meanwhile is this thread's.
      let (done, more) = {
        let mut queue = self.queue.lock().unwrap();
        let (_, done) = queue.pop_front().unwrap();
        (done, !queue.is_empty())
      };

      match prepared {
        Ok(true) => {
          info!("{}: prepared for the Neural Engine in {:.1?}", path.display(), started.elapsed());
          done();
        }
        Ok(false) => {}
        Err(e) => warn!("{}: not prepared for the Neural Engine: {e:#}", path.display()),
      }

      if !more {
        return;
      }
    }
  }
}

/// Parakeet's pieces on the GPU, short of the 60–70 s where it loses speech.
const PARAKEET_WINDOW_MS: u32 = 25_000;

impl SpeechModel for wavo::Model {
  fn device(&self) -> String {
    wavo::Model::device(self).to_string()
  }

  /// A language the model cannot be told (any for Parakeet v3, another than its own for a model of
  /// one) is not sent: the model goes by the speech.
  fn transcribe(
    &self,
    pcm: &[f32],
    language: Option<&str>,
    on_piece: &mut dyn FnMut(usize, usize) -> ControlFlow<()>,
  ) -> Result<String> {
    let mut options = wavo::Options::default();
    let listed = language.and_then(|c| self.languages().iter().find(|l| l.eq_ignore_ascii_case(c)));
    match (language, listed) {
      (_, Some(code)) if self.takes_language() => options.language = Some(code.clone()),
      (Some(code), _) => warn!("language {code} not sent: the model cannot be told it"),
      (None, _) => {}
    }

    // Parakeet on the GPU has no window of its own: wavo cuts it at 60 s, where it silently drops
    // seconds of speech (https://github.com/handy-computer/transcribe.cpp/issues/71). On the
    // Neural Engine its pieces stay within 15 s anyway.
    if self.max_audio_ms().is_none() {
      options.window_ms = Some(PARAKEET_WINDOW_MS);
    }

    let transcript = self.transcribe_long(pcm, &options, |p| on_piece(p.index, p.count))?;
    Ok(transcript.text.trim().to_owned())
  }
}

// MARK: Engine

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
  #[error("could not load the model: {0}")]
  Load(String),
  #[error("transcription failed: {0}")]
  Failed(String),
  #[error("the transcription was cancelled")]
  Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
  /// No model in memory.
  #[default]
  Stopped,
  Loading,
  Idle,
  Running,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
  pub phase: Phase,
  /// Where the loaded model runs: `Apple M2 Neural Engine (Core ML)`.
  pub device: Option<String>,
  /// Finished and planned pieces of the job in progress.
  pub progress: Option<(usize, usize)>,
  /// Why the last load failed, until one succeeds.
  pub load_error: Option<String>,
}

pub enum Event {
  /// [`Engine::status`] changed.
  Changed,
  /// The text of job `id`.
  Done(u64, Result<String, EngineError>),
}

/// Dropping it, and its clones, lets the thread finish the queued jobs, then it drops the model
/// and ends.
#[derive(Clone)]
pub struct Engine {
  tx: Sender<Msg>,
  status: Arc<Mutex<Status>>,
}

enum Msg {
  Prewarm,
  Release,
  Unload,
  Job(Job),
  Cancel(u64),
}

struct Job {
  id: u64,
  pcm: Vec<f32>,
  language: Option<String>,
  background: bool,
}

/// A prewarmed model waits this long for its job even with a shorter `unload_after`: the
/// recording it was loaded for is still going.
const PREWARM_HOLD: Duration = Duration::from_secs(5 * 60);
/// A job waits this long for the Neural Engine's load, which takes 0.07–0.15 s from Core ML's
/// cache, before it runs on the GPU.
const ANE_GRACE: Duration = Duration::from_millis(300);

/// The Neural Engine's load, from its thread: the model or why not, and how long it took.
type Loaded = (Result<Model, String>, Duration);

impl Engine {
  pub fn start(
    loader: Arc<dyn Loader>,
    path: PathBuf,
    ane: bool,
    unload_after: Option<Duration>,
    events: Sender<Event>,
  ) -> Self {
    let (tx, rx) = unbounded();
    let status = Arc::new(Mutex::new(Status::default()));
    let shared = status.clone();
    let run = move || {
      let worker = Worker {
        loader,
        path,
        ane,
        unload_after,
        rx,
        events,
        status: shared,
        model: None,
        stand_in: false,
        pending: None,
        queue: VecDeque::new(),
        cancelled: Vec::new(),
        hold: false,
        prewarm: false,
        unload: false,
      };

      worker.run();
    };

    thread::Builder::new().name("engine".into()).spawn(run).expect("spawn the engine thread");
    Self { tx, status }
  }

  /// Loads the model ahead of a job, as a recording starts; it stays until the job or a release.
  pub fn prewarm(&self) {
    self.send(Msg::Prewarm);
  }

  /// The recording of a prewarm ended without a job: the idle unload counts again.
  pub fn release(&self) {
    self.send(Msg::Release);
  }

  /// Drops the model after the job in progress.
  pub fn unload(&self) {
    self.send(Msg::Unload);
  }

  /// `background` jobs go after dictations, and yield to one after their piece in progress.
  pub fn transcribe(&self, id: u64, pcm: Vec<f32>, language: Option<String>, background: bool) {
    self.send(Msg::Job(Job { id, pcm, language, background }));
  }

  /// A running job stops after its piece in progress.
  pub fn cancel(&self, id: u64) {
    self.send(Msg::Cancel(id));
  }

  pub fn status(&self) -> Status {
    self.status.lock().unwrap().clone()
  }

  fn send(&self, msg: Msg) {
    let _ = self.tx.send(msg);
  }
}

struct Worker {
  loader: Arc<dyn Loader>,
  path: PathBuf,
  /// Off after the Neural Engine failed a run: the GPU for the rest of the engine's life.
  ane: bool,
  unload_after: Option<Duration>,
  rx: Receiver<Msg>,
  events: Sender<Event>,
  status: Arc<Mutex<Status>>,
  model: Option<Model>,
  /// The model is the GPU's, standing in while the Neural Engine's loads.
  stand_in: bool,
  /// The Neural Engine's model loading on its thread.
  pending: Option<Receiver<Loaded>>,
  queue: VecDeque<Job>,
  /// Jobs to stop: the one running, or one that has finished already.
  cancelled: Vec<u64>,
  hold: bool,
  prewarm: bool,
  unload: bool,
}

impl Worker {
  fn run(mut self) {
    while let Some(job) = self.next_job() {
      self.run_job(job);
      self.cancelled.clear();
    }

    self.drop_model();
  }

  /// `None` once the [`Engine`] is dropped and the queue is empty.
  fn next_job(&mut self) -> Option<Job> {
    loop {
      if std::mem::take(&mut self.unload) {
        self.drop_model();
      }

      if std::mem::take(&mut self.prewarm) {
        match self.on_ane() {
          true => self.start_ane(),
          false => _ = self.load(),
        }
      }

      let at = self.queue.iter().position(|j| !j.background).unwrap_or(0);
      if let Some(job) = self.queue.remove(at) {
        return Some(job);
      }

      let idle = match (self.hold, self.unload_after) {
        _ if self.model.is_none() => None,
        (true, after) => after.map(|a| a.max(PREWARM_HOLD)),
        (false, after) => after,
      };

      let (rx, pending) = (self.rx.clone(), self.pending.clone().unwrap_or_else(never));
      let timeout = idle.map_or_else(never, after);
      select! {
        recv(rx) -> msg => match msg {
          Ok(msg) => self.on_msg(msg),
          Err(_) => return None,
        },
        recv(pending) -> loaded => self.loaded(loaded.ok()),
        recv(timeout) -> _ => {
          info!("engine idle, unloading");
          self.drop_model();
        }
      }
    }
  }

  /// Notes what a message asks for; it can come in the middle of a run.
  fn on_msg(&mut self, msg: Msg) {
    match msg {
      Msg::Prewarm => (self.hold, self.prewarm) = (true, true),
      Msg::Release => self.hold = false,
      Msg::Unload => self.unload = true,
      Msg::Job(job) => {
        self.hold &= job.background;
        self.queue.push_back(job);
      }
      Msg::Cancel(id) => match self.queue.iter().position(|j| j.id == id) {
        Some(i) => {
          self.queue.remove(i);
          self.done(id, Err(EngineError::Cancelled));
        }
        None => self.cancelled.push(id),
      },
    }
  }

  /// The model loads for the Neural Engine, on a thread of its own.
  fn on_ane(&self) -> bool {
    self.ane && self.loader.ane_ready(&self.path)
  }

  /// A model for the job: the Neural Engine's if it comes within [ANE_GRACE], else the GPU's while
  /// it goes on loading.
  fn load(&mut self) -> Result<(), String> {
    if self.model.is_some() {
      return Ok(());
    }

    if !self.on_ane() {
      return self.load_here(self.ane, false);
    }

    self.start_ane();
    if let Some(pending) = self.pending.clone() {
      match pending.recv_timeout(ANE_GRACE) {
        Ok(loaded) => self.loaded(Some(loaded)),
        Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => self.pending = None,
      }
    }

    match self.model {
      Some(_) => Ok(()),
      None => self.load_here(false, self.pending.is_some()),
    }
  }

  /// Loads on the engine thread.
  fn load_here(&mut self, ane: bool, stand_in: bool) -> Result<(), String> {
    self.set(|s| s.phase = Phase::Loading);
    let started = Instant::now();
    let backend = if ane { Backend::Auto } else { Backend::Gpu };
    match guarded(|| self.loader.load(&self.path, backend)) {
      Ok(model) => {
        self.install(model, started.elapsed(), stand_in);
        Ok(())
      }
      Err(e) => {
        warn!("engine could not load {}: {e}", self.path.display());
        self.set(|s| (s.phase, s.load_error) = (Phase::Stopped, Some(e.clone())));
        Err(e)
      }
    }
  }

  /// Starts the Neural Engine's load on its thread, unless it runs or loads already.
  fn start_ane(&mut self) {
    if self.pending.is_some() || (self.model.is_some() && !self.stand_in) {
      return;
    }

    if self.model.is_none() {
      self.set(|s| s.phase = Phase::Loading);
    }

    let (tx, rx) = bounded(1);
    let (loader, path) = (self.loader.clone(), self.path.clone());
    let load = move || {
      let started = Instant::now();
      // Strictly the Neural Engine: Auto would load a second GPU model when Core ML fails.
      let model = guarded(|| loader.load(&path, Backend::NeuralEngine));
      // Unloaded meanwhile: the model goes with this thread.
      let _ = tx.send((model, started.elapsed()));
    };

    match thread::Builder::new().name("engine-ane".into()).spawn(load) {
      Ok(_) => self.pending = Some(rx),
      Err(e) => warn!("the Neural Engine's load did not start: {e}"),
    }
  }

  /// The Neural Engine's load ended: its model takes over from a GPU stand-in. After a failure the
  /// GPU runs the jobs for the rest of the engine's life.
  fn loaded(&mut self, loaded: Option<Loaded>) {
    self.pending = None;
    let Some((model, took)) = loaded else { return };
    match model {
      Ok(model) => {
        if let Some(stand_in) = self.model.take() {
          discard(stand_in);
        }

        self.install(model, took, false);
      }
      Err(e) => {
        warn!("engine could not load {} for the Neural Engine: {e}", self.path.display());
        // The GPU's stand-in, if any, stays for good.
        (self.ane, self.stand_in) = (false, false);
        if self.model.is_none() {
          self.set(|s| (s.phase, s.load_error) = (Phase::Stopped, Some(e)));
        }
      }
    }
  }

  fn install(&mut self, model: Model, took: Duration, stand_in: bool) {
    let (path, device) = (self.path.display(), model.device());
    let meanwhile = if stand_in { ", while the Neural Engine's loads" } else { "" };
    info!("engine loaded {path} on {device} in {took:.2?}{meanwhile}");
    (self.model, self.stand_in) = (Some(model), stand_in);
    self.set(|s| (s.phase, s.device, s.load_error) = (Phase::Idle, Some(device), None));
  }

  fn run_job(&mut self, job: Job) {
    if let Err(e) = self.load() {
      return self.done(job.id, Err(EngineError::Load(e)));
    }

    let model = self.model.take().unwrap();
    let device = model.device();
    self.set(|s| s.phase = Phase::Running);
    let started = Instant::now();
    let mut yielded = false;
    let run = panic::catch_unwind(AssertUnwindSafe(|| {
      model.transcribe(&job.pcm, job.language.as_deref(), &mut |index, count| {
        self.set(|s| s.progress = Some((index + 1, count)));
        while let Ok(msg) = self.rx.try_recv() {
          self.on_msg(msg);
        }

        // After the last piece the run is done anyway.
        let dictation = job.background && self.queue.iter().any(|j| !j.background);
        if index + 1 < count && (dictation || self.cancelled.contains(&job.id)) {
          yielded = dictation;
          return ControlFlow::Break(());
        }

        ControlFlow::Continue(())
      })
    }));

    let (id, took) = (job.id, started.elapsed());
    info!("job {id} ran on {device} in {took:.2?}, {} samples", job.pcm.len());
    self.model = Some(model);
    self.set(|s| (s.phase, s.progress) = (Phase::Idle, None));
    // After a panic or a lost GPU device the model may not work again.
    let error = match &run {
      Ok(Err(e)) => e.downcast_ref::<wavo::Error>(),
      _ => None,
    };

    // Auto would pick the Neural Engine again and fail the same way.
    if self.ane && matches!(error, Some(wavo::Error::NeuralEngine(_))) {
      warn!("the Neural Engine failed: the GPU from now on");
      self.ane = false;
    }

    if run.is_err() || error.is_some_and(wavo::Error::is_fatal) {
      self.drop_model();
    }

    // The Neural Engine's model, loaded meanwhile, takes over before the next job.
    if self.stand_in
      && let Some(loaded) = self.pending.as_ref().and_then(|p| p.try_recv().ok())
    {
      self.loaded(Some(loaded));
    }

    let result = match run {
      _ if self.cancelled.contains(&job.id) => Err(EngineError::Cancelled),
      Ok(Err(_)) if yielded => {
        debug!(id = job.id, "a dictation goes first: the background job starts over after it");
        return self.queue.push_front(job);
      }
      Ok(Ok(text)) => Ok(text),
      Ok(Err(e)) => Err(EngineError::Failed(format!("{e:#}"))),
      Err(panic) => Err(EngineError::Failed(format!("panicked: {}", panic_message(&*panic)))),
    };

    self.done(job.id, result);
  }

  /// The model goes, and a Neural Engine load still going on ends with its thread.
  fn drop_model(&mut self) {
    (self.pending, self.stand_in) = (None, false);
    if let Some(model) = self.model.take() {
      discard(model);
    }

    self.set(|s| (s.phase, s.device) = (Phase::Stopped, None));
  }

  fn done(&self, id: u64, result: Result<String, EngineError>) {
    let _ = self.events.send(Event::Done(id, result));
  }

  fn set(&self, edit: impl FnOnce(&mut Status)) {
    let mut status = self.status.lock().unwrap();
    let before = status.clone();
    edit(&mut status);
    if *status != before {
      drop(status);
      let _ = self.events.send(Event::Changed);
    }
  }
}

/// A load, a panic in it as its error.
fn guarded(load: impl FnOnce() -> Result<Model>) -> Result<Model, String> {
  match panic::catch_unwind(AssertUnwindSafe(load)) {
    Ok(Ok(model)) => Ok(model),
    Ok(Err(e)) => Err(format!("{e:#}")),
    Err(panic) => Err(format!("panicked: {}", panic_message(&*panic))),
  }
}

/// A model a panicking run left half-way may panic in its destructor too.
fn discard(model: Model) {
  if let Err(panic) = panic::catch_unwind(AssertUnwindSafe(move || drop(model))) {
    warn!("dropping the model panicked: {}", panic_message(&*panic));
  }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
  match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
    (Some(s), _) => (*s).to_owned(),
    (_, Some(s)) => s.clone(),
    _ => "a panic without a message".into(),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use super::*;
  use crate::audio::SAMPLE_RATE;
  use crate::testing::{FakeModels, TempDir, fixture, model_en, model_ru, poll, wer};

  const SECOND: usize = SAMPLE_RATE as usize;

  struct Fake {
    dir: TempDir,
    engine: Engine,
    events: Receiver<Event>,
  }

  impl Fake {
    fn start(name: &str, models: &FakeModels, unload_after: Option<Duration>) -> Self {
      Self::start_on(name, models, false, unload_after)
    }

    fn start_on(name: &str, models: &FakeModels, ane: bool, unload: Option<Duration>) -> Self {
      let dir = TempDir::new(name);
      fs::write(dir.path().join("model.gguf"), b"GGUF").unwrap();
      let (tx, events) = unbounded();
      let loader = Arc::new(models.clone());
      let engine = Engine::start(loader, dir.path().join("model.gguf"), ane, unload, tx);
      Self { dir, engine, events }
    }

    fn done(&self) -> (u64, Result<String, EngineError>) {
      loop {
        match self.events.recv_timeout(Duration::from_secs(10)).expect("a job done") {
          Event::Done(id, result) => return (id, result),
          Event::Changed => {}
        }
      }
    }

    fn run(&self, id: u64, samples: usize) -> Result<String, EngineError> {
      self.engine.transcribe(id, vec![0.0; samples], None, false);
      let (done, result) = self.done();
      assert_eq!(done, id);
      result
    }

    fn wait(&self, phase: Phase) {
      poll("the phase", Duration::from_secs(10), || {
        (self.engine.status().phase == phase).then_some(())
      });
    }
  }

  #[test]
  fn long_input_runs_in_pieces() {
    let models = FakeModels::new("text").window_ms(10_000);
    let f = Fake::start("engine-pieces", &models, None);
    assert_eq!(f.run(1, 25 * SECOND).unwrap(), "text text text");
    let runs = models.runs();
    assert_eq!(runs.iter().sum::<usize>(), 25 * SECOND);
    assert!(runs.iter().all(|&n| n <= 10 * SECOND), "{runs:?}");
    let idle = Status { phase: Phase::Idle, device: Some("fake".into()), ..Status::default() };
    assert_eq!(f.engine.status(), idle);
  }

  #[test]
  fn after_the_neural_engine_fails_the_model_runs_on_the_gpu() {
    let models = FakeModels::new("text");
    let f = Fake::start_on("engine-ane", &models, true, None);
    assert_eq!(f.run(1, SECOND).unwrap(), "text");

    models.ane_fails();
    assert!(matches!(f.run(2, SECOND), Err(EngineError::Failed(_))));
    assert_eq!(f.run(3, SECOND).unwrap(), "text", "a fresh model on the GPU");
    assert_eq!(models.ane_loads(), [true, false]);
  }

  #[test]
  fn a_failed_load_fails_the_job_and_stays_in_the_status() {
    let models = FakeModels::new("text");
    let f = Fake::start("engine-load-error", &models, None);
    let model = f.dir.path().join("model.gguf");
    fs::write(&model, b"BROKEN").unwrap();
    let error = "not a speech model".to_owned();
    assert_eq!(f.run(1, SECOND), Err(EngineError::Load(error.clone())));
    let status = f.engine.status();
    assert_eq!((status.phase, status.load_error), (Phase::Stopped, Some(error)));

    fs::write(&model, b"GGUF").unwrap();
    assert_eq!(f.run(2, SECOND).unwrap(), "text");
    assert_eq!(f.engine.status().load_error, None);
  }

  #[test]
  fn a_panic_in_a_run_fails_its_job_and_the_next_job_has_a_fresh_model() {
    let models = FakeModels::new("text");
    let f = Fake::start("engine-panic", &models, None);
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    models.panic_next();
    let Err(EngineError::Failed(e)) = f.run(2, SECOND) else { panic!("no failure") };
    assert!(e.starts_with("panicked: "), "{e}");
    assert_eq!((f.engine.status().phase, models.live()), (Phase::Stopped, 0));

    assert_eq!(f.run(3, SECOND).unwrap(), "text");
    assert_eq!((models.loads(), models.live()), (2, 1));
  }

  /// A background job gives way to a dictation after its piece in progress and starts over after
  /// it; a queued job cancelled goes at once, a running one after its piece.
  #[test]
  fn dictations_go_first_and_cancel_stops_after_the_piece() {
    let models = FakeModels::new("text").window_ms(10_000);
    let f = Fake::start("engine-queue", &models, None);
    let gate = models.hold();
    f.engine.transcribe(1, vec![0.0; 25 * SECOND], None, true);
    f.wait(Phase::Running);
    f.engine.transcribe(2, vec![0.0; 1000], None, false);
    f.engine.transcribe(3, vec![0.0; 2000], None, false);
    f.engine.cancel(3);
    drop(gate);
    assert_eq!(f.done(), (3, Err(EngineError::Cancelled)));
    assert_eq!(f.done(), (2, Ok("text".into())));
    assert_eq!(f.done(), (1, Ok("text text text".into())));
    let runs = models.runs();
    assert_eq!((runs.len(), runs[1]), (5, 1000), "{runs:?}");

    let gate = models.hold();
    f.engine.transcribe(4, vec![0.0; 25 * SECOND], None, false);
    f.wait(Phase::Running);
    f.engine.cancel(4);
    drop(gate);
    assert_eq!(f.done(), (4, Err(EngineError::Cancelled)));
    assert_eq!(models.runs().len(), 6, "the rest never ran");
  }

  /// With an unload right away a prewarmed model still waits for its job; a release ends that.
  #[test]
  fn an_idle_engine_unloads_after_a_prewarm_waited_for_its_job() {
    let models = FakeModels::new("text");
    let f = Fake::start("engine-unload", &models, Some(Duration::ZERO));
    f.engine.prewarm();
    f.wait(Phase::Idle);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(f.engine.status().phase, Phase::Idle, "held for its recording");
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    f.wait(Phase::Stopped);
    assert_eq!(models.live(), 0);

    f.engine.prewarm();
    f.wait(Phase::Idle);
    f.engine.release();
    f.wait(Phase::Stopped);
    assert_eq!(models.loads(), 2);
  }

  /// A Neural Engine load that compiles again runs the job on the GPU; the next one goes to the
  /// Neural Engine, and the GPU's model is gone.
  #[test]
  fn a_slow_neural_engine_load_leaves_the_job_to_the_gpu() {
    let models = FakeModels::new("text").on_ane(Duration::from_millis(800));
    let f = Fake::start_on("engine-ane-slow", &models, true, None);
    f.engine.prewarm();
    let started = Instant::now();
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    assert!(started.elapsed() < Duration::from_millis(700), "{:?}", started.elapsed());
    poll("the Neural Engine's model", Duration::from_secs(10), || {
      (models.loads() == 2 && models.live() == 1).then_some(())
    });
    assert_eq!(f.run(2, SECOND).unwrap(), "text");
    assert_eq!(models.jobs_on_ane(), [false, true]);
  }

  /// From Core ML's cache the Neural Engine's model comes in time: no GPU model at all, prewarmed
  /// or not.
  #[test]
  fn a_cached_neural_engine_load_needs_no_gpu() {
    let models = FakeModels::new("text").on_ane(Duration::from_millis(50));
    let f = Fake::start_on("engine-ane-cached", &models, true, None);
    f.engine.prewarm();
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    f.engine.unload();
    f.wait(Phase::Stopped);
    assert_eq!(f.run(2, SECOND).unwrap(), "text");
    assert_eq!((models.ane_loads(), models.jobs_on_ane()), (vec![true; 2], vec![true; 2]));
  }

  /// A Neural Engine load that fails (Core ML refuses the program) leaves the jobs to the GPU for
  /// good: no second GPU model, no further Neural Engine load.
  #[test]
  fn a_failed_neural_engine_load_keeps_the_gpu() {
    let models = FakeModels::new("text").on_ane(Duration::from_millis(500));
    models.ane_load_fails();
    let f = Fake::start_on("engine-ane-failed", &models, true, None);
    f.engine.prewarm();
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    poll("the Neural Engine's load to fail", Duration::from_secs(10), || {
      (models.ane_loads().len() == 2).then_some(())
    });
    assert_eq!(f.run(2, SECOND).unwrap(), "text");
    assert_eq!((models.ane_loads(), models.jobs_on_ane()), (vec![true, false], vec![false; 2]));
    assert_eq!((models.loads(), models.live()), (1, 1));
  }

  /// An unload while the Neural Engine's model loads drops the GPU's at once, and the Neural
  /// Engine's once its load ends.
  #[test]
  fn an_unload_during_a_neural_engine_load_drops_both() {
    let models = FakeModels::new("text").on_ane(Duration::from_millis(500));
    let f = Fake::start_on("engine-ane-unload", &models, true, None);
    assert_eq!(f.run(1, SECOND).unwrap(), "text");
    f.engine.unload();
    f.wait(Phase::Stopped);
    poll("both models gone", Duration::from_secs(10), || {
      (models.loads() == 2 && models.live() == 0).then_some(())
    });
    assert_eq!(f.engine.status().device, None);
  }

  /// Files are prepared for the Neural Engine one at a time, in the order asked, each once.
  #[test]
  fn files_are_prepared_one_at_a_time_in_order() {
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static MOST: AtomicUsize = AtomicUsize::new(0);
    fn slow(_: &Path) -> Result<bool> {
      MOST.fetch_max(RUNNING.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
      thread::sleep(Duration::from_millis(50));
      RUNNING.fetch_sub(1, Ordering::SeqCst);
      Ok(true)
    }

    static QUEUE: Preparing = Preparing { queue: Mutex::new(VecDeque::new()), run: slow };
    let (tx, done) = unbounded();
    for name in ["a", "b", "a", "c"] {
      let tx = tx.clone();
      QUEUE.add(Path::new(name), Box::new(move || _ = tx.send(name)));
    }

    let order: Vec<&str> =
      (0..3).map(|_| done.recv_timeout(Duration::from_secs(10)).unwrap()).collect();
    assert_eq!(order, ["a", "b", "c"], "the second a goes with the first");
    assert_eq!(MOST.load(Ordering::SeqCst), 1);
  }

  /// A panic preparing one file leaves the files after it to be prepared.
  #[test]
  fn a_panic_preparing_a_file_stops_no_other() {
    fn flaky(path: &Path) -> Result<bool> {
      assert_ne!(path, Path::new("bad"), "injected panic in a preparation");
      Ok(true)
    }

    static QUEUE: Preparing = Preparing { queue: Mutex::new(VecDeque::new()), run: flaky };
    let (tx, done) = unbounded();
    for name in ["bad", "good"] {
      let tx = tx.clone();
      QUEUE.add(Path::new(name), Box::new(move || _ = tx.send(name)));
    }

    assert_eq!(done.recv_timeout(Duration::from_secs(10)), Ok("good"));
  }

  // MARK: Real models

  const MAX_WER: f64 = 0.15;

  /// Each fixture after a warm-up run; prints WER and timings.
  fn check_wer(model: PathBuf, cases: &[(&str, Option<&str>)]) {
    let name = model.file_stem().unwrap().to_string_lossy().into_owned();
    let (tx, events) = unbounded();
    let engine = Engine::start(Arc::new(Wavo), model, false, None, tx);
    let mut failed = Vec::new();
    for (id, &(fixture_name, lang)) in cases.iter().cycle().take(cases.len() + 1).enumerate() {
      let f = fixture(fixture_name);
      let started = Instant::now();
      engine.transcribe(id as u64, f.input().pcm, lang.map(str::to_owned), false);
      let text = loop {
        if let Event::Done(_, result) = events.recv().unwrap() {
          break result.unwrap();
        }
      };

      let w = wer(&f.text, &text);
      let took = started.elapsed();
      eprintln!("{name} {fixture_name}: WER {:.1}% in {took:.2?}\n  {text}", w * 100.0);
      if id > 0 && w > MAX_WER {
        failed.push(fixture_name);
      }
    }

    assert!(failed.is_empty(), "{name}: WER above {:.0}% on {failed:?}", MAX_WER * 100.0);
  }

  #[test]
  #[ignore = "needs models: make test-models"]
  fn gigaam_ru_wer() {
    check_wer(model_ru(), &[("ru-short", None), ("ru-typical", None)]);
  }

  #[test]
  #[ignore = "needs models: make test-models"]
  fn whisper_ru_en_wer() {
    let cases = [
      ("ru-short", Some("ru")),
      ("ru-typical", None),
      ("en-short", Some("en")),
      ("en-paragraph", None),
      ("en-short-44k", None),
    ];
    check_wer(model_en(), &cases);
  }
}
