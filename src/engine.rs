// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Speech engine: one "engine" thread owns the model. It loads it on demand (or on a prewarm), runs
//! the jobs one at a time, dictations first, and drops the model when idle. A panic in wavo fails
//! its job, the model goes, and the next job loads a fresh one; a native crash ends the app.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use tracing::{debug, info, warn};

// MARK: Models

pub trait Loader: Send + Sync {
  fn load(&self, path: &Path) -> Result<Box<dyn SpeechModel>>;
}

/// Lives and is dropped on the engine thread that loaded it.
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

pub struct Wavo;

impl Loader for Wavo {
  fn load(&self, path: &Path) -> Result<Box<dyn SpeechModel>> {
    prepare(path);
    Ok(Box::new(wavo::Model::load(path)?))
  }
}

/// A Parakeet or GigaAM file not yet prepared for the Neural Engine (one wavo or another app
/// downloaded) is prepared on a thread of its own, 15–40 s once; until then it loads on the GPU.
fn prepare(path: &Path) {
  static PREPARING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
  if wavo::Model::prepared(path) != Some(false) {
    return;
  }

  let mut preparing = PREPARING.lock().unwrap();
  if preparing.iter().any(|p| p == path) {
    return;
  }

  preparing.push(path.to_owned());
  let path = path.to_owned();
  thread::spawn(move || {
    let started = Instant::now();
    match wavo::Model::prepare(&path, |_| ControlFlow::Continue(())) {
      Ok(_) => {
        info!("{}: prepared for the Neural Engine in {:?}", path.display(), started.elapsed())
      }
      Err(e) => warn!("{}: not prepared for the Neural Engine: {e}", path.display()),
    }

    PREPARING.lock().unwrap().retain(|p| *p != path);
  });
}

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

/// Dropping it lets the thread finish the queued jobs, then it drops the model and ends.
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

impl Engine {
  pub fn start(
    loader: Arc<dyn Loader>,
    path: PathBuf,
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
        unload_after,
        rx,
        events,
        status: shared,
        model: None,
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
  unload_after: Option<Duration>,
  rx: Receiver<Msg>,
  events: Sender<Event>,
  status: Arc<Mutex<Status>>,
  model: Option<Box<dyn SpeechModel>>,
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
        let _ = self.load();
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

      let msg = match idle {
        Some(idle) => self.rx.recv_timeout(idle),
        None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
      };

      match msg {
        Ok(msg) => self.on_msg(msg),
        Err(RecvTimeoutError::Timeout) => {
          info!("engine idle, unloading");
          self.drop_model();
        }
        Err(RecvTimeoutError::Disconnected) => return None,
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

  fn load(&mut self) -> Result<(), String> {
    if self.model.is_some() {
      return Ok(());
    }

    self.set(|s| s.phase = Phase::Loading);
    let started = Instant::now();
    let loaded = panic::catch_unwind(AssertUnwindSafe(|| self.loader.load(&self.path)));
    let loaded = match loaded {
      Ok(Ok(model)) => Ok(model),
      Ok(Err(e)) => Err(format!("{e:#}")),
      Err(panic) => Err(format!("panicked: {}", panic_message(&*panic))),
    };

    match loaded {
      Ok(model) => {
        let (path, device) = (self.path.display(), model.device());
        info!("engine loaded {path} on {device} in {:.2?}", started.elapsed());
        self.model = Some(model);
        self.set(|s| (s.phase, s.load_error) = (Phase::Idle, None));
        Ok(())
      }
      Err(e) => {
        warn!("engine could not load {}: {e}", self.path.display());
        self.set(|s| (s.phase, s.load_error) = (Phase::Stopped, Some(e.clone())));
        Err(e)
      }
    }
  }

  fn run_job(&mut self, job: Job) {
    if let Err(e) = self.load() {
      return self.done(job.id, Err(EngineError::Load(e)));
    }

    let model = self.model.take().unwrap();
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

    debug!(samples = job.pcm.len(), ms = started.elapsed().as_millis() as u64, "run");
    self.model = Some(model);
    self.set(|s| (s.phase, s.progress) = (Phase::Idle, None));
    // After a panic or a lost GPU device the model may not work again.
    let broken = match &run {
      Ok(Ok(_)) => false,
      Ok(Err(e)) => e.downcast_ref().is_some_and(wavo::Error::is_fatal),
      Err(_) => true,
    };

    if broken {
      self.drop_model();
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

  /// A model a panicking run left half-way may panic in its destructor too.
  fn drop_model(&mut self) {
    if let Some(model) = self.model.take()
      && let Err(panic) = panic::catch_unwind(AssertUnwindSafe(move || drop(model)))
    {
      warn!("dropping the model panicked: {}", panic_message(&*panic));
    }

    self.set(|s| s.phase = Phase::Stopped);
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
      let dir = TempDir::new(name);
      fs::write(dir.path().join("model.gguf"), b"GGUF").unwrap();
      let (tx, events) = unbounded();
      let loader = Arc::new(models.clone());
      let engine = Engine::start(loader, dir.path().join("model.gguf"), unload_after, tx);
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
    assert_eq!(f.engine.status(), Status { phase: Phase::Idle, ..Status::default() });
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

  // MARK: Real models

  const MAX_WER: f64 = 0.15;

  /// Each fixture after a warm-up run; prints WER and timings.
  fn check_wer(model: PathBuf, cases: &[(&str, Option<&str>)]) {
    let name = model.file_stem().unwrap().to_string_lossy().into_owned();
    let (tx, events) = unbounded();
    let engine = Engine::start(Arc::new(Wavo), model, None, tx);
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
