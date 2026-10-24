// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Supervisor of the engine thread, on a thread of its own ("engine-supervisor"), so it answers
//! requests, cancels and deadlines while a run takes seconds.
//!
//! - The engine thread ("engine-N") owns the model: it loads it on demand (or on
//!   [`Engine::prewarm`]), runs the pieces it gets over a channel, sends its events back over
//!   another and drops the model when its channel closes. The model is never touched elsewhere.
//! - Panics: the load and every run are inside `catch_unwind`. A load that panics is a failed load
//!   ([`EngineError::Load`]); a run that panics fails its job ([`EngineError::Crashed`]), as does
//!   one that fails so the model may not work again (a lost device: [`wavo::Error::is_fatal`]) with
//!   [`EngineError::Failed`]. Either way the thread drops the model and ends, and the next job
//!   loads a fresh one on a new thread. A native crash (a segfault or an abort in a driver) is not
//!   a panic: it ends the app.
//! - Deadlines ([`Deadlines`]): the model load, each piece of a run max(a minimum, a factor × the
//!   run's audio up to the model's window), from the end of the one before. A thread cannot be
//!   stopped from outside: one past its deadline is left to end on its own, its job fails with
//!   [`EngineError::TimedOut`] and the next job starts a fresh thread. **The stuck thread keeps its
//!   model (memory, GPU buffers) until its run returns**, then drops it and ends; what it answers
//!   is dropped. While [`MAX_STUCK`] of them are still stuck, jobs fail with [`EngineError::Stuck`]
//!   instead of loading yet another model.
//! - An idle engine thread is stopped after [`EngineConfig::unload_after`]: it drops the model and
//!   ends. A prewarmed one waits for the job it was started for up to [`PREWARM_HOLD`], unless
//!   [`Engine::release`] says none comes.
//!
//! A job is one run of [`SpeechModel::transcribe`]: wavo cuts it at pauses into pieces of up to the
//! model's window and tells the thread of each one done ([`wavo::Model::transcribe_long`]), which
//! passes it on; [`Status::progress`] counts them. Dictations go ahead of background jobs: a
//! running one stops after its piece in progress and starts over after them. [`Engine::cancel`]
//! ends a job at once; its run stops after the piece in progress (wavo cannot stop one midway) and
//! what it answers is dropped. The next job waits for that run, which ends within its deadline,
//! rather than loading a second model next to it.

use std::any::Any;
use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{
  Receiver, RecvTimeoutError, Sender, TryRecvError, bounded, never, select, unbounded,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::model::{Loader, Piece, SpeechModel, Wavo};
use crate::audio::SAMPLE_RATE;

/// Engine threads past their deadline that may still hold their models before jobs fail with
/// [`EngineError::Stuck`]: each holds a model, and a wedged GPU would wedge every new one too.
pub const MAX_STUCK: usize = 2;

/// The number of the last engine thread started, in any engine: `engine-N` is one thread for the
/// life of the process.
static THREADS: AtomicU64 = AtomicU64::new(0);

/// How long the supervisor waits for an idle engine thread to drop its model and end.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// A prewarmed engine is kept until its job arrives, up to this long, so the recording it was
/// started for can finish even when `unload_after` is shorter.
const PREWARM_HOLD: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct EngineConfig {
  /// What loads and runs the model: wavo, except in tests.
  pub loader: Arc<dyn Loader>,
  pub model: PathBuf,
  pub deadlines: Deadlines,
  /// Stop an idle engine thread after this long; `None` keeps it until the [`Engine`] is dropped.
  pub unload_after: Option<Duration>,
  /// Told when [`Engine::status`] changes, so the owner shows it without polling (a prewarm that
  /// loads, or fails to, while nothing else happens; a long job's progress).
  pub notify: Option<Sender<()>>,
}

impl EngineConfig {
  /// wavo, default deadlines, no unload.
  pub fn new(model: PathBuf) -> Self {
    Self {
      loader: Arc::new(Wavo),
      model,
      deadlines: Deadlines::default(),
      unload_after: None,
      notify: None,
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
  pub load: Duration,
  pub request_min: Duration,
  pub request_factor: f64,
}

impl Default for Deadlines {
  /// wavo loads a model in ~0.1–0.4 s warm; a cold page cache for a 1.7 GB file takes seconds.
  fn default() -> Self {
    Self {
      load: Duration::from_secs(60),
      request_min: Duration::from_secs(60),
      request_factor: 3.0,
    }
  }
}

impl Deadlines {
  /// Deadline for each piece of a run of `samples` of 16 kHz audio: for the run, up to the model's
  /// window.
  fn piece(&self, samples: usize, window_ms: Option<u32>) -> Duration {
    let window = window_ms.map_or(samples, |w| w as usize * (SAMPLE_RATE as usize / 1000));
    let audio = Duration::from_secs_f64(samples.min(window) as f64 / SAMPLE_RATE as f64);
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
  /// How many pieces the input was cut into.
  pub pieces: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
  /// No engine thread: no model in memory (but in a stuck thread's).
  #[default]
  Stopped,
  Loading,
  Idle,
  Running,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
  pub phase: Phase,
  /// The number of the engine thread (`engine-N`, unique in the process): a new one after a
  /// panic, a timeout or an unload.
  pub thread: Option<u64>,
  /// Thread start → model loaded, of the most recent engine thread.
  pub last_load: Option<Duration>,
  /// The device the model last loaded runs on (kept after its thread is gone).
  pub device: Option<String>,
  /// Finished and planned pieces of the current job, once its first piece is done.
  pub progress: Option<(usize, usize)>,
  /// Why the last load failed (the file gone, not a model…), until a load succeeds.
  pub load_error: Option<String>,
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
  id: JobId,
  reply: Receiver<Result<Transcription, EngineError>>,
}

impl Pending {
  pub fn id(&self) -> JobId {
    self.id
  }

  pub fn wait(self) -> Result<Transcription, EngineError> {
    self.reply.recv().unwrap_or(Err(EngineError::Stopped))
  }
}

enum Request {
  Prewarm,
  Release,
  Transcribe(Job),
  Cancel(JobId),
  Quit,
}

impl Engine {
  /// Starts the supervisor thread; the engine thread starts with the first job or prewarm.
  pub fn start(config: EngineConfig) -> Result<Self, EngineError> {
    let (requests, requests_rx) = unbounded();
    let (msgs_tx, msgs) = unbounded();
    let status = Arc::new(Mutex::new(Status::default()));
    let supervisor = Supervisor::new(config, msgs_tx, status.clone());
    let thread = thread::Builder::new()
      .name("engine-supervisor".into())
      .spawn(move || supervisor.run(requests_rx, msgs))
      .map_err(|e| EngineError::Spawn(e.to_string()))?;

    Ok(Self { requests, next_job: AtomicU64::new(1), status, thread: Some(thread) })
  }

  /// Starts the engine thread and loads the model ahead of the first job (as a recording starts).
  pub fn prewarm(&self) {
    let _ = self.requests.send(Request::Prewarm);
  }

  /// The recording a [`prewarm`](Self::prewarm) was for ended without a job (cancelled, no
  /// speech): the idle unload counts as usual again.
  pub fn release(&self) {
    let _ = self.requests.send(Request::Release);
  }

  /// Queues a transcription of 16 kHz mono audio, the engine's input (see
  /// [`Clip::engine_input`](crate::audio::vad::Clip::engine_input)); `language` `None` leaves it
  /// to the model. It goes ahead of queued [`transcribe_background`](Self::transcribe_background)
  /// jobs.
  pub fn transcribe(&self, pcm: Vec<f32>, language: Option<String>) -> Pending {
    self.submit(pcm, language, false)
  }

  /// Like [`transcribe`](Self::transcribe), after every other job: transcribing a history entry
  /// again never holds up a dictation, nor ends the prewarm hold of the recording in progress.
  pub fn transcribe_background(&self, pcm: Vec<f32>, language: Option<String>) -> Pending {
    self.submit(pcm, language, true)
  }

  fn submit(&self, pcm: Vec<f32>, language: Option<String>, background: bool) -> Pending {
    let (reply, rx) = bounded(1);
    let id = self.next_job.fetch_add(1, Ordering::Relaxed);
    let submitted = Instant::now();
    let job =
      Job { id, background, pcm: Arc::new(pcm), language, reply, submitted, progress: None };

    // If the supervisor is gone the job, and with it the reply sender, is dropped: `Stopped`.
    let _ = self.requests.send(Request::Transcribe(job));
    Pending { id, reply: rx }
  }

  /// Ends job `id` with [`EngineError::Cancelled`] at once. Its run stops after the piece in
  /// progress, and what it answers is dropped. Finished jobs are not affected.
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
  /// Behind dictations: [`Engine::transcribe_background`].
  background: bool,
  pcm: Arc<Vec<f32>>,
  language: Option<String>,
  reply: Sender<Result<Transcription, EngineError>>,
  submitted: Instant,
  /// Finished and planned pieces of its run, once the first is done.
  progress: Option<(usize, usize)>,
}

/// The live engine thread, as the supervisor sees it.
struct Worker {
  /// Its number ([`Status::thread`]): tells its messages from those of threads left behind.
  thread: u64,
  runs: Sender<Run>,
  /// Set to stop the run in progress after its piece in progress ([`Stop`]).
  stop: Arc<AtomicBool>,
  /// Disconnected once the thread has ended, its model dropped.
  ended: Receiver<()>,
  spawned: Instant,
  /// The loaded model's, once loaded.
  model: Option<ModelInfo>,
  state: WorkerState,
}

impl Worker {
  /// Has the run in progress stop after its piece in progress, for `why`; whether one runs.
  fn stop_run(&mut self, why: Stop) -> bool {
    let WorkerState::Running { stop, .. } = &mut self.state else { return false };
    *stop = why;
    self.stop.store(true, Ordering::Relaxed);
    true
  }
}

/// What the supervisor knows of a loaded model.
#[derive(Debug, Clone)]
struct ModelInfo {
  device: String,
  max_audio_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum WorkerState {
  Loading {
    deadline: Instant,
  },
  Idle,
  /// `deadline` of the piece in progress, `timeout` of each piece.
  Running {
    deadline: Instant,
    timeout: Duration,
    stop: Stop,
  },
}

/// Why a run was told to stop after its piece in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
  No,
  /// Its job was cancelled: what it answers is dropped.
  Cancel,
  /// A dictation goes first: the background job starts over after it.
  Yield,
}

/// Supervisor → engine thread: a job's audio to transcribe; one at a time, the next one once its
/// result is in.
struct Run {
  pcm: Arc<Vec<f32>>,
  language: Option<String>,
}

/// Engine thread → supervisor.
struct WorkerMsg {
  thread: u64,
  event: Event,
}

enum Event {
  Loaded {
    model: ModelInfo,
    load_ms: u64,
  },
  /// The load failed or panicked; the thread has ended.
  LoadFailed(String),
  /// A piece of the run in progress is done.
  Piece(Piece),
  /// Of the run it was sent last; one told to stop fails.
  Result(Result<String, String>),
  /// The run panicked or failed for good; the thread dropped its model and ended.
  Ended(EngineError),
}

struct Supervisor {
  config: EngineConfig,
  /// Handed to each engine thread.
  msgs_tx: Sender<WorkerMsg>,
  worker: Option<Worker>,
  job: Option<Job>,
  queue: VecDeque<Job>,
  last_load: Option<Duration>,
  /// Since when the engine thread has had nothing to do: the idle unload counts from it.
  idle_since: Instant,
  /// A prewarm keeps the thread until then, unless its job comes first.
  hold_until: Option<Instant>,
  /// The device of the model last loaded.
  device: Option<String>,
  load_error: Option<String>,
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
      job: None,
      queue: VecDeque::new(),
      last_load: None,
      idle_since: Instant::now(),
      hold_until: None,
      device: None,
      load_error: None,
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
          Ok(Request::Quit) | Err(_) => break,
          Ok(request) => self.on_request(request),
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

  fn on_request(&mut self, request: Request) {
    match request {
      Request::Prewarm => {
        self.hold_until = Some(Instant::now() + PREWARM_HOLD);
        if self.worker.is_none()
          && let Err(e) = self.spawn()
        {
          warn!("prewarm: {e}");
        }
      }
      Request::Release => self.hold_until = None,
      Request::Transcribe(job) if job.background => self.queue.push_back(job),
      Request::Transcribe(job) => {
        // The recording a prewarm was for has arrived; after it the usual unload timer applies.
        self.hold_until = None;
        let at = self.queue.iter().position(|j| j.background).unwrap_or(self.queue.len());
        self.queue.insert(at, job);
      }
      Request::Cancel(id) => self.cancel(id),
      Request::Quit => unreachable!("handled by the loop"),
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

    // Its run stops after the piece in progress, for nothing.
    if let Some(worker) = &mut self.worker
      && worker.stop_run(Stop::Cancel)
    {
      info!("engine thread {}: transcription cancelled; its run ends unread", worker.thread);
    }

    self.finish(Err(EngineError::Cancelled));
  }

  /// A dictation waits for the piece of a background job in progress, not for the rest of it: the
  /// run stops after that piece, and the job starts over after the dictations.
  fn dictation_first(&mut self) {
    let (Some(job), Some(worker)) = (&self.job, &mut self.worker) else { return };
    let running = matches!(worker.state, WorkerState::Running { stop: Stop::No, .. });
    if !running || !job.background || !self.queue.front().is_some_and(|j| !j.background) {
      return;
    }

    debug!(id = job.id, "a dictation goes first: the background job stops after its piece");
    worker.stop_run(Stop::Yield);
  }

  /// Sends the next job to the engine thread, started for it if need be, once it is free.
  fn pump(&mut self) {
    self.dictation_first();
    loop {
      // A job runs, or none waits.
      if self.job.is_some() || self.queue.is_empty() {
        return;
      }

      if self.worker.is_none()
        && let Err(e) = self.spawn()
      {
        self.fail(e);
        continue;
      }

      let Some(worker) = &mut self.worker else { return };
      // Loading, or running a cancelled job (it ends within its deadline).
      let (WorkerState::Idle, Some(model)) = (worker.state, &worker.model) else { return };
      let job = self.job.insert(self.queue.pop_front().unwrap());
      worker.stop.store(false, Ordering::Relaxed);
      if worker.runs.send(Run { pcm: job.pcm.clone(), language: job.language.clone() }).is_err() {
        // The thread ends only after saying why (or when told to): this is a bug.
        warn!("engine thread {} is gone without a word", worker.thread);
        self.worker = None;
        self.finish(Err(EngineError::Crashed("without a word".into())));
        continue;
      }

      let samples = job.pcm.len();
      let timeout = self.config.deadlines.piece(samples, model.max_audio_ms);
      let deadline = Instant::now() + timeout;
      worker.state = WorkerState::Running { deadline, timeout, stop: Stop::No };
      debug!(samples, ?timeout, "run sent");
      return;
    }
  }

  fn on_worker(&mut self, msg: WorkerMsg) {
    let Some(worker) = self.worker.as_mut().filter(|w| w.thread == msg.thread) else {
      return; // from a thread left behind
    };

    let thread = worker.thread;
    match msg.event {
      Event::Loaded { model, load_ms } => {
        let total = worker.spawned.elapsed();
        info!(
          "engine thread {thread} loaded {} on {} in {total:.2?} (model {load_ms} ms)",
          self.config.model.display(),
          model.device,
        );
        self.device = Some(model.device.clone());
        self.load_error = None;
        worker.model = Some(model);
        worker.state = WorkerState::Idle;
        self.idle_since = Instant::now();
        self.last_load = Some(total);
      }

      Event::LoadFailed(error) => {
        warn!("engine could not load {}: {error}", self.config.model.display());
        self.load_error = Some(error.clone());
        self.worker = None; // the thread has ended
        self.fail(EngineError::Load(error));
      }

      Event::Piece(piece) => {
        if let WorkerState::Running { deadline, timeout, .. } = &mut worker.state {
          *deadline = Instant::now() + *timeout; // the next piece's
        }

        // None once cancelled.
        if let Some(job) = &mut self.job {
          job.progress = Some((piece.index + 1, piece.count));
        }
      }

      Event::Result(result) => {
        let stop = match worker.state {
          WorkerState::Running { stop, .. } => stop,
          _ => Stop::No,
        };

        worker.state = WorkerState::Idle;
        self.idle_since = Instant::now();
        if stop == Stop::Cancel {
          debug!("the result of a cancelled job dropped");
          return;
        }

        if stop == Stop::Yield
          && result.is_err()
          && let Some(mut job) = self.job.take()
        {
          // It gave way to a dictation: it starts over after the dictations.
          job.progress = None;
          let at = self.queue.iter().position(|j| j.background).unwrap_or(self.queue.len());
          self.queue.insert(at, job);
          return;
        }

        let device = worker.model.as_ref().map(|m| m.device.clone()).unwrap_or_default();
        let Some(job) = &self.job else { return };
        let result = match result {
          Ok(text) => {
            let (elapsed, pieces) = (job.submitted.elapsed(), job.progress.map_or(1, |p| p.1));
            Ok(Transcription { text, device, elapsed, pieces })
          }
          Err(error) => Err(EngineError::Failed(error)),
        };

        self.finish(result);
      }

      Event::Ended(error) => {
        let state = worker.state;
        self.worker = None; // the thread has dropped its model and ended
        if let WorkerState::Running { stop: Stop::Cancel, .. } = state {
          warn!("engine thread {thread} ended in a cancelled run: {error}");
          return;
        }

        warn!("engine thread {thread} ended: {error}");
        if self.job.is_some() {
          self.finish(Err(error));
        }
      }
    }
  }

  fn on_deadline(&mut self) {
    if self.deadline().is_none_or(|due| Instant::now() < due) {
      return; // a timer for a state already left
    }

    let Some(worker) = &self.worker else { return };
    let thread = worker.thread;
    let (what, timeout) = match worker.state {
      WorkerState::Idle => {
        info!("engine thread {thread} idle, unloading");
        self.stop_worker();
        return;
      }
      WorkerState::Loading { .. } => ("model load", self.config.deadlines.load),
      WorkerState::Running { timeout, stop: Stop::Cancel, .. } => {
        warn!(
          "engine thread {thread}: a cancelled run took longer than {timeout:?}; it is left \
           to end on its own, holding its model until then"
        );
        self.leave_stuck();
        return;
      }
      WorkerState::Running { timeout, .. } => ("transcription", timeout),
    };

    warn!(
      "engine thread {thread}: {what} took longer than {timeout:?}; it is left to end on its \
       own, holding its model until then, and a fresh thread takes the next job"
    );
    self.leave_stuck();
    self.fail(EngineError::TimedOut(timeout));
  }

  /// When the supervisor must wake up without a message: the deadline of a load or a run, or the
  /// idle unload.
  fn deadline(&self) -> Option<Instant> {
    match self.worker.as_ref()?.state {
      WorkerState::Loading { deadline } | WorkerState::Running { deadline, .. } => Some(deadline),
      WorkerState::Idle if !self.queue.is_empty() => None,
      WorkerState::Idle => {
        let due = self.idle_since.checked_add(self.config.unload_after?)?;
        Some(self.hold_until.map_or(due, |hold| due.max(hold)))
      }
    }
  }

  /// Starts an engine thread that loads the model.
  fn spawn(&mut self) -> Result<(), EngineError> {
    self.stuck.retain(|ended| matches!(ended.try_recv(), Err(TryRecvError::Empty)));
    if self.stuck.len() >= MAX_STUCK {
      return Err(EngineError::Stuck(self.stuck.len()));
    }

    let thread = THREADS.fetch_add(1, Ordering::Relaxed) + 1;
    let (runs, runs_rx) = unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let (ended_tx, ended) = bounded::<()>(0);
    let loader = self.config.loader.clone();
    let path = self.config.model.clone();
    let msgs = self.msgs_tx.clone();
    let stop_rx = stop.clone();
    let spawned = Instant::now();
    thread::Builder::new()
      .name(format!("engine-{thread}"))
      .spawn(move || {
        // Dropped as the thread ends, after the model.
        let _ended = ended_tx;
        serve(thread, &*loader, &path, &runs_rx, &stop_rx, &msgs);
      })
      .map_err(|e| EngineError::Spawn(e.to_string()))?;

    debug!(thread, "engine thread started");
    let state = WorkerState::Loading { deadline: spawned + self.config.deadlines.load };
    self.worker = Some(Worker { thread, runs, stop, ended, spawned, model: None, state });
    Ok(())
  }

  /// Ends the engine thread: an idle one drops its model before this returns (up to
  /// [`SHUTDOWN_GRACE`]); a busy one when its load or run returns.
  fn stop_worker(&mut self) {
    let Some(Worker { thread, runs, stop, ended, state, .. }) = self.worker.take() else { return };
    drop(runs); // the end of its input
    stop.store(true, Ordering::Relaxed); // and of its run, after the piece in progress
    if !matches!(state, WorkerState::Idle) {
      debug!(thread, ?state, "engine thread left to end after its load or run");
      return;
    }

    let started = Instant::now();
    match ended.recv_timeout(SHUTDOWN_GRACE) {
      Err(RecvTimeoutError::Disconnected) => {
        debug!(thread, "engine thread ended, its model dropped in {:.2?}", started.elapsed());
      }
      _ => warn!("engine thread {thread} still drops its model after {SHUTDOWN_GRACE:?}"),
    }
  }

  /// Leaves the engine thread to end on its own, past its deadline.
  fn leave_stuck(&mut self) {
    if let Some(worker) = self.worker.take() {
      worker.stop.store(true, Ordering::Relaxed); // its run goes no further than its piece
      self.stuck.push(worker.ended);
    }
  }

  /// Fails the current job, else the next one, whose engine thread did not start or load.
  fn fail(&mut self, error: EngineError) {
    if self.job.is_none() {
      self.job = self.queue.pop_front();
    }

    self.finish(Err(error));
  }

  fn finish(&mut self, result: Result<Transcription, EngineError>) {
    let job = self.job.take();
    self.idle_since = Instant::now();
    // Whoever gets the result sees the status after it.
    self.share();
    if let Some(job) = job {
      let _ = job.reply.send(result);
    }
  }

  /// Publishes the status for [`Engine::status`], and tells the owner when it changed.
  fn share(&self) {
    let worker = self.worker.as_ref();
    let status = Status {
      phase: match worker.map(|w| w.state) {
        None => Phase::Stopped,
        Some(WorkerState::Loading { .. }) => Phase::Loading,
        Some(WorkerState::Idle) => Phase::Idle,
        Some(WorkerState::Running { .. }) => Phase::Running,
      },
      thread: worker.map(|w| w.thread),
      last_load: self.last_load,
      device: self.device.clone(),
      progress: self.job.as_ref().and_then(|j| j.progress),
      load_error: self.load_error.clone(),
    };

    let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
    let changed = *shared != status;
    *shared = status;
    drop(shared);
    if changed && let Some(notify) = &self.config.notify {
      let _ = notify.send(());
    }
  }
}

/// The engine thread: loads the model, runs the jobs until its input ends, drops the model. A panic
/// in the load or a run ends it (the model dropped first) after saying so. `stop` stops a run
/// after its piece in progress.
fn serve(
  thread: u64,
  loader: &dyn Loader,
  path: &Path,
  runs: &Receiver<Run>,
  stop: &AtomicBool,
  msgs: &Sender<WorkerMsg>,
) {
  let send = |event| {
    let _ = msgs.send(WorkerMsg { thread, event });
  };

  let started = Instant::now();
  let load = || {
    let model = loader.load(path)?;
    let loaded = ModelInfo { device: model.device(), max_audio_ms: model.max_audio_ms() };
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

  for Run { pcm, language } in runs {
    let mut on_piece = |piece: Piece| {
      send(Event::Piece(piece));
      // After the last piece the run is done anyway.
      if piece.index + 1 < piece.count && stop.load(Ordering::Relaxed) {
        return ControlFlow::Break(());
      }

      ControlFlow::Continue(())
    };
    let run = || model.transcribe(&pcm, language.as_deref(), &mut on_piece);
    let ended = match panic::catch_unwind(AssertUnwindSafe(run)) {
      Ok(Err(e)) if e.downcast_ref().is_some_and(wavo::Error::is_fatal) => {
        EngineError::Failed(format!("{e:#}"))
      }
      Ok(result) => {
        send(Event::Result(result.map_err(|e| format!("{e:#}"))));
        continue;
      }
      Err(panic) => {
        let message = panic_message(&*panic);
        EngineError::Crashed(format!("during transcription (panicked: {message})"))
      }
    };

    // Whatever state the run left the model in, it goes before the next one loads.
    drop_model(model);
    return send(Event::Ended(ended));
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
  use crate::testing::{FakeModels, Fault, Faults, TempDir, poll};

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
      Self { dir, faults, models, engine: Engine::start(config).unwrap() }
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
    assert_eq!((out.text.as_str(), out.device.as_str(), out.pieces), ("", "fake", 1));
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
    assert_eq!((out.text.as_str(), out.pieces), ("text text text", 3));
    let runs = f.models.runs();
    assert_eq!(runs.iter().sum::<usize>(), 25 * second);
    assert!(runs.iter().all(|&n| n <= 10 * second), "{runs:?}");
  }

  /// Each piece's deadline is for the run, up to the model's window.
  #[test]
  fn each_piece_has_a_deadline_of_its_own() {
    let d =
      Deadlines { load: Duration::ZERO, request_min: Duration::from_secs(1), request_factor: 2.0 };
    let n = 25 * SAMPLE_RATE as usize;
    assert_eq!(d.piece(n, Some(10_000)), Duration::from_secs(20), "the window");
    assert_eq!(d.piece(n, None), Duration::from_secs(50), "no window: all of it");
    assert_eq!(d.piece(n / 100, None), Duration::from_secs(1), "the minimum");
  }

  /// A load that fails (not a model, the file gone) fails the job and stays in the status until a
  /// load succeeds; the owner is told. The next job loads again.
  #[test]
  fn a_failed_load_fails_the_job_and_stays_in_the_status() {
    let (tx, told) = unbounded();
    let f = Fake::start("engine-load-error", FakeModels::new("text"), |c| c.notify = Some(tx));
    fs::write(f.model(), b"BROKEN").unwrap();
    let error = "not a speech model".to_owned();
    assert_eq!(f.run().unwrap_err(), EngineError::Load(error.clone()));
    let status = f.engine.status();
    assert_eq!((status.phase, status.load_error.as_deref()), (Phase::Stopped, Some(&*error)));
    assert!(told.try_iter().count() >= 1, "the owner is told");

    fs::remove_file(f.model()).unwrap();
    let Err(EngineError::Load(gone)) = f.run() else { panic!("loaded a file that is gone") };
    assert!(gone.contains("No such file"), "{gone}");

    fs::write(f.model(), b"GGUF").unwrap();
    assert_eq!(f.run().unwrap().text, "text");
    assert_eq!(f.models.live(), 1);
    let status = f.engine.status();
    assert_eq!((status.load_error, status.device.as_deref()), (None, Some("fake")), "loaded");
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

  /// A run that fails on the GPU fails its job and ends its thread, as a panic does; another error
  /// keeps the model.
  #[test]
  fn a_gpu_failure_retires_the_model() {
    let f = Fake::start("engine-gpu", FakeModels::new("text"), |_| {});
    f.faults.run(Fault::Fail("no such language"));
    let err = f.run().unwrap_err();
    assert_eq!(err, EngineError::Failed("no such language".into()));
    let first = f.engine.status().thread;
    assert_eq!((f.engine.status().phase, f.models.live()), (Phase::Idle, 1), "kept");

    f.faults.run(Fault::GpuLost);
    let Err(EngineError::Failed(err)) = f.run() else { panic!("no failure") };
    assert!(err.starts_with("GPU: "), "{err}");
    assert_eq!((f.engine.status().phase, f.models.live()), (Phase::Stopped, 0), "dropped");

    assert_eq!(f.run().unwrap().text, "text");
    assert_ne!(f.engine.status().thread, first, "a new thread");
    assert_eq!(f.models.loads(), 2);
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

  /// A run left behind, past its deadline or by a dropped engine, stops after its piece in
  /// progress rather than running the rest of its input.
  #[test]
  fn a_run_left_behind_stops_after_its_piece() {
    let long = || vec![0.0; 25 * SAMPLE_RATE as usize];
    let models = FakeModels::new("text").max_audio_ms(10_000);
    let f = Fake::start("engine-left", models.clone(), fast(Duration::from_millis(200)));
    f.faults.run(Fault::Hang);
    assert!(matches!(f.engine.transcribe(long(), None).wait(), Err(EngineError::TimedOut(_))));
    f.faults.release();
    f.live(0);
    assert_eq!(models.runs().len(), 1, "past its deadline: the rest never ran");

    let (dir, faults) = (TempDir::new("engine-left-drop"), Faults::new());
    let model = dir.path().join("model.gguf");
    fs::write(&model, b"GGUF").unwrap();
    let models = FakeModels::new("text").max_audio_ms(10_000);
    let engine = Engine::start(EngineConfig {
      loader: faults.wrap(models.clone()),
      ..EngineConfig::new(model)
    })
    .unwrap();
    faults.run(Fault::Hang);
    let _job = engine.transcribe(long(), None);
    poll("a hanging run", Duration::from_secs(10), || (faults.hanging() == 1).then_some(()));
    drop(engine);
    faults.release();
    poll("the run's end", Duration::from_secs(10), || (models.live() == 0).then_some(()));
    assert_eq!(models.runs().len(), 1, "a dropped engine: the rest never ran");
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

  /// Dictations go ahead of queued background jobs; cancelling a queued job drops it alone.
  #[test]
  fn dictations_go_ahead_of_background_jobs_and_cancel_spares_the_others() {
    let f = Fake::start("engine-queue", FakeModels::new("text"), |_| {});
    f.engine.prewarm();
    let thread = f.wait("the load", |s| s.phase == Phase::Idle).thread;
    f.faults.run(Fault::Hang); // job `a` stays current
    // Told apart by their lengths.
    let a = f.engine.transcribe_background(vec![0.0; 1000], None);
    f.hanging(1);
    let b = f.engine.transcribe_background(vec![0.0; 2000], None);
    let c = f.engine.transcribe(vec![0.0; 3000], None);
    let d = f.engine.transcribe(vec![0.0; 4000], None);
    f.engine.cancel(d.id());
    assert_eq!(d.wait().unwrap_err(), EngineError::Cancelled);

    f.faults.release();
    for job in [a, b, c] {
      job.wait().unwrap();
    }

    assert_eq!(f.models.runs(), [1000, 3000, 2000], "the dictation went ahead");
    assert_eq!(f.engine.status().thread, thread, "one thread for all");
  }

  /// The idle unload ends the thread, which drops the model; a prewarmed one waits for its
  /// recording even with an unload at once.
  #[test]
  fn an_idle_engine_unloads_its_model_after_a_prewarm_waited_for_its_job() {
    let unload = |c: &mut EngineConfig| c.unload_after = Some(Duration::ZERO);
    let f = Fake::start("engine-unload", FakeModels::new("text"), unload);
    f.engine.prewarm();
    f.wait("the load", |s| s.phase == Phase::Idle);
    thread::sleep(Duration::from_millis(100)); // "recording"
    assert_eq!(f.engine.status().phase, Phase::Idle, "the prewarmed model is kept");
    assert_eq!(f.run().unwrap().text, "text");
    f.wait("the unload", |s| s.phase == Phase::Stopped);
    assert_eq!(f.models.live(), 0, "dropped by the time the status says so");
    assert_eq!(f.run().unwrap().text, "text");
    assert_eq!(f.models.loads(), 2, "loaded again");
  }

  /// A prewarm released without a job (its recording was cancelled) holds the model no longer
  /// than the idle unload.
  #[test]
  fn a_released_prewarm_unloads_as_any_idle_engine() {
    let unload = |c: &mut EngineConfig| c.unload_after = Some(Duration::ZERO);
    let f = Fake::start("engine-release", FakeModels::new("text"), unload);
    f.engine.prewarm();
    f.wait("the load", |s| s.phase == Phase::Idle);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(f.engine.status().phase, Phase::Idle, "held for its recording");
    f.engine.release();
    f.wait("the unload", |s| s.phase == Phase::Stopped);
    assert_eq!(f.models.live(), 0);
  }

  /// A job's progress shows as its pieces are done; a cancel stops its run after the piece in
  /// progress.
  #[test]
  fn progress_counts_the_finished_pieces_and_a_cancel_stops_the_rest() {
    let models = FakeModels::new("text").max_audio_ms(10_000);
    let f = Fake::start("engine-progress", models, |_| {});
    f.faults.run(Fault::Pass);
    f.faults.run(Fault::Hang); // the second piece
    let job = f.engine.transcribe(vec![0.0; 25 * SAMPLE_RATE as usize], None);
    f.hanging(1);
    f.wait("the first piece", |s| s.progress == Some((1, 3)));
    f.engine.cancel(job.id());
    assert_eq!(job.wait().unwrap_err(), EngineError::Cancelled);
    assert_eq!(f.engine.status().progress, None);

    f.faults.release();
    f.wait("the run's end", |s| s.phase == Phase::Idle);
    assert_eq!(f.models.runs().len(), 2, "the third piece never ran");
    assert_eq!(f.run().unwrap().text, "text");
  }

  /// A dictation waits for the piece of a background job in progress, then goes ahead of the rest:
  /// the background job starts over after it.
  #[test]
  fn a_dictation_goes_ahead_of_the_rest_of_a_background_job() {
    let models = FakeModels::new("text").max_audio_ms(10_000);
    let f = Fake::start("engine-background", models, |_| {});
    f.faults.run(Fault::Hang);
    let background = f.engine.transcribe_background(vec![0.0; 25 * SAMPLE_RATE as usize], None);
    f.hanging(1);
    let dictation = f.engine.transcribe(vec![0.0; 1000], None);
    // Its reply says the supervisor has the dictation queued.
    let marker = f.engine.transcribe(vec![0.0; 2000], None);
    f.engine.cancel(marker.id());
    assert_eq!(marker.wait().unwrap_err(), EngineError::Cancelled);
    f.faults.release();
    dictation.wait().unwrap();
    let out = background.wait().unwrap();
    assert_eq!((out.text.as_str(), out.pieces), ("text text text", 3));
    let runs = f.models.runs();
    assert_eq!(runs[..2], [runs[2], 1000], "after the piece in progress");
    assert_eq!(runs.len(), 5, "then the background job again, whole");
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
