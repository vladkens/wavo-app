// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Supervisor of the engine thread, on a thread of its own ("engine-supervisor"), so it answers
//! requests, cancels and deadlines while a run takes seconds.
//!
//! - The engine thread ("engine-N") owns the model: it loads it on demand, runs the pieces it gets
//!   over a channel, sends its events back over another and drops the model when its channel
//!   closes. The model is never touched elsewhere.
//! - Panics: the load and every run are inside `catch_unwind`. A load that panics is a failed load
//!   ([`EngineError::Load`]); a run that panics fails its job ([`EngineError::Crashed`]). Either
//!   way the thread drops the model and ends, and the next job loads a fresh one on a new thread. A
//!   native crash (a segfault or an abort in a driver) is not a panic: it ends the app.
//! - Deadlines: model load [`LOAD_DEADLINE`], each run max([`REQUEST_MIN_DEADLINE`],
//!   [`REQUEST_DEADLINE_FACTOR`] × audio). A thread cannot be stopped from outside: one past its
//!   deadline is left to end on its own, its job fails with [`EngineError::TimedOut`] and the next
//!   job starts a fresh thread. **The stuck thread keeps its model (memory, GPU buffers) until its
//!   run returns**, then drops it and ends; what it answers is dropped. While [`MAX_STUCK`] of them
//!   are still stuck, jobs fail with [`EngineError::Stuck`] instead of loading yet another model.
//!
//! A job is cut by [`wavo::split`] into pieces of up to the model's
//! [`max_audio_ms`](SpeechModel::max_audio_ms), one run per piece. [`Engine::cancel`] drops a
//! queued job; a piece of it the thread is running goes on (wavo cannot stop a run midway) and its
//! result is dropped. The next job waits for that run, which ends within its deadline, rather than
//! loading a second model next to it.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{
  Receiver, RecvTimeoutError, Sender, TryRecvError, bounded, never, select, unbounded,
};
use tracing::{debug, info, warn};

use super::model::{Loader, SpeechModel, Wavo};
use crate::audio::SAMPLE_RATE;

/// wavo loads a model in ~0.1–0.4 s warm; a cold page cache for a 1.7 GB file takes seconds.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(60);
pub const REQUEST_MIN_DEADLINE: Duration = Duration::from_secs(60);
pub const REQUEST_DEADLINE_FACTOR: f64 = 3.0;
/// Engine threads past their deadline that may still hold their models before jobs fail with
/// [`EngineError::Stuck`]: each holds a model, and a wedged GPU would wedge every new one too.
pub const MAX_STUCK: usize = 2;

/// The number of the last engine thread started, in any engine: `engine-N` is one thread for the
/// life of the process.
static THREADS: AtomicU64 = AtomicU64::new(0);

/// How long the supervisor waits for an idle engine thread to drop its model and end.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct EngineConfig {
  /// What loads and runs the model: wavo, except in tests.
  pub loader: Arc<dyn Loader>,
  pub model: PathBuf,
  pub deadlines: Deadlines,
}

impl EngineConfig {
  /// wavo, default deadlines.
  pub fn new(model: PathBuf) -> Self {
    Self { loader: Arc::new(Wavo), model, deadlines: Deadlines::default() }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
  pub load: Duration,
  pub request_min: Duration,
  pub request_factor: f64,
}

impl Default for Deadlines {
  fn default() -> Self {
    Self {
      load: LOAD_DEADLINE,
      request_min: REQUEST_MIN_DEADLINE,
      request_factor: REQUEST_DEADLINE_FACTOR,
    }
  }
}

impl Deadlines {
  /// Deadline for transcribing `samples` of 16 kHz audio.
  pub fn request(&self, samples: usize) -> Duration {
    let audio = Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64);
    self.request_min.max(audio.mul_f64(self.request_factor))
  }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
  #[error("could not start the engine: {0}")]
  Spawn(String),
  #[error("could not load the model: {0}")]
  Load(String),
  #[error("the engine crashed {0}")]
  Crashed(String),
  #[error("the engine timed out: no answer within {0:?}")]
  TimedOut(Duration),
  #[error(
    "the engine is stuck: {0} runs past their deadline still hold their models; restart the app"
  )]
  Stuck(usize),
  #[error("transcription failed: {0}")]
  Failed(String),
  #[error("the engine has stopped")]
  Stopped,
  #[error("the transcription was cancelled")]
  Cancelled,
}

#[derive(Debug, Clone)]
pub struct Transcription {
  pub text: String,
  /// The device that produced the text ([`SpeechModel::device`]).
  pub device: String,
  /// From submission to result, including the model load.
  pub elapsed: Duration,
  /// Sample ranges of the input, one per engine run.
  pub pieces: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
  /// No engine thread: no model in memory (but in a stuck thread's).
  #[default]
  Stopped,
  Loading,
  Idle,
  Running,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
  #[cfg_attr(not(test), expect(dead_code, reason = "the session (step 6) is its first user"))]
  pub phase: Phase,
  /// The number of the engine thread (`engine-N`, unique in the process): a new one after a
  /// panic or a timeout.
  pub thread: Option<u64>,
  /// Thread start → model loaded, of the most recent engine thread.
  pub last_load: Option<Duration>,
}

/// Handle to the supervisor thread. Dropping it stops the engine thread and the supervisor.
pub struct Engine {
  requests: Sender<Request>,
  next_job: AtomicU64,
  /// The supervisor's status as of its last turn: read without waiting for its thread, which can
  /// be busy for a moment stopping the engine thread.
  status: Arc<Mutex<Status>>,
  thread: Option<JoinHandle<()>>,
}

/// Identifies a submitted job, for [`Engine::cancel`].
pub type JobId = u64;

/// A submitted transcription.
pub struct Pending {
  #[cfg_attr(not(test), expect(dead_code, reason = "the session (step 6) is its first user"))]
  id: JobId,
  reply: Receiver<Result<Transcription, EngineError>>,
}

impl Pending {
  #[cfg_attr(not(test), expect(dead_code, reason = "the session (step 6) is its first user"))]
  pub fn id(&self) -> JobId {
    self.id
  }

  pub fn wait(self) -> Result<Transcription, EngineError> {
    self.reply.recv().unwrap_or(Err(EngineError::Stopped))
  }
}

enum Request {
  Transcribe(Job),
  #[cfg_attr(not(test), expect(dead_code, reason = "the session (step 6) is its first user"))]
  Cancel(JobId),
  Quit,
}

impl Engine {
  pub fn start(config: EngineConfig) -> Self {
    let (requests, requests_rx) = unbounded();
    let (msgs_tx, msgs) = unbounded();
    let status = Arc::new(Mutex::new(Status::default()));
    let supervisor = Supervisor::new(config, msgs_tx, status.clone());
    let thread = thread::Builder::new()
      .name("engine-supervisor".into())
      .spawn(move || supervisor.run(requests_rx, msgs))
      .expect("spawn the engine supervisor thread");

    Self { requests, next_job: AtomicU64::new(1), status, thread: Some(thread) }
  }

  /// Queues a transcription of 16 kHz mono audio, the engine's input (see
  /// [`Clip::engine_input`](crate::audio::vad::Clip::engine_input)); `language` `None` leaves it
  /// to the model.
  pub fn transcribe(&self, pcm: Vec<f32>, language: Option<String>) -> Pending {
    let (reply, rx) = bounded(1);
    let id = self.next_job.fetch_add(1, Ordering::Relaxed);
    let job = Job {
      id,
      pcm,
      language,
      reply,
      submitted: Instant::now(),
      pieces: Vec::new(),
      texts: Vec::new(),
    };

    // If the supervisor is gone the job, and with it the reply sender, is dropped: `Stopped`.
    let _ = self.requests.send(Request::Transcribe(job));
    Pending { id, reply: rx }
  }

  /// Ends job `id` with [`EngineError::Cancelled`] at once. A run of a piece of it goes on and its
  /// result is dropped. Finished jobs are not affected.
  #[cfg_attr(not(test), expect(dead_code, reason = "the session (step 6) is its first user"))]
  pub fn cancel(&self, id: JobId) {
    let _ = self.requests.send(Request::Cancel(id));
  }

  /// As of the supervisor's last turn: a request sent just before may not show yet; a job's
  /// result does.
  pub fn status(&self) -> Status {
    self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
  }
}

impl Drop for Engine {
  fn drop(&mut self) {
    let _ = self.requests.send(Request::Quit);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

struct Job {
  id: JobId,
  pcm: Vec<f32>,
  language: Option<String>,
  reply: Sender<Result<Transcription, EngineError>>,
  submitted: Instant,
  /// Planned once the model is loaded.
  pieces: Vec<Range<usize>>,
  /// Texts of the finished pieces; the next piece to send is `pieces[texts.len()]`.
  texts: Vec<String>,
}

/// The live engine thread, as the supervisor sees it.
struct Worker {
  /// Its number: tells its messages from those of threads left behind.
  generation: u64,
  pieces: Sender<Piece>,
  /// Disconnected once the thread has ended, its model dropped.
  ended: Receiver<()>,
  spawned: Instant,
  /// The loaded model's, once loaded.
  model: Option<Loaded>,
  state: WorkerState,
}

/// What the supervisor knows of a loaded model.
#[derive(Debug, Clone)]
struct Loaded {
  device: String,
  max_audio_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum WorkerState {
  Loading {
    deadline: Instant,
  },
  Idle,
  /// `cancelled`: the run's job was cancelled, its result is dropped.
  Running {
    id: u64,
    deadline: Instant,
    timeout: Duration,
    cancelled: bool,
  },
}

/// Supervisor → engine thread: one piece of audio to transcribe.
struct Piece {
  id: u64,
  language: Option<String>,
  pcm: Vec<f32>,
}

/// Engine thread → supervisor.
struct WorkerMsg {
  generation: u64,
  event: Event,
}

enum Event {
  Loaded {
    model: Loaded,
    load_ms: u64,
  },
  /// The load failed or panicked; the thread has ended.
  LoadFailed(String),
  Result {
    id: u64,
    result: Result<String, String>,
  },
  /// The run panicked; the thread dropped its model and ended.
  Panicked(String),
}

struct Supervisor {
  config: EngineConfig,
  /// Handed to each engine thread.
  msgs_tx: Sender<WorkerMsg>,
  worker: Option<Worker>,
  next_id: u64,
  job: Option<Job>,
  queue: VecDeque<Job>,
  last_load: Option<Duration>,
  /// Engine threads left behind past their deadline, until they end ([`Worker::ended`]).
  stuck: Vec<Receiver<()>>,
  /// Where [`Engine::status`] reads the status.
  shared: Arc<Mutex<Status>>,
}

impl Supervisor {
  fn new(config: EngineConfig, msgs_tx: Sender<WorkerMsg>, shared: Arc<Mutex<Status>>) -> Self {
    Self {
      config,
      msgs_tx,
      worker: None,
      next_id: 0,
      job: None,
      queue: VecDeque::new(),
      last_load: None,
      stuck: Vec::new(),
      shared,
    }
  }

  fn run(mut self, requests: Receiver<Request>, msgs: Receiver<WorkerMsg>) {
    loop {
      self.pump();
      self.share();
      let timer = self.deadline().map(crossbeam_channel::at).unwrap_or_else(never);
      select! {
        recv(requests) -> request => match request {
          Ok(Request::Transcribe(job)) => self.queue.push_back(job),
          Ok(Request::Cancel(id)) => self.cancel(id),
          Ok(Request::Quit) | Err(_) => break,
        },
        recv(msgs) -> msg => {
          if let Ok(msg) = msg {
            self.on_worker(msg);
          }
        },
        recv(timer) -> _ => self.on_deadline(),
      }
    }

    self.stop_worker();
    self.share();
    for job in self.job.take().into_iter().chain(self.queue.drain(..)) {
      let _ = job.reply.send(Err(EngineError::Stopped));
    }
  }

  fn cancel(&mut self, id: JobId) {
    if let Some(i) = self.queue.iter().position(|j| j.id == id) {
      let job = self.queue.remove(i).unwrap();
      let _ = job.reply.send(Err(EngineError::Cancelled));
      debug!(id, "queued job cancelled");
      return;
    }

    if self.job.as_ref().is_none_or(|j| j.id != id) {
      return; // already finished
    }

    // A thread loading or idle between pieces stays as it is; one running a piece of the job
    // finishes it for nothing.
    if let Some(worker) = &mut self.worker
      && let WorkerState::Running { cancelled, .. } = &mut worker.state
    {
      info!("engine thread {}: transcription cancelled; its run ends unread", worker.generation);
      *cancelled = true;
    }

    self.finish(Err(EngineError::Cancelled));
  }

  /// Moves the current job forward: starts an engine thread for it, or sends its next piece.
  fn pump(&mut self) {
    loop {
      if self.job.is_none() {
        self.job = self.queue.pop_front();
      }

      if self.job.is_none() {
        return;
      }

      if self.worker.is_none()
        && let Err(e) = self.spawn()
      {
        self.finish(Err(e));
        continue;
      }

      let (Some(job), Some(worker)) = (&mut self.job, &mut self.worker) else { return };
      // Loading, or running a piece (a cancelled job's too: it ends within its deadline).
      let (WorkerState::Idle, Some(model)) = (worker.state, &worker.model) else { return };
      if job.pieces.is_empty() {
        job.pieces = wavo::split(&job.pcm, model.max_audio_ms.unwrap_or(u32::MAX));
        if job.pieces.len() > 1 {
          let seconds: Vec<String> = (job.pieces.iter())
            .map(|p| format!("{:.1}", p.len() as f64 / SAMPLE_RATE as f64))
            .collect();
          debug!("{} pieces of {} s", job.pieces.len(), seconds.join(" / "));
        }
      }

      let Some(range) = job.pieces.get(job.texts.len()).cloned() else {
        let device = model.device.clone();
        self.complete(device);
        continue;
      };

      self.next_id += 1;
      let id = self.next_id;
      let samples = range.len();
      let piece = Piece { id, language: job.language.clone(), pcm: job.pcm[range].to_vec() };
      if worker.pieces.send(piece).is_err() {
        // The thread ends only after saying why (or when told to): this is a bug.
        warn!("engine thread {} is gone without a word", worker.generation);
        self.worker = None;
        self.finish(Err(EngineError::Crashed("without a word".into())));
        continue;
      }

      let timeout = self.config.deadlines.request(samples);
      let deadline = Instant::now() + timeout;
      worker.state = WorkerState::Running { id, deadline, timeout, cancelled: false };
      debug!(id, samples, ?timeout, "piece sent");
      return;
    }
  }

  fn on_worker(&mut self, msg: WorkerMsg) {
    if self.worker.as_ref().map(|w| w.generation) != Some(msg.generation) {
      return; // from a thread left behind
    }

    let worker = self.worker.as_mut().unwrap();
    let generation = worker.generation;
    match msg.event {
      Event::Loaded { model, load_ms } => {
        let total = worker.spawned.elapsed();
        info!(
          "engine thread {generation} loaded {} on {} in {total:.2?} (model {load_ms} ms)",
          self.config.model.display(),
          model.device,
        );
        worker.model = Some(model);
        worker.state = WorkerState::Idle;
        self.last_load = Some(total);
      }

      Event::LoadFailed(error) => {
        warn!("engine could not load {}: {error}", self.config.model.display());
        self.worker = None; // the thread has ended
        if self.job.is_some() {
          self.finish(Err(EngineError::Load(error)));
        }
      }

      Event::Result { id, result } => {
        let WorkerState::Running { id: expected, cancelled, .. } = worker.state else {
          warn!("engine thread {generation} sent a result for {id} while not running");
          return;
        };

        if id != expected {
          warn!("engine thread {generation} sent a result for {id}, expected {expected}");
          return;
        }

        worker.state = WorkerState::Idle;
        if cancelled {
          debug!(id, "the result of a cancelled job dropped");
          return;
        }

        let device = worker.model.as_ref().map(|m| m.device.clone()).unwrap_or_default();
        let Some(job) = &mut self.job else { return };
        match result {
          Err(error) => self.finish(Err(EngineError::Failed(error))),
          Ok(text) => {
            job.texts.push(text);
            if job.texts.len() == job.pieces.len() {
              self.complete(device);
            }
          }
        }
      }

      Event::Panicked(message) => {
        let state = worker.state;
        self.worker = None; // the thread has dropped its model and ended
        if let WorkerState::Running { cancelled: true, .. } = state {
          warn!("engine thread {generation} panicked in a cancelled run: {message}");
          return;
        }

        warn!("engine thread {generation} panicked during transcription: {message}");
        if self.job.is_some() {
          let error = format!("during transcription (panicked: {message})");
          self.finish(Err(EngineError::Crashed(error)));
        }
      }
    }
  }

  fn on_deadline(&mut self) {
    if self.deadline().is_none_or(|due| Instant::now() < due) {
      return; // a timer for a state already left
    }

    let Some(worker) = &self.worker else { return };
    let generation = worker.generation;
    let (what, timeout) = match worker.state {
      WorkerState::Idle => return,
      WorkerState::Loading { .. } => ("model load", self.config.deadlines.load),
      WorkerState::Running { timeout, cancelled: true, .. } => {
        warn!(
          "engine thread {generation}: a cancelled run took longer than {timeout:?}; it is left \
           to end on its own, holding its model until then"
        );
        self.leave_stuck();
        return;
      }
      WorkerState::Running { timeout, .. } => ("transcription", timeout),
    };

    warn!(
      "engine thread {generation}: {what} took longer than {timeout:?}; it is left to end on its \
       own, holding its model until then, and a fresh thread takes the next job"
    );
    self.leave_stuck();
    if self.job.is_some() {
      self.finish(Err(EngineError::TimedOut(timeout)));
    }
  }

  /// When the supervisor must wake up without a message: the deadline of a load or a run.
  fn deadline(&self) -> Option<Instant> {
    match self.worker.as_ref()?.state {
      WorkerState::Loading { deadline } | WorkerState::Running { deadline, .. } => Some(deadline),
      WorkerState::Idle => None,
    }
  }

  /// Starts an engine thread that loads the model.
  fn spawn(&mut self) -> Result<(), EngineError> {
    self.stuck.retain(|ended| matches!(ended.try_recv(), Err(TryRecvError::Empty)));
    if self.stuck.len() >= MAX_STUCK {
      return Err(EngineError::Stuck(self.stuck.len()));
    }

    let generation = THREADS.fetch_add(1, Ordering::Relaxed) + 1;
    let (pieces, pieces_rx) = unbounded();
    let (ended_tx, ended) = bounded::<()>(0);
    let loader = self.config.loader.clone();
    let path = self.config.model.clone();
    let msgs = self.msgs_tx.clone();
    let spawned = Instant::now();
    thread::Builder::new()
      .name(format!("engine-{generation}"))
      .spawn(move || {
        // Dropped as the thread ends, after the model.
        let _ended = ended_tx;
        serve(generation, &*loader, &path, &pieces_rx, &msgs);
      })
      .map_err(|e| EngineError::Spawn(e.to_string()))?;

    debug!(generation, "engine thread started");
    let state = WorkerState::Loading { deadline: spawned + self.config.deadlines.load };
    self.worker = Some(Worker { generation, pieces, ended, spawned, model: None, state });
    Ok(())
  }

  /// Ends the engine thread: an idle one drops its model before this returns (up to
  /// [`SHUTDOWN_GRACE`]); a busy one when its load or run returns.
  fn stop_worker(&mut self) {
    let Some(Worker { generation, pieces, ended, state, .. }) = self.worker.take() else { return };
    drop(pieces); // the end of its input
    if !matches!(state, WorkerState::Idle) {
      debug!(generation, ?state, "engine thread left to end after its load or run");
      return;
    }

    let started = Instant::now();
    match ended.recv_timeout(SHUTDOWN_GRACE) {
      Err(RecvTimeoutError::Disconnected) => {
        debug!(generation, "engine thread ended, its model dropped in {:.2?}", started.elapsed());
      }
      _ => warn!("engine thread {generation} still drops its model after {SHUTDOWN_GRACE:?}"),
    }
  }

  /// Leaves the engine thread to end on its own, past its deadline.
  fn leave_stuck(&mut self) {
    if let Some(worker) = self.worker.take() {
      self.stuck.push(worker.ended);
    }
  }

  fn complete(&mut self, device: String) {
    let Some(job) = &mut self.job else { return };
    let texts = job.texts.iter().map(|t| t.trim()).filter(|t| !t.is_empty());
    let transcription = Transcription {
      text: texts.collect::<Vec<_>>().join(" "),
      device,
      elapsed: job.submitted.elapsed(),
      pieces: std::mem::take(&mut job.pieces),
    };

    self.finish(Ok(transcription));
  }

  fn finish(&mut self, result: Result<Transcription, EngineError>) {
    let job = self.job.take();
    // Whoever gets the result sees the status after it.
    self.share();
    if let Some(job) = job {
      let _ = job.reply.send(result);
    }
  }

  /// Publishes the status for [`Engine::status`].
  fn share(&self) {
    let worker = self.worker.as_ref();
    let status = Status {
      phase: match worker.map(|w| w.state) {
        None => Phase::Stopped,
        Some(WorkerState::Loading { .. }) => Phase::Loading,
        Some(WorkerState::Idle) => Phase::Idle,
        Some(WorkerState::Running { .. }) => Phase::Running,
      },
      thread: worker.map(|w| w.generation),
      last_load: self.last_load,
    };

    *self.shared.lock().unwrap_or_else(PoisonError::into_inner) = status;
  }
}

/// The engine thread: loads the model, runs the pieces until its input ends, drops the model. A
/// panic in the load or a run ends it (the model dropped first) after saying so.
fn serve(
  generation: u64,
  loader: &dyn Loader,
  path: &Path,
  pieces: &Receiver<Piece>,
  msgs: &Sender<WorkerMsg>,
) {
  let send = |event| {
    let _ = msgs.send(WorkerMsg { generation, event });
  };

  let started = Instant::now();
  let load = || {
    let model = loader.load(path)?;
    let loaded = Loaded { device: model.device(), max_audio_ms: model.max_audio_ms() };
    anyhow::Ok((model, loaded))
  };
  let model = match panic::catch_unwind(AssertUnwindSafe(load)) {
    Ok(Ok((model, loaded))) => {
      send(Event::Loaded { model: loaded, load_ms: started.elapsed().as_millis() as u64 });
      model
    }
    Ok(Err(e)) => return send(Event::LoadFailed(format!("{e:#}"))),
    Err(panic) => return send(Event::LoadFailed(format!("panicked: {}", panic_message(&*panic)))),
  };

  for Piece { id, language, pcm } in pieces {
    let run = panic::catch_unwind(AssertUnwindSafe(|| model.transcribe(&pcm, language.as_deref())));
    match run {
      Ok(result) => send(Event::Result { id, result: result.map_err(|e| format!("{e:#}")) }),
      Err(panic) => {
        let message = panic_message(&*panic);
        // Whatever state the run left the model in, it goes before the next one loads.
        drop_model(model);
        return send(Event::Panicked(message));
      }
    }
  }

  drop_model(model);
}

/// Drops a model, catching a panic of its destructor (one left half-way by a panicking run).
fn drop_model(model: Box<dyn SpeechModel>) {
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

  use super::*;
  use crate::testing::{FakeModels, Fault, Faults, TempDir};

  /// One second of silence: the fake transcribes anything.
  fn pcm() -> Vec<f32> {
    vec![0.0; SAMPLE_RATE as usize]
  }

  /// An engine on [`FakeModels`] answering `text` through [`Faults`], with a model file of its
  /// own.
  struct Fake {
    dir: TempDir,
    faults: Faults,
    models: FakeModels,
    engine: Engine,
  }

  impl Fake {
    fn start(name: &str, models: FakeModels, edit: impl FnOnce(&mut EngineConfig)) -> Self {
      let dir = TempDir::new(name);
      let model = dir.path().join("model.gguf");
      fs::write(&model, b"GGUF").unwrap();
      let faults = Faults::new();
      let mut config =
        EngineConfig { loader: faults.wrap(models.clone()), ..EngineConfig::new(model) };
      edit(&mut config);
      Self { dir, faults, models, engine: Engine::start(config) }
    }

    fn model(&self) -> PathBuf {
      self.dir.path().join("model.gguf")
    }

    fn run(&self) -> Result<Transcription, EngineError> {
      self.engine.transcribe(pcm(), None).wait()
    }

    fn wait(&self, what: &str, done: impl Fn(&Status) -> bool) -> Status {
      let deadline = Instant::now() + Duration::from_secs(10);
      loop {
        let status = self.engine.status();
        if done(&status) {
          return status;
        }

        assert!(Instant::now() < deadline, "no {what}: {status:?}");
        thread::sleep(Duration::from_millis(2));
      }
    }

    /// Waits until a load or a run is held in [`Fault::Hang`].
    fn hanging(&self, n: usize) {
      self.wait("a hanging run", |_| self.faults.hanging() == n);
    }

    /// Waits until `n` fake models are in memory.
    fn live(&self, n: usize) {
      self.wait("the models dropped", |_| self.models.live() == n);
    }
  }

  impl Drop for Fake {
    fn drop(&mut self) {
      self.faults.release();
    }
  }

  /// Deadlines of `d` for the load and every run.
  fn fast(d: Duration) -> impl FnOnce(&mut EngineConfig) {
    move |c| c.deadlines = Deadlines { load: d, request_min: d, request_factor: 0.0 }
  }

  /// The model makes nothing of the speech: an empty text, no error; the model stays loaded.
  #[test]
  fn blanks_are_an_empty_text() {
    let f = Fake::start("engine-empty", FakeModels::new("  "), |_| {});
    let out = f.run().unwrap();
    assert_eq!((out.text.as_str(), out.device.as_str(), out.pieces.len()), ("", "fake", 1));
    let status = f.engine.status();
    assert_eq!(status.phase, Phase::Idle);
    assert!(status.thread.is_some() && status.last_load.is_some());
    assert_eq!(f.models.live(), 1, "the model stays loaded");
    assert_eq!(f.run().unwrap().text, "");
    assert_eq!(f.engine.status().thread, status.thread, "on the same thread");
  }

  /// Audio longer than the model's window runs in pieces, their texts joined.
  #[test]
  fn long_input_runs_in_pieces() {
    let models = FakeModels::new("text").max_audio_ms(10_000);
    let f = Fake::start("engine-pieces", models, |_| {});
    let second = SAMPLE_RATE as usize;
    let out = f.engine.transcribe(vec![0.0; 25 * second], None).wait().unwrap();
    assert_eq!(out.text, "text text text");
    assert_eq!(out.pieces.len(), 3);
    assert_eq!((out.pieces[0].start, out.pieces[2].end), (0, 25 * second));
    assert!(out.pieces.iter().all(|p| p.len() <= 10 * second), "{:?}", out.pieces);
    assert_eq!(f.models.runs(), out.pieces.iter().map(Range::len).collect::<Vec<_>>());
  }

  /// A load that fails (not a model, the file gone) fails the job; the next job loads again.
  #[test]
  fn a_failed_load_fails_the_job() {
    let f = Fake::start("engine-load-error", FakeModels::new("text"), |_| {});
    fs::write(f.model(), b"BROKEN").unwrap();
    assert_eq!(f.run().unwrap_err(), EngineError::Load("not a speech model".into()));
    assert_eq!(f.engine.status().phase, Phase::Stopped);

    fs::remove_file(f.model()).unwrap();
    let Err(EngineError::Load(gone)) = f.run() else { panic!("loaded a file that is gone") };
    assert!(gone.contains("No such file"), "{gone}");

    fs::write(f.model(), b"GGUF").unwrap();
    assert_eq!(f.run().unwrap().text, "text");
    assert_eq!(f.models.live(), 1);
  }

  #[test]
  fn a_panic_in_the_load_fails_it_as_a_load_and_the_next_job_loads_again() {
    let f = Fake::start("engine-load-panic", FakeModels::new("text"), |_| {});
    f.faults.load(Fault::Panic);
    let Err(EngineError::Load(error)) = f.run() else { panic!("no load error") };
    assert!(error.starts_with("panicked: injected panic"), "{error}");
    assert_eq!(f.engine.status().phase, Phase::Stopped);
    assert_eq!(f.run().unwrap().text, "text");
    assert_eq!(f.models.loads(), 1, "loaded on a new thread");
  }

  /// A run that panics fails its job, its thread drops the model and ends; the next job loads a
  /// fresh one on a new thread.
  #[test]
  fn a_panic_in_a_run_fails_its_job_and_the_next_job_has_a_fresh_model() {
    let f = Fake::start("engine-run-panic", FakeModels::new("text"), |_| {});
    assert_eq!(f.run().unwrap().text, "text");
    let first = f.engine.status().thread;
    f.faults.run(Fault::Panic);
    let err = f.run().unwrap_err();
    assert_eq!(
      err.to_string(),
      "the engine crashed during transcription (panicked: injected panic in a run)"
    );
    assert_eq!(f.engine.status().phase, Phase::Stopped);
    assert_eq!(f.models.live(), 0, "the broken model is dropped");

    assert_eq!(f.run().unwrap().text, "text");
    assert_ne!(f.engine.status().thread, first, "a new thread");
    assert_eq!((f.models.loads(), f.models.live()), (2, 1));
  }

  /// A run past its deadline fails its job; the thread is left holding its model until the run
  /// returns, and a fresh thread takes the next job.
  #[test]
  fn a_hung_run_times_out_and_the_next_job_gets_a_fresh_thread() {
    let deadline = Duration::from_millis(200);
    let f = Fake::start("engine-run-hang", FakeModels::new("text"), fast(deadline));
    f.faults.run(Fault::Hang);
    let started = Instant::now();
    let a = f.engine.transcribe(pcm(), None);
    f.hanging(1);
    let stuck = f.engine.status().thread;
    assert_eq!(a.wait().unwrap_err(), EngineError::TimedOut(deadline));
    assert!(started.elapsed() >= deadline, "{:?}", started.elapsed());
    assert_eq!(f.engine.status().phase, Phase::Stopped);
    assert_eq!(f.models.live(), 1, "the stuck thread holds its model");

    assert_eq!(f.run().unwrap().text, "text");
    let fresh = f.engine.status().thread;
    assert_ne!(fresh, stuck);
    assert_eq!(f.models.live(), 2, "the stuck one and the fresh one");
    f.faults.release();
    f.live(1); // the stuck run returned for nothing, its model dropped
    assert_eq!(f.engine.status().thread, fresh, "its answer is dropped");
    assert_eq!(f.run().unwrap().text, "text");
  }

  #[test]
  fn a_hung_load_times_out_and_the_next_job_loads_again() {
    let deadline = Duration::from_millis(200);
    let f = Fake::start("engine-load-hang", FakeModels::new("text"), fast(deadline));
    f.faults.load(Fault::Hang);
    assert_eq!(f.run().unwrap_err(), EngineError::TimedOut(deadline));
    assert_eq!(f.run().unwrap().text, "text");
    let fresh = f.engine.status().thread;
    f.faults.release();

    // The stuck load loads, and its thread ends at once: its input is gone.
    f.wait("the stuck load's end", |_| f.models.loads() == 2 && f.models.live() == 1);
    assert_eq!(f.engine.status().thread, fresh);
  }

  /// Past [`MAX_STUCK`] stuck threads no new model is loaded: jobs fail at once until one ends.
  #[test]
  fn stuck_threads_beyond_the_limit_fail_jobs_at_once() {
    let models = FakeModels::new("text");
    let f = Fake::start("engine-stuck", models, fast(Duration::from_millis(100)));
    for _ in 0..MAX_STUCK {
      f.faults.run(Fault::Hang);
      assert!(matches!(f.run(), Err(EngineError::TimedOut(_))));
    }

    let started = Instant::now();
    assert_eq!(f.run().unwrap_err(), EngineError::Stuck(MAX_STUCK));
    assert!(started.elapsed() < Duration::from_millis(100), "{:?}", started.elapsed());
    assert_eq!(f.models.live(), MAX_STUCK, "no model loaded for it");
    f.faults.release();
    f.live(0);
    assert_eq!(f.run().unwrap().text, "text");
  }

  #[test]
  fn cancel_during_a_run_drops_its_result_and_keeps_the_thread() {
    let f = Fake::start("engine-cancel", FakeModels::new("text"), |_| {});
    f.faults.run(Fault::Hang);
    let a = f.engine.transcribe(pcm(), None);
    f.hanging(1);
    let thread = f.engine.status().thread;
    f.engine.cancel(a.id());
    assert_eq!(a.wait().unwrap_err(), EngineError::Cancelled, "answered at once");
    assert_eq!(f.engine.status().phase, Phase::Running, "the run goes on");

    f.faults.release();
    f.wait("the run's end", |s| s.phase == Phase::Idle);
    assert_eq!(f.run().unwrap().text, "text");
    assert_eq!((f.engine.status().thread, f.models.live()), (thread, 1), "the warm thread");
  }

  /// A job that needs the thread while a cancelled run holds it waits for that run: no second
  /// model.
  #[test]
  fn a_job_behind_a_cancelled_run_waits_for_it() {
    let f = Fake::start("engine-cancel-wait", FakeModels::new("text"), |_| {});
    f.faults.run(Fault::Hang);
    let a = f.engine.transcribe(pcm(), None);
    f.hanging(1);
    let thread = f.engine.status().thread;
    f.engine.cancel(a.id());
    let b = f.engine.transcribe(pcm(), None);
    thread::sleep(Duration::from_millis(50));
    let status = f.engine.status();
    assert_eq!((status.phase, status.thread, f.models.live()), (Phase::Running, thread, 1));

    f.faults.release();
    assert_eq!(b.wait().unwrap().text, "text");
    assert_eq!(a.wait().unwrap_err(), EngineError::Cancelled);
    assert_eq!((f.engine.status().thread, f.models.loads()), (thread, 1));
  }

  #[test]
  fn a_cancelled_run_past_its_deadline_fails_nothing() {
    let models = FakeModels::new("text");
    let f = Fake::start("engine-cancel-hang", models, fast(Duration::from_millis(100)));
    f.faults.run(Fault::Hang);
    let a = f.engine.transcribe(pcm(), None);
    f.hanging(1);
    let stuck = f.engine.status().thread;
    f.engine.cancel(a.id());
    let b = f.engine.transcribe(pcm(), None);
    assert_eq!(b.wait().unwrap().text, "text", "on a fresh thread once the run is past it");
    assert_eq!(a.wait().unwrap_err(), EngineError::Cancelled);
    assert_ne!(f.engine.status().thread, stuck);
  }

  /// Jobs run in order; cancelling a queued one drops it alone.
  #[test]
  fn a_cancelled_queued_job_spares_the_others() {
    let f = Fake::start("engine-queue", FakeModels::new("text"), |_| {});
    f.faults.run(Fault::Hang); // job `a` stays current
    let a = f.engine.transcribe(vec![0.0; 1000], None);
    f.hanging(1);
    let b = f.engine.transcribe(vec![0.0; 2000], None);
    let c = f.engine.transcribe(vec![0.0; 3000], None);
    f.engine.cancel(b.id());
    assert_eq!(b.wait().unwrap_err(), EngineError::Cancelled);

    f.faults.release();
    for job in [a, c] {
      job.wait().unwrap();
    }

    assert_eq!(f.models.runs(), [1000, 3000]);
  }

  #[test]
  fn dropping_the_engine_drops_the_model() {
    let f = Fake::start("engine-drop", FakeModels::new("text"), |_| {});
    f.run().unwrap();
    let models = f.models.clone();
    drop(f);
    assert_eq!(models.live(), 0);
  }
}
