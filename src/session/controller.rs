// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The session controller: one "session" thread owns the [`Machine`], the recorder, the engine,
//! the history and the text output, and carries out the machine's effects. What takes long runs
//! elsewhere (the engine on its threads, a waiter thread per transcription), so the controller
//! answers `wavo-app ctl` within milliseconds in every state.
//!
//! - Start → the microphone opens and the observer sees the recording first; then where the person
//!   types is noted and the engine starts loading the model. The session never waits for the model:
//!   the transcription is queued until it is loaded.
//! - Stop → after the post-roll (the end of a word the key-up cut) the microphone closes and the
//!   recording goes to the engine. Without speech, or cancelled, nothing is kept: the recording is
//!   deleted.
//! - Whether a model can transcribe is looked at without loading one ([`NoModel`] in the status):
//!   at the start, when an engine starts, when a setting changes the model and when a load fails. A
//!   loaded model whose file is gone goes with its engine at the next recording. A chosen model
//!   that is not on disk gives way to the first one found while it is not, a warning in the status
//!   saying so; the choice stays (downloading it again brings it back).
//! - The text goes to the history first, then it is pasted (`pasted` says whether it was). The
//!   history keeps transcriptions only: a dictation that fails (no model can transcribe it, the
//!   engine fails, nothing is recognized in its speech) ends with its error in the status, and its
//!   recording is kept as the last failed one, to transcribe again (`ctl retranscribe-last`); the
//!   next failure takes its place.
//! - Every dictation ends with the history retention.
//! - On start, before any recording, the recordings a crash left without an entry are transcribed
//!   in the background, as the last failed one is again: the text becomes an entry, a failure the
//!   last failed recording. The first retention runs on a thread of its own: it may have a lot to
//!   delete.
//! - A platform shell can observe the session: [`Session::start_observed`] calls the observer on
//!   the session thread after every change, with the status, the settings and the level meter.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, after, bounded, never, select, unbounded};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use wavo::hub::{self, Hub};

use super::machine::{Command, Effect, Input, Machine, State, Take};
use super::output::TextOutput;
use crate::audio::capture::{self, Source};
use crate::audio::recorder::{Meter, Recorder};
use crate::audio::vad::Clip;
use crate::audio::{SAMPLE_RATE, wav};
use crate::ctl::{Reply, Request};
use crate::engine::model::{self, Loader, Wavo};
use crate::engine::supervisor::{
  Deadlines, EngineError, JobId, Phase as EnginePhase, Transcription,
};
use crate::engine::{Engine, EngineConfig};
use crate::history::db::NewEntry;
use crate::history::{History, Retention, now_ms, recordings};
use crate::settings::{self, Activation, Settings};

/// How long a request waits for the session thread; less than `wavo-app ctl` waits for the reply,
/// so the client gets this error rather than a timeout of its own.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// After a stop the microphone stays open until the device has delivered what was said up to the
/// stop (the built-in microphone of an M2 MacBook Pro delivers it 42 ms late, a 512-frame buffer
/// at 48 kHz and 30 ms of device latency; a USB one 14 ms), in three ticks, and the VAD has
/// scored it…
const POST_ROLL_MIN: Duration = Duration::from_millis(48);
/// …then until the recording ends in this much audio without speech, so a word the key-up cut is
/// finished (a shorter dip may be inside a word)…
const POST_ROLL_QUIET: Duration = Duration::from_millis(96);
/// …but never longer: the rest of a word (250–430 ms a word in the test fixtures).
const POST_ROLL_MAX: Duration = Duration::from_millis(300);
/// How often the post-roll looks for the end of the speech: a VAD frame.
const POST_ROLL_TICK: Duration = Duration::from_millis(16);

/// What a session runs with; [`Config::from_env`] for the app, explicit values in tests.
#[derive(Clone)]
pub struct Config {
  /// Settings, history and recordings.
  pub home: PathBuf,
  /// Where speech models are found.
  pub hub: Hub,
  /// What loads and runs the models: wavo, except in tests.
  pub loader: Arc<dyn Loader>,
  pub deadlines: Deadlines,
  /// A WAV file recorded instead of the microphone (`WAVO_FAKE_MIC`).
  pub fake_mic: Option<PathBuf>,
}

impl Config {
  pub fn from_env() -> Self {
    Self {
      home: crate::home(),
      hub: Hub::new(),
      loader: Arc::new(Wavo),
      deadlines: Deadlines::default(),
      fake_mic: capture::fake_mic(),
    }
  }
}

/// What `wavo-app ctl status` shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
  pub state: Phase,
  pub activation: Activation,
  /// The current dictation's history entry, once its text is kept.
  pub entry: Option<i64>,
  pub recording: Option<RecordingStatus>,
  pub engine: EngineStatus,
  /// The last dictation that ended.
  pub last: Option<Last>,
  /// Entries being transcribed again.
  pub retranscribing: Vec<i64>,
  /// When the last failed dictation's recording started (Unix ms), kept to transcribe again
  /// (`ctl retranscribe-last`); `None` while that runs.
  pub failed_recording: Option<i64>,
  /// The last problem that is no dictation's: the microphone did not open, a recording that could
  /// not be transcribed again…
  pub error: Option<String>,
  /// Settings problems, and the current or last dictation's (device fallback, lost samples…).
  pub warnings: Vec<String>,
  /// Why no model can transcribe now, if none can: a recording is kept for later.
  pub no_model: Option<NoModel>,
}

/// Why no model can transcribe ([`Status::no_model`]); the shells say it in their own words, the
/// last dictation's error as it displays: `parakeet-tdt-0.6b-v3 is not on disk`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "why")]
pub enum NoModel {
  /// None is downloaded.
  #[error("no model is downloaded")]
  NoneDownloaded,
  /// The chosen model's file is gone from the disk (`model`: what it is called).
  #[error("{model} is not on disk")]
  Gone { model: String },
  /// The model is there but does not load.
  #[error("{model} could not be loaded: {error}")]
  LoadFailed { model: String, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
  Idle,
  Recording,
  Transcribing,
  Pasting,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatus {
  pub device: String,
  pub seconds: f64,
  /// RMS of the last ~33 ms, 0 to 1.
  pub level: f32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EngineStatus {
  /// Model file name without extension, once one is chosen.
  pub model: Option<String>,
  pub phase: EnginePhase,
  /// The engine thread's number ([`Status::thread`](crate::engine::supervisor::Status::thread)).
  pub thread: Option<u64>,
  /// Finished and planned pieces of the current job.
  pub progress: Option<(usize, usize)>,
  /// The device the model runs on, once the engine loaded it: `Apple M2 (Metal, integrated)`.
  pub device: Option<String>,
}

/// How a dictation ended. Only `Done` is a history entry; `Failed`, `NoText` and `NoModel` keep
/// the recording as the last failed one ([`Status::failed_recording`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
  Done,
  Failed,
  /// Speech (the VAD said so) of which the engine made no text: a model that does not know the
  /// language, most likely.
  NoText,
  /// Speech recorded while no model could transcribe it (none downloaded, the chosen one gone
  /// from the disk, one that does not load); the error says why.
  NoModel,
  /// No speech in the recording; the engine was not called. Nothing is kept.
  NoSpeech,
  /// Nothing is kept.
  Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Last {
  pub take: Take,
  pub entry: Option<i64>,
  pub outcome: Outcome,
  pub text: String,
  pub error: Option<String>,
  pub pasted: bool,
  /// Why the text was not pasted.
  pub not_pasted: Option<String>,
  pub seconds: f64,
}

/// What a platform shell shows, sent to the [`Observer`] after every change.
#[derive(Debug, Clone)]
pub struct Update {
  pub status: Status,
  pub settings: Settings,
  /// The level of the recording in progress.
  pub meter: Option<Meter>,
  /// Moves on every change of the history (an entry added, finished, deleted, cleaned up): a
  /// history window reloads then.
  pub history: u64,
}

/// Called on the session thread after every change of the status (the recording's time and level
/// aside), of the settings or of the history; it must return at once.
pub type Observer = Box<dyn FnMut(Update) + Send>;

/// A running session. Dropping it ends the session thread: a recording in progress is closed and
/// kept (it is adopted by the history on the next start), the engine is stopped.
pub struct Session {
  handle: Handle,
  thread: Option<JoinHandle<()>>,
}

/// Talks to the session thread; cheap to clone.
#[derive(Clone)]
pub struct Handle(Sender<Msg>);

enum Msg {
  Input(Input),
  Request(Request, Sender<Reply>),
  Edit(Box<dyn FnOnce(&mut Settings) + Send>),
  Transcribed {
    take: Take,
    result: Result<Transcription, EngineError>,
  },
  Retranscribed {
    target: Target,
    result: Result<Transcription, EngineError>,
  },
  /// The background thread removed entries.
  Cleaned,
  CheckModel,
  Quit,
}

impl Session {
  /// Loads the settings, opens the history in `config.home` and starts the session thread, which
  /// recovers the history before anything else.
  pub fn start<O: TextOutput>(config: Config, output: O) -> Result<Self> {
    Self::start_with(config, output, None)
  }

  /// [`start`](Self::start) with an observer, called once the history is recovered and then after
  /// every change.
  pub fn start_observed<O: TextOutput>(
    config: Config,
    output: O,
    observer: Observer,
  ) -> Result<Self> {
    Self::start_with(config, output, Some(observer))
  }

  fn start_with<O: TextOutput>(
    config: Config,
    output: O,
    observer: Option<Observer>,
  ) -> Result<Self> {
    fs::create_dir_all(&config.home)
      .with_context(|| format!("create {}", config.home.display()))?;
    let (settings, warning) = Settings::load(&config.home.join(settings::FILE));
    if let Some(w) = &warning {
      warn!("{w}");
    }

    let history = History::open(&config.home)?;
    let (tx, rx) = unbounded();
    let (engine_tx, engine_rx) = unbounded();
    let controller = Controller {
      machine: Machine::new(settings.activation),
      settings,
      history,
      output,
      config,
      engine: None,
      tx: tx.clone(),
      dictation: None,
      last: None,
      retranscribing: Vec::new(),
      orphans: VecDeque::new(),
      failed_recording: None,
      error: None,
      warnings: warning.into_iter().collect(),
      engine_stale: false,
      engine_tx,
      engine_rx,
      model_error: None,
      model_fallback: None,
      observer,
      published: None,
    };

    let thread = thread::Builder::new()
      .name("session".into())
      .spawn(move || controller.run(rx))
      .context("spawn the session thread")?;
    Ok(Self { handle: Handle(tx), thread: Some(thread) })
  }

  pub fn handle(&self) -> Handle {
    self.handle.clone()
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    let _ = self.handle.0.send(Msg::Quit);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

impl Handle {
  /// A hotkey event from the platform shell.
  pub fn input(&self, input: Input) {
    let _ = self.0.send(Msg::Input(input));
  }

  /// Changes the settings the session has with `edit`, saves them to `settings.json` and uses them
  /// from now on. Edits apply in order to the latest settings, so two quick changes never undo
  /// each other. A new model or unload timeout takes effect once the engine is not busy; the old
  /// engine is stopped in the background.
  pub fn edit_settings(&self, edit: impl FnOnce(&mut Settings) + Send + 'static) {
    let _ = self.0.send(Msg::Edit(Box::new(edit)));
  }

  /// The models on disk changed (a download ended, a model was deleted): whether one can
  /// transcribe is looked at again, and an engine of another model than that lets go of it.
  pub fn check_model(&self) {
    let _ = self.0.send(Msg::CheckModel);
  }

  /// Handles a control request; `quit` is refused, the app answers it.
  pub fn request(&self, request: Request) -> Reply {
    let (reply, rx) = bounded(1);
    if self.0.send(Msg::Request(request, reply)).is_err() {
      return Reply::error("the session has stopped");
    }

    rx.recv_timeout(REQUEST_TIMEOUT)
      .unwrap_or_else(|_| Reply::error("the session did not answer in time"))
  }
}

/// The engine for the model in the settings.
struct Loaded {
  /// The model file's name without extension.
  name: String,
  path: PathBuf,
  engine: Engine,
}

/// What a background transcription is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
  /// A history entry: new text replaces its text, a failure leaves it as it was.
  Entry(i64),
  /// A recording no entry names (left by a crash, the last failed one), as
  /// [`Recordings`](crate::history::recordings::Recordings) names it: its text becomes an entry, a
  /// failure keeps it as the last failed recording.
  Recording(String),
}

/// A background transcription, with the model and language it runs with.
struct Background {
  target: Target,
  model: String,
  language: Option<String>,
  /// How long the recording is.
  duration_ms: i64,
}

/// The dictation in progress.
struct Dictation<T> {
  /// How it ends, as it goes: the last dictation once it has ended (`outcome` until then:
  /// failed).
  last: Last,
  /// Noted once the recording is shown.
  target: Option<T>,
  /// Unix ms.
  created_at: i64,
  audio: String,
  started: Instant,
  /// The stop; the recorder still open then is in its post-roll.
  stopped: Option<Instant>,
  /// The audio delivered [`POST_ROLL_MIN`] after the stop: it reaches past the stop.
  post_roll_mark: Option<Duration>,
  recorder: Option<Recorder>,
  device: String,
  /// The engine started loading the model for it: that engine stays until the dictation ends.
  prewarmed: bool,
  job: Option<JobId>,
  /// The model and language of its job.
  model: Option<String>,
  language: Option<String>,
  duration_ms: i64,
  warnings: Vec<String>,
}

struct Controller<O: TextOutput> {
  config: Config,
  machine: Machine,
  settings: Settings,
  history: History,
  output: O,
  engine: Option<Loaded>,
  /// For the waiter threads.
  tx: Sender<Msg>,
  dictation: Option<Dictation<O::Target>>,
  last: Option<Last>,
  retranscribing: Vec<Background>,
  /// Recordings a crash left, waiting for their turn ([`Controller::transcribe_orphans`]).
  orphans: VecDeque<String>,
  /// When the last failed recording started, as last looked ([`Status::failed_recording`]).
  failed_recording: Option<i64>,
  error: Option<String>,
  warnings: Vec<String>,
  /// The engine no longer fits (the settings changed, its file is gone, an unload): replace it
  /// once it is not busy.
  engine_stale: bool,
  /// What the engines tell of their changes ([`EngineConfig::notify`]).
  engine_tx: Sender<()>,
  engine_rx: Receiver<()>,
  /// Why no model on disk can transcribe, as last looked ([`Controller::check_model`]); a load
  /// that fails is the engine's ([`Status::no_model`]).
  model_error: Option<NoModel>,
  /// The warning of a chosen model not on disk while another one transcribes, as last looked.
  model_fallback: Option<String>,
  observer: Option<Observer>,
  /// The status last sent to the observer (without the recording's time and level), the settings
  /// and the history's changes then.
  published: Option<(Status, Settings, u64)>,
}

impl<O: TextOutput> Controller<O> {
  fn run(mut self, rx: Receiver<Msg>) {
    self.startup();
    self.publish();
    let engine = self.engine_rx.clone();
    loop {
      // The recorder reports a microphone that stops by itself mid-recording.
      let failures = (self.dictation.as_ref())
        .and_then(|d| d.recorder.as_ref())
        .map(|r| r.failures().clone())
        .unwrap_or_else(never);

      // On a fixed beat from the stop, and at its end: requests in between do not put it off.
      let post_roll = match self.post_roll() {
        Some(elapsed) => {
          let ticks = elapsed.as_millis() / POST_ROLL_TICK.as_millis() + 1;
          let next = (POST_ROLL_TICK * ticks as u32).min(POST_ROLL_MAX);
          after(next.saturating_sub(elapsed))
        }
        None => never(),
      };

      select! {
        recv(rx) -> msg => match msg {
          Ok(Msg::Quit) | Err(_) => break,
          Ok(msg) => self.on_msg(msg),
        },
        recv(failures) -> failure => self.on_mic_failure(failure.unwrap_or_default()),
        recv(post_roll) -> _ => self.on_post_roll(),
        recv(engine) -> _ => self.on_engine_changed(),
      }

      self.publish();
    }

    if let Some(d) = &self.dictation {
      info!("dictation {}: the session ended before it did", d.last.take);
    }
  }

  /// Recovers what a crash left (before any recording can make new files): its recordings go to
  /// the engine in the background. Then starts the first retention in the background: with a lot
  /// to delete it would hold up the hotkey and `wavo-app ctl`.
  fn startup(&mut self) {
    let _ = self.check_model();
    self.failed_recording = self.history.recordings.last_failed().map(|(_, at)| at);
    match self.history.recover() {
      Ok(orphans) => self.orphans = orphans.into(),
      Err(e) => warn!("history recovery: {e:#}"),
    }

    self.transcribe_orphans();
    self.cleanup_in_background();
  }

  /// The retention (at start and when the setting changes), on its own thread with its own
  /// connection. Rows are deleted after their files, so the two never disagree.
  fn cleanup_in_background(&self) {
    let (home, retention) = (self.config.home.clone(), self.settings.history_retention);
    let busy = self.retranscribing_entries();
    if retention == Retention::Unlimited {
      return;
    }

    let tx = self.tx.clone();
    let work = move || {
      let history = match History::open(&home) {
        Ok(history) => history,
        Err(e) => return warn!("history: {e:#}"),
      };

      if retain(&history, retention, &busy) > 0 {
        let _ = tx.send(Msg::Cleaned);
      }
    };

    if let Err(e) = thread::Builder::new().name("history".into()).spawn(work) {
      warn!("history: {e}");
    }
  }

  /// Tells the observer, if the status, the settings or the history changed.
  fn publish(&mut self) {
    if self.observer.is_none() {
      return;
    }

    let status = self.status();
    let mut key = status.clone();
    if let Some(r) = key.recording.as_mut() {
      (r.seconds, r.level) = (0.0, 0.0);
    }

    let key = (key, self.settings.clone(), self.history.db.changes());
    if self.published.as_ref() == Some(&key) {
      return;
    }

    let history = key.2;
    self.published = Some(key);
    let meter = self.dictation.as_ref().and_then(|d| d.recorder.as_ref()).map(Recorder::meter);
    let update = Update { status, settings: self.settings.clone(), meter, history };
    if let Some(observer) = self.observer.as_mut() {
      observer(update);
    }
  }

  fn on_msg(&mut self, msg: Msg) {
    match msg {
      Msg::Input(input) => {
        // A press is a new attempt: the last one's error goes, to come back if it fails again.
        if matches!(input, Input::KeyDown { repeat: false, .. }) && self.error.take().is_some() {
          self.publish();
        }

        self.feed(input)
      }
      Msg::Request(request, reply) => {
        let _ = reply.send(self.on_request(request).into());
      }
      Msg::Edit(edit) => {
        let mut settings = self.settings.clone();
        edit(&mut settings);
        self.apply_settings(settings);
      }
      Msg::Transcribed { take, result } => self.on_transcribed(take, result),
      Msg::Retranscribed { target, result } => self.on_retranscribed(target, result),
      Msg::Cleaned => {} // the history's changes show it
      // An engine of another model than the one to use now (its file gone, the first one
      // downloaded another) stops.
      Msg::CheckModel => {
        let path = self.check_model().ok();
        if let Some(loaded) = &self.engine
          && path.as_ref() != Some(&loaded.path)
        {
          info!("model {}: another is to transcribe, its engine stops", loaded.name);
          self.retire_engine();
        }
      }
      Msg::Quit => unreachable!("handled by the loop"),
    }
  }

  fn on_request(&mut self, request: Request) -> Result<Value> {
    let command = match request {
      Request::Toggle => Command::Toggle,
      Request::Start => Command::Start,
      Request::Stop => Command::Stop,
      Request::Cancel => Command::Cancel,
      Request::Status => return Ok(serde_json::to_value(self.status())?),
      Request::Retranscribe { id } => {
        self.retranscribe(id)?;
        return Ok(json!({ "entry": id, "state": "transcribing" }));
      }
      Request::RetranscribeLast => {
        let state = self.retranscribe_last()?;
        return Ok(json!({ "state": state }));
      }
      Request::Delete { id } => {
        self.delete(id)?;
        return Ok(json!({ "entry": id, "state": "deleted" }));
      }
      Request::Unload => {
        info!("the model is unloaded on request");
        self.retire_engine();
        let state = if self.engine.is_some() { "unloading" } else { "unloaded" };
        return Ok(json!({ "state": state }));
      }
      Request::OpenSettings | Request::OpenHistory | Request::Autostart { .. } | Request::Quit => {
        bail!("{request:?} is not handled by the session")
      }
    };

    self.error = None;
    self.feed(Input::Command(command));
    let state = self.phase();
    if let (Some(e), Command::Start | Command::Toggle, Phase::Idle) = (&self.error, command, state)
    {
      bail!("{e}");
    }

    Ok(json!({ "state": state }))
  }

  fn feed(&mut self, input: Input) {
    let Some(effect) = self.machine.handle(input) else { return };
    debug!(?input, ?effect, "session");
    match effect {
      Effect::StartRecording { take } => self.start_recording(take),
      Effect::StopRecording { take } => self.stop_recording(take),
      Effect::CancelRecording { take } => self.cancel_recording(take),
      Effect::AbortRecording { take } => {
        // Nothing to tell: the last dictation, and its warnings, stay the ones before.
        let (last, warnings) = (self.last.take(), self.warnings.clone());
        self.cancel_recording(take);
        (self.last, self.warnings) = (last, warnings);
      }
      Effect::CancelTranscription { take } => self.cancel_transcription(take),
      Effect::Deliver { take } => self.deliver(take),
    }
  }

  fn phase(&self) -> Phase {
    match self.machine.state() {
      State::Idle => Phase::Idle,
      State::Recording { .. } => Phase::Recording,
      State::Transcribing { .. } => Phase::Transcribing,
      State::Pasting { .. } => Phase::Pasting,
    }
  }

  fn apply_settings(&mut self, new: Settings) {
    if new == self.settings {
      return;
    }

    let path = self.config.home.join(settings::FILE);
    if let Err(e) = new.save(&path) {
      // Used anyway; only the next start would not have them.
      warn!("{e:#}");
      self.error = Some(format!("could not save the settings: {e:#}"));
    }

    let old = std::mem::replace(&mut self.settings, new);
    info!("settings changed");
    self.machine.set_activation(self.settings.activation);
    if old.model != self.settings.model
      || old.unload_after_minutes != self.settings.unload_after_minutes
    {
      self.retire_engine();
    }

    // Another model: whether it can transcribe.
    if old.model != self.settings.model {
      let _ = self.check_model();
    }

    if old.history_retention != self.settings.history_retention {
      self.cleanup_in_background();
    }
  }

  /// Ends the engine and frees the model now, or once it is not busy; the next use starts one with
  /// the current settings.
  fn retire_engine(&mut self) {
    self.engine_stale = self.engine.is_some();
    self.retire_stale_engine();
  }

  /// Stops an engine that no longer fits, unless it is working for a dictation (from its prewarm
  /// on: it would load the model twice) or an entry; the next use starts one with the current
  /// settings. The old one shuts down on its own thread: its engine thread gets up to a few seconds
  /// to drop the model.
  fn retire_stale_engine(&mut self) {
    let prewarmed = self.dictation.as_ref().is_some_and(|d| d.prewarmed) && self.engine_can_serve();
    let dictation = prewarmed || self.dictation.as_ref().is_some_and(|d| d.job.is_some());
    let busy = dictation || !self.retranscribing.is_empty();
    if !self.engine_stale || busy {
      return;
    }

    self.engine_stale = false;
    if let Some(old) = self.engine.take() {
      info!("model {}: its engine stops", old.name);
      let retire = move || drop(old);
      if let Err(e) = thread::Builder::new().name("engine-retire".into()).spawn(retire) {
        warn!("stop the old engine: {e}");
      }
    }
  }

  /// Whether a model can transcribe now, without loading one: the files on disk against the
  /// choice. A chosen model that is not on disk gives way to the first one found, with a warning
  /// (kept for the status); the choice stays. With none found the choice is the reason. The
  /// model's path, or why there is none (kept for the status).
  fn check_model(&mut self) -> Result<PathBuf, NoModel> {
    let hub = &self.config.hub;
    let query = self.settings.model.clone();
    let error = match (model::find(query.as_deref(), hub), query) {
      (Ok(path), _) => {
        (self.model_error, self.model_fallback) = (None, None);
        return Ok(path);
      }
      (Err(_), Some(query)) => match model::find(None, hub) {
        Ok(path) => {
          let fallback = label(&path.to_string_lossy());
          let note = format!("{} is not on disk: {fallback} transcribes instead", label(&query));
          if self.model_fallback.as_ref() != Some(&note) {
            warn!("model {note}");
          }

          (self.model_error, self.model_fallback) = (None, Some(note));
          return Ok(path);
        }
        Err(_) => NoModel::Gone { model: label(&query) },
      },
      (Err(_), None) => NoModel::NoneDownloaded,
    };

    if self.model_error.as_ref() != Some(&error) {
      warn!("no model: {}", error.to_string());
    }

    self.model_fallback = None;

    self.model_error = Some(error.clone());
    Err(error)
  }

  /// An engine told of a change ([`EngineConfig::notify`]); the loop shows it. A load that failed
  /// may be a file gone.
  fn on_engine_changed(&mut self) {
    while self.engine_rx.try_recv().is_ok() {}
    if self.engine.as_ref().is_some_and(|l| l.engine.status().load_error.is_some()) {
      let _ = self.check_model();
    }
  }

  /// Whether the engine has its model, or can still load it: not after a load that failed, nor
  /// while it loads a file that is gone.
  fn engine_can_serve(&self) -> bool {
    let Some(loaded) = &self.engine else { return false };
    let status = loaded.engine.status();
    match status.phase {
      EnginePhase::Idle | EnginePhase::Running => true,
      EnginePhase::Stopped if status.load_error.is_some() => false,
      EnginePhase::Stopped | EnginePhase::Loading => loaded.path.is_file(),
    }
  }

  /// The engine for the model in the settings, started on first use; or why there is none. One
  /// whose file is gone goes, once it is not busy, as does one standing in for the chosen model
  /// once that is back.
  fn engine(&mut self) -> Result<&Loaded, NoModel> {
    if let Some(loaded) = &self.engine
      && !loaded.path.is_file()
    {
      warn!("model {}: its file is gone", loaded.name);
      self.engine_stale = true;
    }

    // A stand-in for the chosen model, which is back (a few file lookups, of that model alone).
    if let (Some(loaded), Some(chosen)) = (&self.engine, &self.settings.model)
      && model::find(Some(chosen), &self.config.hub).is_ok_and(|p| p != loaded.path)
    {
      info!("model {}: the chosen model is back", loaded.name);
      self.engine_stale = true;
    }

    self.retire_stale_engine();
    if self.engine.is_none() {
      let path = self.check_model()?;
      let name = file_stem(&path);
      info!("model {name} ({})", path.display());
      let config = EngineConfig {
        loader: self.config.loader.clone(),
        model: path.clone(),
        deadlines: self.config.deadlines,
        unload_after: self.settings.unload_after(),
        notify: Some(self.engine_tx.clone()),
      };

      // Without its thread the model cannot load: the recording waits, as for a load that fails.
      let engine = Engine::start(config)
        .map_err(|e| NoModel::LoadFailed { model: label(&name), error: e.to_string() })?;
      self.engine = Some(Loaded { name, path, engine });
    }

    Ok(self.engine.as_ref().unwrap())
  }

  /// The microphone first, and the shell shows the recording at once; then, while it already
  /// records, the focus check (up to a second) and the engine starts loading the model.
  fn start_recording(&mut self, take: Take) {
    let started = Instant::now();
    let created_at = now_ms();
    let (audio, wav) = self.history.recordings.new_file(created_at);
    let source = Source::new(self.config.fake_mic.clone(), self.settings.microphone.clone());

    let recorder = match Recorder::start(&source, &wav) {
      Ok(recorder) => recorder,
      Err(e) => {
        let error = format!("could not start recording: {e:#}");
        warn!("dictation {take}: {error}");
        self.error = Some(error);
        // The recording was not made: its day folder goes if nothing else is in it.
        let _ = self.history.recordings.discard(&audio);
        return self.feed(Input::RecordingFailed { take });
      }
    };

    let opened = started.elapsed();
    info!("dictation {take}: recording from {:?}", recorder.device());
    let warnings: Vec<String> = recorder.warning().map(str::to_owned).into_iter().collect();
    for w in &warnings {
      warn!("{w}");
    }

    self.dictation = Some(Dictation {
      last: Last {
        take,
        entry: None,
        outcome: Outcome::Failed,
        text: String::new(),
        error: None,
        pasted: false,
        not_pasted: None,
        seconds: 0.0,
      },
      target: None,
      created_at,
      audio,
      started,
      stopped: None,
      post_roll_mark: None,
      device: recorder.device().to_owned(),
      recorder: Some(recorder),
      prewarmed: false,
      job: None,
      model: None,
      language: None,
      duration_ms: 0,
      warnings,
    });
    self.publish();

    // Where the person types, before the model lookup can take time and the focus move.
    let target = self.output.target();
    debug!(
      "dictation {take}: microphone open in {opened:.0?}, focus noted after {:.0?}",
      started.elapsed()
    );
    if let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take) {
      d.target = Some(target);
    }

    // The engine thread loads the model while the person speaks; without one it records anyway,
    // the status saying why from the start.
    match self.engine() {
      Ok(loaded) => loaded.engine.prewarm(),
      Err(e) => return info!("dictation {take}: recording without a model: {}", e.to_string()),
    }

    if let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take) {
      d.prewarmed = true;
    }
  }

  fn on_mic_failure(&mut self, failure: String) {
    let Some(d) = &self.dictation else { return };
    warn!("dictation {}: the recording stopped: {failure}", d.last.take);
    let take = d.last.take;
    // In the post-roll the machine is past the recording: the recording ends here. Either way the
    // recorder goes, and with it the failures channel, which stays ready once it is closed.
    match d.stopped {
      Some(_) => self.transcribe(take),
      None => self.feed(Input::MicStopped { take }),
    }
  }

  /// Closes the microphone of dictation `take`: its audio and duration, or what went wrong.
  fn close_recorder(&mut self, take: Take) -> Option<Result<Clip, String>> {
    let d = self.dictation.as_mut().filter(|d| d.last.take == take)?;
    let recorder = d.recorder.take()?;
    d.stopped.get_or_insert_with(Instant::now);
    Some(match recorder.stop() {
      Ok(rec) => {
        for w in rec.warnings {
          if !d.warnings.contains(&w) {
            warn!("dictation {take}: {w}");
            d.warnings.push(w);
          }
        }

        d.duration_ms = rec.clip.pcm.len() as i64 * 1000 / i64::from(SAMPLE_RATE);
        Ok(rec.clip)
      }
      Err(e) => Err(format!("the recording failed: {e:#}")),
    })
  }

  /// Starts the post-roll; a microphone that failed is closed at once.
  fn stop_recording(&mut self, take: Take) {
    let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take && d.recorder.is_some())
    else {
      // Cannot happen (a recording that started has a recorder); never stay `Transcribing`.
      warn!("dictation {take}: nothing to stop");
      return self.feed(Input::Finished { take });
    };

    d.stopped = Some(Instant::now());
    if d.recorder.as_ref().is_some_and(Recorder::failed) {
      self.transcribe(take);
    }
  }

  /// How long the dictation in its post-roll has been in it.
  fn post_roll(&self) -> Option<Duration> {
    let d = self.dictation.as_ref().filter(|d| d.recorder.is_some())?;
    Some(d.stopped?.elapsed())
  }

  fn on_post_roll(&mut self) {
    let Some(d) = self.dictation.as_mut() else { return };
    let (Some(stopped), Some(recorder)) = (d.stopped, d.recorder.as_ref()) else { return };
    let elapsed = stopped.elapsed();
    if elapsed >= POST_ROLL_MIN && d.post_roll_mark.is_none() {
      d.post_roll_mark = Some(recorder.delivered());
    }

    // The VAD's word on the audio up to the stop, once it has scored it.
    let quiet =
      d.post_roll_mark.filter(|&mark| recorder.scored() >= mark).map(|_| recorder.quiet());
    if recorder.failed() || post_roll_over(elapsed, quiet) {
      let take = d.last.take;
      debug!("dictation {take}: post-roll {elapsed:.0?}");
      self.transcribe(take);
    }
  }

  /// Closes the microphone of dictation `take` and hands the recording to the engine.
  fn transcribe(&mut self, take: Take) {
    let Some(recording) = self.close_recorder(take) else {
      // Cannot happen (a recording that started has a recorder); never stay `Transcribing`.
      warn!("dictation {take}: nothing to stop");
      return self.feed(Input::Finished { take });
    };

    let input = match recording {
      Ok(clip) => clip.engine_input(),
      Err(error) => return self.end(take, Outcome::Failed, Some(error)),
    };

    let Some(input) = input else {
      return self.end(take, Outcome::NoSpeech, None);
    };

    let language = self.settings.engine_language();
    let (model, pending) = match self.engine() {
      Ok(loaded) => (loaded.name.clone(), loaded.engine.transcribe(input.pcm, language.clone())),
      Err(e) => return self.end(take, Outcome::NoModel, Some(e.to_string())),
    };

    let d = self.dictation.as_mut().unwrap();
    (d.job, d.model, d.language) = (Some(pending.id()), Some(model), language);
    let tx = self.tx.clone();
    spawn_waiter(move || {
      let _ = tx.send(Msg::Transcribed { take, result: pending.wait() });
    });
  }

  /// Ends dictation `take` before it reached the engine with `outcome`, then idle.
  fn end(&mut self, take: Take, outcome: Outcome, error: Option<String>) {
    if let Some(e) = &error {
      warn!("dictation {take}: {e}");
    }

    if let Some(d) = self.dictation.as_mut() {
      d.last.outcome = outcome;
      d.last.error = error;
    }

    self.settle(take);
    self.feed(Input::Finished { take });
    self.complete();
  }

  /// Keeps what dictation `take` leaves by its outcome: its text as an entry, a failure's
  /// recording as the last failed one; nothing of the rest (cancelled, no speech).
  fn settle(&mut self, take: Take) {
    let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take) else { return };
    match d.last.outcome {
      Outcome::Done => {
        let entry = NewEntry {
          created_at: d.created_at,
          duration_ms: d.duration_ms,
          audio_path: Some(&d.audio),
          model: d.model.as_deref(),
          language: d.language.as_deref(),
          text: d.last.text.trim(),
        };

        d.last.entry = add_entry(&self.history, &entry);
      }
      Outcome::Failed | Outcome::NoText | Outcome::NoModel => {
        let audio = d.audio.clone();
        self.keep_failed(&audio);
      }
      Outcome::NoSpeech | Outcome::Cancelled => {
        if let Err(e) = self.history.recordings.discard(&d.audio) {
          warn!("dictation {take}: {e:#}");
        }
      }
    }
  }

  /// Keeps recording `name` as the last failed one (unless that is newer).
  fn keep_failed(&mut self, name: &str) {
    match self.history.recordings.keep_failed(name) {
      Ok(at) => {
        let kept = recordings::started_at(name) == Some(at);
        info!("recording {name}: {}", if kept { "kept to try again" } else { "deleted, older" });
        self.failed_recording = Some(at);
      }
      Err(e) => {
        warn!("recording {name}: {e:#}");
        self.failed_recording = self.history.recordings.last_failed().map(|(_, at)| at);
      }
    }
  }

  fn on_transcribed(&mut self, take: Take, result: Result<Transcription, EngineError>) {
    if !self.dictation.as_ref().is_some_and(|d| d.last.take == take && d.job.is_some()) {
      debug!("dictation {take}: late result ignored");
      return;
    }

    let (outcome, text, error) = self.outcome(result);
    if let Some(e) = &error {
      warn!("dictation {take}: {outcome:?}: {e}");
    }

    let d = self.dictation.as_mut().unwrap();
    d.job = None;
    (d.last.outcome, d.last.text, d.last.error) = (outcome, text, error);
    self.settle(take);
    if outcome == Outcome::NoModel {
      // The model did not load: its file may be gone.
      let _ = self.check_model();
    }

    if outcome == Outcome::Done {
      self.feed(Input::Transcribed { take });
    } else {
      self.feed(Input::Finished { take });
      self.complete();
    }
  }

  /// The outcome, text and error of an engine's result: an error for every outcome but
  /// [`Outcome::Done`]. A transcription without text, of speech the VAD heard, is
  /// [`Outcome::NoText`]; a model that does not load, [`Outcome::NoModel`].
  fn outcome(
    &self,
    result: Result<Transcription, EngineError>,
  ) -> (Outcome, String, Option<String>) {
    match result {
      Ok(t) => {
        debug!("{:.2?} on {} in {} piece(s)", t.elapsed, t.device, t.pieces.len());
        match t.text.trim() {
          "" => (Outcome::NoText, String::new(), Some("nothing recognized".into())),
          _ => (Outcome::Done, t.text, None),
        }
      }
      Err(EngineError::Cancelled) => {
        (Outcome::Cancelled, String::new(), Some("the transcription was cancelled".into()))
      }
      Err(EngineError::Load(error)) => {
        let model = self.engine.as_ref().map_or_else(|| "the model".into(), |l| label(&l.name));
        (Outcome::NoModel, String::new(), Some(NoModel::LoadFailed { model, error }.to_string()))
      }
      Err(e) => (Outcome::Failed, String::new(), Some(e.to_string())),
    }
  }

  fn deliver(&mut self, take: Take) {
    let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take) else {
      return self.feed(Input::Delivered { take });
    };

    let keep = self.settings.keep_in_clipboard;
    let auto_paste = self.settings.auto_paste;
    let refused = if auto_paste {
      match &d.target {
        Some(target) => self.output.paste(&d.last.text, target, keep).err().map(|e| e.to_string()),
        None => Some("the app in focus when the dictation started is unknown".to_owned()),
      }
    } else {
      // Without auto-paste the clipboard is how the text gets out.
      Some(match self.output.copy(&d.last.text) {
        Ok(()) => "auto-paste is off; the text is on the clipboard".to_owned(),
        Err(e) => format!("auto-paste is off, and copying failed: {e:#}"),
      })
    };

    d.last.pasted = refused.is_none();
    if let Some(reason) = &refused {
      info!("dictation {take}: not pasted: {reason}");
    }

    d.last.not_pasted = refused;
    if let Some(entry) = d.last.entry {
      // Without auto-paste nothing was refused: the history keeps no reason.
      let why = d.last.not_pasted.as_deref().filter(|_| auto_paste);
      log_history(self.history.db.mark_pasted(entry, d.last.pasted, why));
    }

    self.feed(Input::Delivered { take });
    self.complete();
  }

  /// The person cancelled: nothing of the dictation is kept.
  fn cancel_recording(&mut self, take: Take) {
    let error = match self.close_recorder(take) {
      Some(Err(e)) => Some(e),
      _ => None,
    };

    if let Some(d) = self.dictation.as_mut() {
      (d.last.outcome, d.last.error) = (Outcome::Cancelled, error);
    }

    self.settle(take);

    self.complete();
  }

  fn cancel_transcription(&mut self, take: Take) {
    self.close_recorder(take); // in the post-roll
    let Some(d) = self.dictation.as_mut().filter(|d| d.last.take == take) else { return };
    d.last.outcome = Outcome::Cancelled;
    if let (Some(job), Some(loaded)) = (d.job.take(), &self.engine) {
      loaded.engine.cancel(job);
    }

    self.settle(take);
    self.complete();
  }

  /// The dictation is over: it becomes the last one, and the retention runs. An engine prewarmed
  /// for it unloads as usual, also when no job came (cancelled, no speech).
  fn complete(&mut self) {
    let Some(Dictation { mut last, duration_ms, stopped, device, prewarmed, warnings, .. }) =
      self.dictation.take()
    else {
      return;
    };

    if prewarmed && let Some(loaded) = &self.engine {
      loaded.engine.release();
    }

    last.seconds = duration_ms as f64 / 1000.0;
    let after_stop = stopped.map(|s| format!(", {:.0?} after the stop", s.elapsed()));
    info!(
      "dictation {}: {:?} ({:.1} s from {device:?}{}{})",
      last.take,
      last.outcome,
      last.seconds,
      after_stop.unwrap_or_default(),
      if last.pasted { ", pasted" } else { "" },
    );
    self.warnings.retain(|w| !w.starts_with("dictation "));
    self.warnings.extend(warnings.into_iter().map(|w| format!("dictation {}: {w}", last.take)));
    self.last = Some(last);
    self.cleanup();
    self.retire_stale_engine();
  }

  /// Entry `id`'s recording through the engine again, behind any dictation; its text stays until
  /// new text replaces it.
  fn retranscribe(&mut self, id: i64) -> Result<()> {
    let entry = self.history.db.get_entry(id)?.ok_or_else(|| anyhow!("no history entry {id}"))?;
    if self.retranscribing.iter().any(|b| b.target == Target::Entry(id)) {
      bail!("entry {id} is being transcribed");
    }

    let wav = self
      .history
      .audio_file(&entry)
      .ok_or_else(|| anyhow!("the recording of entry {id} is gone"))?;
    if !self.transcribe_background(Target::Entry(id), &wav)? {
      bail!("no speech in the recording of entry {id}: it stays as it is");
    }

    Ok(())
  }

  /// The last failed recording through the engine again, as [`Target::Recording`]. Returns the
  /// state: `transcribing`, or `discarded` for a recording without speech.
  fn retranscribe_last(&mut self) -> Result<&'static str> {
    let name = self.history.recordings.restore_failed()?;
    let name = name.ok_or_else(|| anyhow!("no failed recording to transcribe again"))?;
    self.failed_recording = None;
    self.transcribe_recording(name)
  }

  /// Recording `name`, which no entry names, through the engine in the background (see
  /// [`Target::Recording`]); one without speech goes. Returns the state: `transcribing` or
  /// `discarded`. If it cannot start, the recording is the last failed one.
  fn transcribe_recording(&mut self, name: String) -> Result<&'static str> {
    let path = self.history.recordings.path(&name).context("not a recording");
    match path.and_then(|path| self.transcribe_background(Target::Recording(name.clone()), &path)) {
      Ok(true) => Ok("transcribing"),
      Ok(false) => {
        info!("recording {name}: no speech, deleted");
        self.history.recordings.discard(&name)?;
        Ok("discarded")
      }
      Err(e) => {
        self.keep_failed(&name);
        Err(e)
      }
    }
  }

  /// Recordings a crash left, one at a time once none is running: each holds its audio in memory
  /// until it is transcribed. Without a model they stay where they are, for a start with one.
  fn transcribe_orphans(&mut self) {
    let running = self.retranscribing.iter().any(|b| matches!(b.target, Target::Recording(_)));
    if running || self.orphans.is_empty() || self.check_model().is_err() {
      return;
    }

    while let Some(name) = self.orphans.pop_front() {
      info!("recording {name}: left without an entry, transcribed in the background");
      match self.transcribe_recording(name) {
        Ok("transcribing") => return,
        Ok(_) => {}
        Err(e) => self.error = Some(format!("a recording could not be transcribed: {e:#}")),
      }
    }
  }

  /// Recording `wav` for `target` to the engine, behind any dictation; false if it has no speech.
  fn transcribe_background(&mut self, target: Target, wav: &Path) -> Result<bool> {
    // The model first: without one the recording is not worth reading.
    self.engine()?;
    let clip = Clip::analyze(wav::read(wav)?);
    let Some(input) = clip.engine_input() else { return Ok(false) };
    let language = self.settings.engine_language();
    let loaded = self.engine.as_ref().expect("started above");
    let model = loaded.name.clone();
    info!("{target:?}: transcribing {:.1} s with {model}", clip.seconds());
    let pending = loaded.engine.transcribe_background(input.pcm, language.clone());
    let duration_ms = (clip.seconds() * 1000.0) as i64;
    let job = Background { target: target.clone(), model, language, duration_ms };
    self.retranscribing.push(job);
    let tx = self.tx.clone();
    spawn_waiter(move || {
      let _ = tx.send(Msg::Retranscribed { target, result: pending.wait() });
    });

    Ok(true)
  }

  /// Deletes entry `id` with its recording, unless it is being transcribed. The last dictation's
  /// text goes with it: what the person deleted is not kept anywhere.
  fn delete(&mut self, id: i64) -> Result<()> {
    let current = self.dictation.as_ref().is_some_and(|d| d.last.entry == Some(id));
    if current || self.retranscribing.iter().any(|b| b.target == Target::Entry(id)) {
      bail!("entry {id} is being transcribed");
    }

    if !self.history.delete(id)? {
      bail!("no history entry {id}");
    }

    info!("entry {id}: deleted with its recording");
    if self.last.as_ref().is_some_and(|l| l.entry == Some(id)) {
      self.last = None;
    }

    Ok(())
  }

  /// A background transcription's result (see [`Target`]); a failure is the session's error.
  fn on_retranscribed(&mut self, target: Target, result: Result<Transcription, EngineError>) {
    let Some(at) = self.retranscribing.iter().position(|b| b.target == target) else { return };
    let Background { target, model, language, duration_ms } = self.retranscribing.remove(at);
    let (outcome, text, error) = self.outcome(result);
    let text = text.trim();
    let language = language.as_deref();
    match (target, outcome) {
      (Target::Entry(id), Outcome::Done) => {
        match self.history.db.update_text(id, text, &model, language) {
          Ok(true) => info!("entry {id}: transcribed again"),
          Ok(false) => info!("entry {id}: transcribed again, but deleted meanwhile"),
          Err(e) => warn!("history: {e:#}"),
        }
      }
      (Target::Recording(name), Outcome::Done) => {
        let entry = NewEntry {
          created_at: recordings::started_at(&name).unwrap_or_else(now_ms),
          duration_ms,
          audio_path: Some(&name),
          model: Some(&model),
          language,
          text,
        };

        add_entry(&self.history, &entry);
        self.cleanup();
      }
      (target, _) => {
        let why = error.unwrap_or_default();
        let error = match target {
          Target::Entry(id) => format!("entry {id} could not be transcribed again: {why}"),
          Target::Recording(name) => {
            self.keep_failed(&name);
            format!("a recording could not be transcribed: {why}")
          }
        };

        warn!("{error}");
        self.error = Some(error);
      }
    }

    self.transcribe_orphans();
    self.retire_stale_engine();
    if outcome == Outcome::NoModel {
      let _ = self.check_model();
    }
  }

  fn cleanup(&mut self) {
    retain(&self.history, self.settings.history_retention, &self.retranscribing_entries());
  }

  /// The entries being transcribed again.
  fn retranscribing_entries(&self) -> Vec<i64> {
    let entries = self.retranscribing.iter().filter_map(|b| match b.target {
      Target::Entry(id) => Some(id),
      Target::Recording(_) => None,
    });
    entries.collect()
  }

  fn status(&self) -> Status {
    let d = self.dictation.as_ref();
    let engine = self.engine.as_ref().map(|l| (l, l.engine.status()));
    let mut warnings = self.warnings.clone();
    warnings.extend(self.model_fallback.clone());
    if let Some(d) = d {
      warnings.extend(d.warnings.iter().map(|w| format!("dictation {}: {w}", d.last.take)));
    }

    let load_failed = engine.as_ref().and_then(|(l, s)| {
      let error = s.load_error.clone()?;
      Some(NoModel::LoadFailed { model: label(&l.name), error })
    });

    Status {
      state: self.phase(),
      activation: self.settings.activation,
      entry: d.and_then(|d| d.last.entry),
      recording: d.and_then(|d| {
        let r = d.recorder.as_ref()?;
        Some(RecordingStatus {
          device: r.device().to_owned(),
          seconds: d.started.elapsed().as_secs_f64(),
          level: r.level(),
        })
      }),
      engine: engine.map_or_else(EngineStatus::default, |(l, s)| EngineStatus {
        model: Some(l.name.clone()),
        phase: s.phase,
        thread: s.thread,
        progress: s.progress,
        device: s.device,
      }),
      last: self.last.clone(),
      retranscribing: self.retranscribing_entries(),
      failed_recording: self.failed_recording,
      error: self.error.clone(),
      warnings,
      no_model: self.model_error.clone().or(load_failed),
    }
  }
}

/// Applies `retention` to `history`, but to the `busy` entries; how many entries went.
fn retain(history: &History, retention: Retention, busy: &[i64]) -> usize {
  let started = Instant::now();
  match history.cleanup(retention, now_ms(), busy) {
    Ok(0) => 0,
    Ok(n) => {
      info!("history: {n} old entries removed ({retention:?}) in {:.0?}", started.elapsed());
      n
    }
    Err(e) => {
      warn!("history cleanup: {e:#}");
      0
    }
  }
}

fn log_history(result: Result<()>) {
  if let Err(e) = result {
    warn!("history: {e:#}");
  }
}

/// Adds `entry` to `history`; without it the text still reaches the person, and its recording,
/// which no entry names, is transcribed again on the next start.
fn add_entry(history: &History, entry: &NewEntry<'_>) -> Option<i64> {
  match history.db.insert_entry(entry) {
    Ok(id) => Some(id),
    Err(e) => {
      warn!("{:?}: could not add it to the history: {e:#}", entry.audio_path);
      None
    }
  }
}

/// The post-roll that began `elapsed` ago is over; `quiet` is how long the recording ends without
/// speech, once the VAD has scored the audio up to the stop.
fn post_roll_over(elapsed: Duration, quiet: Option<Duration>) -> bool {
  elapsed >= POST_ROLL_MAX || quiet.is_some_and(|q| q >= POST_ROLL_QUIET)
}

/// Waits for a transcription off the session thread.
fn spawn_waiter(wait: impl FnOnce() + Send + 'static) {
  thread::Builder::new().name("engine-wait".into()).spawn(wait).expect("spawn a waiter thread");
}

/// A model file's name without extension, as entries keep it: `gigaam-v3-e2e-rnnt-Q8_0`.
fn file_stem(path: &Path) -> String {
  path.file_stem().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into())
}

/// What a model choice (wavo's name, a file's path or name) is called where it shows, also once
/// its file is gone: wavo's full name (`parakeet-tdt-0.6b-v3`), else the file's name without
/// extension.
pub fn label(model: &str) -> String {
  if let Ok(info) = hub::lookup(model) {
    return info.full_name.into();
  }

  let file = Path::new(model).file_name().and_then(|f| f.to_str()).unwrap_or(model);
  let file = file.strip_suffix(".gguf").unwrap_or(file);
  let info = hub::MODELS.iter().find(|m| m.file().strip_suffix(".gguf") == Some(file));
  info.map_or_else(|| file.to_owned(), |m| m.full_name.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_post_roll_ends_with_the_speech_within_its_bounds() {
    let ms = Duration::from_millis;
    // The audio up to the stop not scored yet: however quiet before it.
    assert!(!post_roll_over(ms(100), None));
    assert!(post_roll_over(ms(60), Some(POST_ROLL_QUIET)));
    // A word in progress: until it ends…
    assert!(!post_roll_over(ms(150), Some(ms(80))));
    assert!(post_roll_over(ms(150), Some(ms(96))));
    // …or the maximum.
    assert!(!post_roll_over(ms(299), Some(Duration::ZERO)));
    assert!(post_roll_over(POST_ROLL_MAX, None));
  }

  #[test]
  fn models_are_labelled_by_wavos_full_name_else_the_file() {
    for name in ["parakeet-v3", "parakeet-tdt-0.6b-v3", "/x/parakeet-tdt-0.6b-v3-Q8_0.gguf"] {
      assert_eq!(label(name), "parakeet-tdt-0.6b-v3", "{name}");
    }

    assert_eq!(label("gigaam-v3-e2e-rnnt-Q8_0"), "gigaam-v3-e2e-rnnt", "an entry's model");
    assert_eq!(label("/models/broken.gguf"), "broken");
  }
}
