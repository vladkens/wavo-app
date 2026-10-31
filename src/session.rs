// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation session: one "session" thread owns the state [`Machine`], the recorder, the engine,
//! the history and the text output. The engine runs on its own thread, so the session answers in
//! milliseconds in every state.
//!
//! - A recording with speech gets its entry (busy, without text) before the engine has it: a crash
//!   leaves an entry to transcribe again, which the next start recovers before any recording.
//! - Text is optional: no model, an engine error or nothing recognized leave the entry without
//!   text. Such entries are transcribed by themselves once a model can: one at a time, started
//!   while no dictation runs, each by the model its language goes to ([route]) unless that one
//!   tried it already (`tried_model`). A dictation that starts meanwhile goes first on the same
//!   model's engine, and runs beside it on another one's. An entry's language is the one it was
//!   recorded in: a model of one language's own, else the setting's (none: detected).
//! - A chosen model not on disk gives way to the first one found, with a warning; the choice stays.
//! - A model key ([settings::ModelKey]) sends a dictation to its model: a letter or digit pressed
//!   while the hotkey is held, or a hotkey of the model's own. Its model not on disk gets no
//!   stand-in: the recording is kept without text. Each model has an engine of its own, with its
//!   own idle unload.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fs, mem};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, at, bounded, never, select, unbounded};
use nimbo::hotkey::Edge;
use tracing::{debug, info, warn};
use wavo::hub::{self, Hub, ModelInfo};

use crate::audio::{self, Clip, Meter, Recorder, Source};
use crate::engine::{self, Engine, EngineError, Loader, Wavo};
use crate::history::{Entry, History, now_ms};
use crate::settings::{self, Activation, Settings, language_name};

/// The microphone stays open at least this long after the stop, for the end of a word the key-up
/// cut.
const POST_ROLL: Duration = Duration::from_millis(150);
/// While the voice goes on after the stop, the microphone stays open until it pauses this long
/// ([tail_end]): the end of a word. The VAD's scores fall below a pause 50–100 ms after the
/// speech, and its count runs about 40 ms behind the microphone.
const TAIL_PAUSE: Duration = Duration::from_millis(120);
/// The microphone closes this long after the stop at the latest: after speech the VAD takes a
/// breath or a fan's hiss for voice for seconds.
const TAIL_MAX: Duration = Duration::from_millis(450);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

// MARK: Machine

/// In [`Activation::HoldOrToggle`], a press held at least this long is push-to-talk; a shorter
/// one is a tap that latches the recording on.
pub const HOLD_THRESHOLD: Duration = Duration::from_millis(300);
/// A press this soon after the previous one, with no release in between, is autorepeat even when
/// the platform does not flag it (macOS repeats a key after at most ~1.8 s): a lost release costs
/// at most this long.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(2);

/// Number of a dictation, from 1.
pub type Take = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
  Idle,
  Recording { take: Take, hold: Hold },
  Transcribing { take: Take },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
  /// The hotkey that started the recording is down since then: its release decides.
  Held(Instant),
  /// Held, and a model key came: the release stops the recording, however short the press.
  Keyed,
  /// Records until the next press: a tap or toggle mode started it.
  Latched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
  /// Hotkey `which` pressed, or held (`repeat`): 0 is the main one, the others the models' own
  /// ([Settings::own_hotkeys], in order), whose dictations go to their models.
  KeyDown { at: Instant, repeat: bool, which: usize },
  /// Hotkey `which` released.
  KeyUp { at: Instant, which: usize },
  /// Model key `index` ([Settings::held_keys], a letter or digit) pressed while the dictation's
  /// hotkey is down: the dictation goes to its model, the last one pressed. A held press then stops
  /// at its release, however short; in toggle mode the recording stays latched. Not after a tap
  /// latched it.
  #[cfg_attr(not(test), allow(dead_code, reason = "the hotkeys send it once nimbo reports keys"))]
  ModelKey { index: usize },
  /// Another key went down while hotkey `which`, a modifier alone, was held: the press was a chord
  /// (⌥E, or fn+1 when that is a hotkey too). A recording it started is dropped without a trace.
  KeyAborted { which: usize },
  /// Cancels a recording or its transcription.
  Escape,
  /// Starts a recording that runs until the next one, or stops it: `wavo-app toggle`, for a
  /// desktop's own shortcut where Wavo's hotkey can't be (GNOME before 48 on Wayland).
  Toggle,
  /// The microphone did not open.
  RecordingFailed { take: Take },
  /// The microphone stopped by itself: what it recorded is transcribed.
  MicStopped { take: Take },
  /// The dictation ended, delivered or not.
  Finished { take: Take },
}

impl From<Edge> for Input {
  fn from(edge: Edge) -> Self {
    match edge {
      Edge::Down { at, repeat } => Input::KeyDown { at, repeat, which: 0 },
      Edge::Up { at } => Input::KeyUp { at, which: 0 },
      Edge::Escape => Input::Escape,
      Edge::Aborted => Input::KeyAborted { which: 0 },
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
  /// By hotkey `which` ([Input::KeyDown]).
  StartRecording(Take, usize),
  /// Model key `index`: the dictation goes to its model.
  PickModel(Take, usize),
  StopRecording(Take),
  CancelRecording(Take),
  /// Like a cancel, without a word: the press was no hotkey press.
  AbortRecording(Take),
  CancelTranscription(Take),
}

/// A pure reducer: hotkey edges and what came of earlier effects in, the effect to carry out out.
#[derive(Debug)]
struct Machine {
  activation: Activation,
  state: State,
  last_take: Take,
  /// The last press or repeat of a hotkey (which, when), if no release of it came after it.
  key_down: Option<(usize, Instant)>,
  /// The take the press in progress started, until its release.
  pressed: Option<Take>,
  /// The hotkey that started the recording in progress: only its release counts.
  which: usize,
}

impl Machine {
  fn new(activation: Activation) -> Self {
    Self { activation, state: State::Idle, last_take: 0, key_down: None, pressed: None, which: 0 }
  }

  fn handle(&mut self, input: Input) -> Option<Effect> {
    use State::{Idle, Recording, Transcribing};
    match (self.state, self.track_key(input)) {
      (_, Input::KeyDown { repeat: true, .. }) => None,
      (Idle, Input::KeyDown { at, which, .. }) => {
        let hold = match self.activation {
          Activation::Toggle => Hold::Latched,
          Activation::HoldOrToggle | Activation::PushToTalk => Hold::Held(at),
        };

        self.last_take += 1;
        (self.pressed, self.which) = (Some(self.last_take), which);
        self.state = Recording { take: self.last_take, hold };
        Some(Effect::StartRecording(self.last_take, which))
      }
      // A second press before the release: the release to come decides.
      (Recording { hold: Hold::Held(_) | Hold::Keyed, .. }, Input::KeyDown { .. }) => None,
      (Recording { take, hold: Hold::Latched }, Input::KeyDown { .. }) => self.stop(take),
      (Recording { take, hold: Hold::Held(since) }, Input::KeyUp { at, which })
        if which == self.which =>
      {
        let tap = at.saturating_duration_since(since) < HOLD_THRESHOLD;
        if self.activation == Activation::HoldOrToggle && tap {
          self.state = Recording { take, hold: Hold::Latched };
          return None;
        }

        self.stop(take)
      }
      (Recording { take, hold: Hold::Keyed }, Input::KeyUp { which, .. })
        if which == self.which =>
      {
        self.stop(take)
      }
      (_, Input::KeyDown { .. } | Input::KeyUp { .. }) => None,

      (Recording { take, hold: Hold::Held(_) | Hold::Keyed }, Input::ModelKey { index }) => {
        self.state = Recording { take, hold: Hold::Keyed };
        Some(Effect::PickModel(take, index))
      }
      // Toggle mode latches at the press: a key counts while that hotkey is still down.
      (Recording { take, hold: Hold::Latched }, Input::ModelKey { index })
        if self.pressed == Some(take) =>
      {
        Some(Effect::PickModel(take, index))
      }
      (_, Input::ModelKey { .. }) => None,

      (Idle, Input::Toggle) => {
        self.last_take += 1;
        (self.pressed, self.which) = (None, 0);
        self.state = Recording { take: self.last_take, hold: Hold::Latched };
        Some(Effect::StartRecording(self.last_take, 0))
      }
      (Recording { take, .. }, Input::Toggle) => self.stop(take),
      (Transcribing { .. }, Input::Toggle) => None,

      (Recording { take, .. }, Input::KeyAborted { which })
        if self.pressed == Some(take) && which == self.which =>
      {
        (self.state, self.pressed) = (Idle, None);
        Some(Effect::AbortRecording(take))
      }
      (_, Input::KeyAborted { which }) => {
        if which == self.which {
          self.pressed = None;
        }

        None
      }

      (Recording { take, .. }, Input::Escape) => {
        self.state = Idle;
        Some(Effect::CancelRecording(take))
      }
      (Transcribing { take }, Input::Escape) => {
        self.state = Idle;
        Some(Effect::CancelTranscription(take))
      }
      (Idle, Input::Escape) => None,

      (Recording { take, .. }, Input::RecordingFailed { take: t })
      | (Transcribing { take }, Input::Finished { take: t })
        if t == take =>
      {
        self.state = Idle;
        None
      }
      (Recording { take, .. }, Input::MicStopped { take: t }) if t == take => self.stop(take),
      // Late news of a take that is no longer current.
      (_, Input::RecordingFailed { .. } | Input::MicStopped { .. } | Input::Finished { .. }) => {
        None
      }
    }
  }

  /// Flags an unflagged autorepeat (see [`REPEAT_WINDOW`]).
  fn track_key(&mut self, input: Input) -> Input {
    match input {
      Input::KeyDown { at, repeat, which } => {
        let held = self
          .key_down
          .is_some_and(|(w, t)| w == which && at.saturating_duration_since(t) < REPEAT_WINDOW);
        self.key_down = Some((which, at));
        Input::KeyDown { at, repeat: repeat || held, which }
      }
      // Another hotkey's release leaves this one's press as it is.
      Input::KeyUp { which, .. } | Input::KeyAborted { which } => {
        self.key_down = self.key_down.filter(|&(w, _)| w != which);
        if which == self.which && matches!(input, Input::KeyUp { .. }) {
          self.pressed = None;
        }

        input
      }
      _ => input,
    }
  }

  fn stop(&mut self, take: Take) -> Option<Effect> {
    self.state = State::Transcribing { take };
    Some(Effect::StopRecording(take))
  }
}

// MARK: Output

/// Delivers dictated text to the app the person is typing in.
pub trait TextOutput: Send + 'static {
  /// Where the person is typing: the frontmost app and its focused window.
  type Target: Send + 'static;

  /// Taken when a recording starts.
  fn target(&mut self) -> Self::Target;

  /// Pastes `text` into `target`, unless the focus has moved since or pasting there is unsafe. The
  /// clipboard is restored afterwards unless `keep_in_clipboard`.
  fn paste(
    &mut self,
    text: &str,
    target: &Self::Target,
    keep_in_clipboard: bool,
  ) -> Result<(), NotPasted>;

  fn copy(&mut self, text: &str) -> Result<()>;
}

/// Why a text was not pasted. It is in the history either way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotPasted {
  #[error("the focus moved to another window during the dictation")]
  FocusChanged,
  #[error("the focused field is a password field")]
  SecureField,
  #[error("{0}")]
  Failed(String),
}

// MARK: Status

#[derive(Clone)]
pub struct Config {
  /// Settings, history and recordings.
  pub home: PathBuf,
  pub hub: Hub,
  pub loader: Arc<dyn Loader>,
  /// A WAV file played instead of the microphone (`WAVO_FAKE_MIC`).
  pub fake_mic: Option<PathBuf>,
}

impl Config {
  pub fn from_env() -> Self {
    Self {
      home: crate::home(),
      hub: Hub::new(),
      loader: Arc::new(Wavo),
      fake_mic: audio::fake_mic(),
    }
  }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
  pub state: Phase,
  pub recording: Option<RecordingStatus>,
  pub engine: EngineStatus,
  pub last: Option<Last>,
  /// The entry of the dictation in progress, recording or transcribing.
  pub current: Option<i64>,
  /// Entries being transcribed again.
  pub retranscribing: Vec<i64>,
  /// Entries transcribed again in this run, whose first text a restore puts back.
  pub restorable: Vec<i64>,
  /// The last problem that is no entry's: the microphone did not open, a failed retranscription…
  pub error: Option<String>,
  /// Settings problems, the model fallback, and the current or last dictation's.
  pub warnings: Vec<String>,
  /// Why no model can transcribe now: recordings are kept for later.
  pub no_model: Option<NoModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoModel {
  #[error("no model is downloaded")]
  NoneDownloaded,
  #[error("{model} is not on disk")]
  Gone { model: String },
  #[error("{model} could not be loaded: {error}")]
  LoadFailed { model: String, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
  #[default]
  Idle,
  Recording,
  Transcribing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordingStatus {
  pub device: String,
  pub seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineStatus {
  /// Model file name without extension, once an engine has one.
  pub model: Option<String>,
  pub phase: engine::Phase,
  /// Where the loaded model runs.
  pub device: Option<String>,
  /// Finished and planned pieces of the job in progress.
  pub progress: Option<(usize, usize)>,
}

/// How a dictation ended, shown once; the history keeps none of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
  Done,
  /// The engine failed (the entry is kept without text) or the recording did not close.
  #[default]
  Failed,
  /// Speech of which the engine made no text: a language the model does not know, most likely.
  NoText,
  /// Speech recorded while no model could transcribe it: kept without text.
  NoModel,
  /// No speech: the engine was not called, nothing is kept.
  NoSpeech,
  Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Last {
  pub take: Take,
  pub entry: Option<i64>,
  pub outcome: Outcome,
  pub text: String,
  pub error: Option<String>,
  pub pasted: bool,
  pub not_pasted: Option<String>,
  /// From the stop to the text, pasted or not.
  pub took_ms: Option<u64>,
}

/// What a platform shell shows, sent to the [`Observer`] after every change.
#[derive(Debug, Clone)]
pub struct Update {
  pub status: Status,
  pub settings: Settings,
  /// The level of the recording in progress.
  pub meter: Option<Meter>,
  /// Moves on every change of the history: a history window reloads then.
  pub history: u64,
}

/// Called on the session thread; it must return at once.
pub type Observer = Box<dyn FnMut(Update) + Send>;

// MARK: Session

/// Dropping it ends the session thread: a recording in progress is closed and kept (the next
/// start adopts it).
pub struct Session {
  handle: Handle,
  thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct Handle(Sender<Msg>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
  /// Transcribe an entry's recording again; new text replaces the text.
  Retranscribe(i64),
  /// Put back the text an entry had before it was transcribed again in this run.
  Restore(i64),
  Delete(i64),
  /// Free the model's memory, after the job in progress.
  Unload,
  /// An import added dictations: the update after it carries the history's change.
  Imported,
  /// The app quits for an update: no dictation runs, as the session stands after what was sent
  /// before it, and none starts from then on; an error while one runs, which ends as usual.
  Close,
}

enum Msg {
  Input(Input),
  Request(Request, Sender<Result<(), String>>),
  Edit(Box<dyn FnOnce(&mut Settings) + Send>),
  CheckModel,
  /// The model at this path is prepared for the Neural Engine now.
  Prepared(PathBuf),
  Quit,
}

impl Session {
  /// Loads the settings and the history in `config.home` and starts the session thread; the
  /// observer's first update comes once the history is recovered.
  pub fn start<O: TextOutput>(config: Config, output: O, observer: Observer) -> Result<Self> {
    fs::create_dir_all(&config.home)
      .with_context(|| format!("create {}", config.home.display()))?;
    let (settings, warning) = Settings::load(&config.home.join(settings::FILE));
    if let Some(w) = &warning {
      warn!("{w}");
    }

    let history = History::open(&config.home)?;
    let (tx, rx) = unbounded();
    let (events_tx, events) = unbounded();
    let controller = Controller {
      tx: tx.clone(),
      machine: Machine::new(settings.activation),
      settings,
      history,
      output,
      config,
      engines: Vec::new(),
      events_tx,
      events,
      next_job: 0,
      dictation: None,
      last: None,
      last_warnings: Vec::new(),
      retranscribing: Vec::new(),
      originals: BTreeMap::new(),
      untried: false,
      closed: false,
      on_disk: Vec::new(),
      model_path: None,
      model_info: None,
      model_error: None,
      model_fallback: None,
      error: None,
      settings_warning: warning,
      observer,
      published: None,
    };

    let spawn = thread::Builder::new().name("session".into()).spawn(move || controller.run(rx));
    let thread = spawn.context("spawn the session thread")?;
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
  pub fn input(&self, input: Input) {
    let _ = self.0.send(Msg::Input(input));
  }

  /// Edits apply in order to the latest settings, which are saved and used from then on.
  pub fn edit_settings(&self, edit: impl FnOnce(&mut Settings) + Send + 'static) {
    let _ = self.0.send(Msg::Edit(Box::new(edit)));
  }

  /// The models on disk changed: which one transcribes is looked at again.
  pub fn check_model(&self) {
    let _ = self.0.send(Msg::CheckModel);
  }

  pub fn request(&self, request: Request) -> Result<(), String> {
    let (reply, rx) = bounded(1);
    if self.0.send(Msg::Request(request, reply)).is_err() {
      return Err("the session has stopped".into());
    }

    rx.recv_timeout(REQUEST_TIMEOUT).unwrap_or_else(|_| Err("the session did not answer".into()))
  }
}

// MARK: Controller

/// The engine of a model.
struct Loaded {
  /// The model file's name without extension.
  name: String,
  /// wavo's, unless the model is a file of the person's own.
  info: Option<&'static ModelInfo>,
  path: PathBuf,
  engine: Engine,
}

/// A model on disk to transcribe with: the default one, or a key's.
struct OnDisk {
  /// The key's model choice; `None`: the default one.
  choice: Option<String>,
  path: PathBuf,
  info: Option<&'static ModelInfo>,
}

struct Job {
  id: u64,
  /// The model's file name without extension, and wavo's info on it.
  model: String,
  info: Option<&'static ModelInfo>,
  path: PathBuf,
  language: Option<String>,
  /// The one it runs on, which a model change may have replaced since.
  engine: Engine,
}

/// An entry's text before it was transcribed again.
struct Original {
  text: String,
  model: Option<String>,
  language: Option<String>,
}

struct Retranscription {
  job: Job,
  entry: i64,
  /// Started by itself, not asked for: its failure is no news.
  auto: bool,
}

/// When a recording stopped at `stopped` closes, its voice pausing for `pause` at `now`: once the
/// pause reaches [TAIL_PAUSE], but not before [POST_ROLL] nor after [TAIL_MAX] from the stop. A
/// word the person finishes after the release is kept; silence costs only the post-roll.
fn tail_end(stopped: Instant, now: Instant, pause: Duration) -> Instant {
  let quiet = now + TAIL_PAUSE.saturating_sub(pause);
  quiet.clamp(stopped + POST_ROLL, stopped + TAIL_MAX)
}

struct Dictation<T> {
  /// How it ends, as it goes.
  last: Last,
  target: Option<T>,
  /// Unix ms.
  created_at: i64,
  audio: String,
  started: Instant,
  /// The tail ([tail_end]) runs from then until the recorder closes.
  stopped: Option<Instant>,
  recorder: Option<Recorder>,
  /// The model its hotkey or a key picked (a choice, as [Settings::model]); `None`: the default
  /// one.
  model: Option<String>,
  /// The engine loading its model for it, by the model's path.
  prewarmed: Option<PathBuf>,
  /// The engines whose model its prewarms loaded: a key that picks another model drops them.
  cold: Vec<PathBuf>,
  job: Option<Job>,
  duration_ms: i64,
  warnings: Vec<String>,
}

struct Controller<O: TextOutput> {
  /// Its own inbox, for what background threads tell it.
  tx: Sender<Msg>,
  config: Config,
  machine: Machine,
  settings: Settings,
  history: History,
  output: O,
  /// An engine per model, by its file: the default one's, the keys', one an entry's own.
  engines: Vec<Loaded>,
  events_tx: Sender<engine::Event>,
  events: Receiver<engine::Event>,
  next_job: u64,
  dictation: Option<Dictation<O::Target>>,
  last: Option<Last>,
  last_warnings: Vec<String>,
  retranscribing: Vec<Retranscription>,
  originals: BTreeMap<i64, Original>,
  /// Entries without text may wait for the model now found.
  untried: bool,
  /// The app quits for an update ([Request::Close]): no dictation starts.
  closed: bool,
  /// The models on disk [`Controller::note_models`] last noted.
  on_disk: Vec<PathBuf>,
  /// The model [`Controller::check_model`] last found, and wavo's info on it.
  model_path: Option<PathBuf>,
  model_info: Option<&'static ModelInfo>,
  model_error: Option<NoModel>,
  /// The warning of a chosen model not on disk while another one transcribes.
  model_fallback: Option<String>,
  error: Option<String>,
  settings_warning: Option<String>,
  observer: Observer,
  /// The last update's status (without the recording's time), settings and history changes.
  published: Option<(Status, Settings, u64)>,
}

impl<O: TextOutput> Controller<O> {
  fn run(mut self, rx: Receiver<Msg>) {
    // What a crash left, before any recording makes new files; the models found now are no new
    // ones.
    let _ = self.check_model();
    self.note_models();
    self.untried = false;
    self.prepare();
    if let Err(e) = self.history.recover(now_ms()) {
      warn!("history recovery: {e:#}");
    }

    self.cleanup();
    self.publish();
    let events = self.events.clone();
    loop {
      let recorder = self.dictation.as_ref().and_then(|d| Some((d.recorder.as_ref()?, d.stopped)));
      let failures = recorder.map_or_else(never, |(r, _)| r.failures().clone());
      let tail = match recorder {
        Some((r, Some(stopped))) => at(tail_end(stopped, Instant::now(), r.meter().pause())),
        _ => never(),
      };

      select! {
        recv(rx) -> msg => match msg {
          Ok(Msg::Quit) | Err(_) => break,
          Ok(msg) => self.on_msg(msg),
        },
        recv(failures) -> failure => self.on_mic_failure(failure.unwrap_or_default()),
        recv(tail) -> _ => self.on_tail(),
        recv(events) -> event => self.on_engine(event.unwrap()),
      }

      self.transcribe_untried();
      self.publish();
    }

    // Quit in the middle of a transcription: the entry is not tried, the next start takes it.
    let dictation = self.dictation.as_ref().filter(|d| d.job.is_some()).and_then(|d| d.last.entry);
    for id in dictation.into_iter().chain(self.retranscribing.iter().map(|r| r.entry)) {
      log_history(self.history.db.mark_untried(id));
    }
  }

  fn publish(&mut self) {
    let status = self.status();
    let mut key = status.clone();
    if let Some(r) = key.recording.as_mut() {
      r.seconds = 0.0;
    }

    let key = (key, self.settings.clone(), self.history.db.changes());
    if self.published.as_ref() == Some(&key) {
      return;
    }

    let history = key.2;
    self.published = Some(key);
    let meter = self.dictation.as_ref().and_then(|d| d.recorder.as_ref()).map(Recorder::meter);
    (self.observer)(Update { status, settings: self.settings.clone(), meter, history });
  }

  fn on_msg(&mut self, msg: Msg) {
    match msg {
      Msg::Input(Input::KeyDown { .. } | Input::Toggle) if self.closed => {}
      Msg::Input(input) => {
        // A press is a new attempt: the last one's error goes.
        if matches!(input, Input::KeyDown { repeat: false, .. }) && self.error.take().is_some() {
          self.publish();
        }

        self.feed(input);
      }
      Msg::Request(request, reply) => {
        let _ = reply.send(self.on_request(request).map_err(|e| format!("{e:#}")));
      }
      Msg::Edit(edit) => {
        let mut settings = self.settings.clone();
        edit(&mut settings);
        self.apply_settings(settings);
      }
      Msg::CheckModel => {
        let _ = self.check_model();
        self.note_models();
        self.prune();
        self.prepare();
      }
      Msg::Prepared(path) => {
        // Loaded on the GPU meanwhile: the next job loads it on the Neural Engine, in ~0.1 s.
        let on_gpu = |l: &Loaded| {
          let device = l.engine.status().device.unwrap_or_default();
          l.path == path && !device.contains("Neural Engine")
        };

        if self.settings.neural_engine {
          self.engines.retain(|l| !on_gpu(l));
        }
      }
      Msg::Quit => unreachable!("handled by the loop"),
    }
  }

  fn on_request(&mut self, request: Request) -> Result<()> {
    match request {
      Request::Retranscribe(id) => {
        let entry =
          self.history.db.get_entry(id)?.ok_or_else(|| anyhow!("no history entry {id}"))?;
        let model = self.model_again(&entry);
        self.retranscribe(entry, model.as_deref(), false)
      }
      Request::Restore(id) => {
        if self.is_busy(id) {
          bail!("entry {id} is being transcribed");
        }

        let Some(o) = self.originals.get(&id) else { bail!("entry {id} has no text before") };
        let (model, language) = (o.model.as_deref(), o.language.as_deref());
        self.history.db.update_text(id, &o.text, model, language)?;
        // Kept until the text is back: a failed write can be tried again.
        self.originals.remove(&id);
        Ok(())
      }
      Request::Delete(id) => self.delete(id),
      Request::Unload => {
        info!("the models are unloaded on request");
        self.untried = false;
        for l in &self.engines {
          l.engine.unload();
        }

        Ok(())
      }
      Request::Imported => Ok(()),
      Request::Close => {
        if self.dictation.is_some() {
          bail!("a dictation runs");
        }

        info!("closed for the update: no dictation starts");
        self.closed = true;
        Ok(())
      }
    }
  }

  fn feed(&mut self, input: Input) {
    let Some(effect) = self.machine.handle(input) else { return };
    debug!(?input, ?effect, "session");
    match effect {
      Effect::StartRecording(take, which) => self.start_recording(take, which),
      Effect::PickModel(_, index) => self.pick(index),
      Effect::StopRecording(_) => self.stop_recording(),
      Effect::CancelRecording(_) => self.cancel(),
      Effect::AbortRecording(_) => {
        // The last dictation and its warnings stay the ones before.
        let (last, warnings) = (self.last.take(), mem::take(&mut self.last_warnings));
        self.cancel();
        (self.last, self.last_warnings) = (last, warnings);
      }
      Effect::CancelTranscription(_) => self.cancel(),
    }
  }

  fn apply_settings(&mut self, new: Settings) {
    if new == self.settings {
      return;
    }

    if let Err(e) = new.save(&self.config.home.join(settings::FILE)) {
      warn!("{e:#}");
      self.error = Some(format!("could not save the settings: {e:#}"));
    }

    let old = mem::replace(&mut self.settings, new);
    self.machine.activation = self.settings.activation;
    let new = &self.settings;
    let (model, keys) = (old.model != new.model, old.keys != new.keys);
    let ane = old.neural_engine != new.neural_engine;
    if ane || old.unload_after_minutes != new.unload_after_minutes {
      // Their threads finish their jobs, then drop the models.
      self.engines.clear();
    }

    if model {
      let _ = self.check_model();
    }

    if model || keys {
      self.note_models();
      self.prune();
    }

    if model || keys || ane {
      self.prepare();
    }

    if old.history_retention != self.settings.history_retention {
      self.cleanup();
    }
  }

  // MARK: Models

  /// The model to transcribe with, from the files on disk and the choice, without loading it, and
  /// wavo's info on it; or why there is none.
  fn check_model(&mut self) -> Result<(PathBuf, Option<&'static ModelInfo>), NoModel> {
    let chosen = self.settings.model.clone();
    if let Some(chosen) = &chosen
      && let Ok(path) = self.config.hub.resolve(chosen)
    {
      (self.model_error, self.model_fallback) = (None, None);
      return Ok(self.found_model(path, hub::lookup(chosen).ok()));
    }

    let error = match (self.config.hub.downloaded().into_iter().next(), chosen) {
      (Some((info, path)), chosen) => {
        let note = chosen
          .map(|c| format!("{} is not on disk: {} transcribes instead", label(&c), info.full_name));
        (self.model_error, self.model_fallback) = (None, note);
        return Ok(self.found_model(path, Some(info)));
      }
      (None, Some(chosen)) => NoModel::Gone { model: label(&chosen) },
      (None, None) => NoModel::NoneDownloaded,
    };

    (self.model_path, self.model_info, self.model_fallback) = (None, None, None);
    self.model_error = Some(error.clone());
    Err(error)
  }

  /// Another model than before may transcribe the entries without text; the engine of the one
  /// before goes, unless a key has it.
  fn found_model(
    &mut self,
    path: PathBuf,
    info: Option<&'static ModelInfo>,
  ) -> (PathBuf, Option<&'static ModelInfo>) {
    self.model_info = info;
    if self.model_path.as_ref() != Some(&path) {
      self.untried = true;
      self.model_path = Some(path.clone());
      self.prune();
    }

    (path, info)
  }

  /// The models on disk to transcribe with: the one found, then those of the keys, each file
  /// once.
  fn models(&self) -> Vec<OnDisk> {
    let mut models = Vec::new();
    if let Some(path) = &self.model_path {
      models.push(OnDisk { choice: None, path: path.clone(), info: self.model_info });
    }

    for k in self.settings.model_keys() {
      let Ok(path) = self.config.hub.resolve(&k.model) else { continue };
      if !models.iter().any(|m: &OnDisk| m.path == path) {
        let info = hub::lookup(&k.model).ok();
        models.push(OnDisk { choice: Some(k.model.clone()), path, info });
      }
    }

    models
  }

  /// Notes the models on disk: one new to the session may transcribe the entries without text.
  fn note_models(&mut self) {
    let paths: Vec<PathBuf> = self.models().into_iter().map(|m| m.path).collect();
    if paths.iter().any(|p| !self.on_disk.contains(p)) {
      self.untried = true;
    }

    self.on_disk = paths;
  }

  /// The engines of models no longer found nor a key's go: their threads finish their jobs first.
  fn prune(&mut self) {
    let paths: Vec<PathBuf> = self.models().into_iter().map(|m| m.path).collect();
    self.engines.retain(|l| paths.contains(&l.path));
  }

  /// The models found, the default one first, then the keys', prepared for the Neural Engine one
  /// at a time ahead of their first dictation, as the app starts or another model is found: a new
  /// app binary compiles them again once.
  fn prepare(&self) {
    if !self.settings.neural_engine {
      return;
    }

    for m in self.models() {
      let (tx, prepared) = (self.tx.clone(), m.path.clone());
      self.config.loader.prepare(&m.path, Box::new(move || _ = tx.send(Msg::Prepared(prepared))));
    }
  }

  /// None on disk, or the one found does not load.
  fn no_model(&self) -> Option<NoModel> {
    self.model_error.clone().or_else(|| {
      let loaded = self.loaded(self.model_path.as_deref()?)?;
      let error = loaded.engine.status().load_error?;
      Some(NoModel::LoadFailed { model: loaded_label(loaded.info, &loaded.name), error })
    })
  }

  fn loaded(&self, path: &Path) -> Option<&Loaded> {
    self.engines.iter().find(|l| l.path == path)
  }

  /// The model of `choice` (a key's, as [Settings::model]; `None`: the one found): a key's gets no
  /// stand-in.
  fn model_for(
    &mut self,
    choice: Option<&str>,
  ) -> Result<(PathBuf, Option<&'static ModelInfo>), NoModel> {
    let Some(choice) = choice else { return self.check_model() };
    match self.config.hub.resolve(choice) {
      Ok(path) => Ok((path, hub::lookup(choice).ok())),
      Err(_) => Err(NoModel::Gone { model: label(choice) }),
    }
  }

  /// The engine of `choice`'s model ([model_for](Self::model_for)), started if it has none. With
  /// an unload right after each dictation, the other engines but the dictation's unload: one model
  /// in memory, two while a Transcribe Again runs beside a dictation.
  fn engine_for(&mut self, choice: Option<&str>) -> Result<&Loaded, NoModel> {
    let (path, info) = self.model_for(choice)?;
    let at = match self.engines.iter().position(|l| l.path == path) {
      Some(at) => at,
      None => {
        let name = file_stem(&path);
        info!("model {name} ({})", path.display());
        let (loader, ane) = (self.config.loader.clone(), self.settings.neural_engine);
        let (unload_after, events) = (self.settings.unload_after(), self.events_tx.clone());
        let engine = Engine::start(loader, path.clone(), ane, unload_after, events);
        self.engines.push(Loaded { name, info, path, engine });
        self.engines.len() - 1
      }
    };

    if self.settings.unload_after() == Some(Duration::ZERO) {
      let current = self.dictation_path();
      for (i, l) in self.engines.iter().enumerate() {
        if i != at && Some(l.path.as_path()) != current {
          l.engine.unload();
        }
      }
    }

    Ok(&self.engines[at])
  }

  /// A job on the engine of `choice`'s model ([engine_for](Self::engine_for)), without its
  /// language yet.
  fn new_job(&mut self, choice: Option<&str>) -> Result<Job, NoModel> {
    let loaded = self.engine_for(choice)?;
    let (model, info, path) = (loaded.name.clone(), loaded.info, loaded.path.clone());
    let engine = loaded.engine.clone();
    self.next_job += 1;
    Ok(Job { id: self.next_job, model, info, path, language: None, engine })
  }

  fn on_engine(&mut self, event: engine::Event) {
    let engine::Event::Done(id, result) = event else { return };
    if self.dictation.as_ref().and_then(|d| d.job.as_ref()).is_some_and(|j| j.id == id) {
      return self.on_transcribed(result);
    }

    if let Some(at) = self.retranscribing.iter().position(|r| r.job.id == id) {
      let r = self.retranscribing.remove(at);
      self.on_retranscribed(r, result);
    }
  }

  /// The outcome, text and error of `job`'s result.
  fn outcome(
    &self,
    result: Result<String, EngineError>,
    job: &Job,
  ) -> (Outcome, String, Option<String>) {
    match result {
      Ok(text) if text.trim().is_empty() => {
        (Outcome::NoText, String::new(), job.info.and_then(understands_only))
      }
      Ok(text) => (Outcome::Done, text.trim().to_owned(), None),
      Err(EngineError::Load(error)) => {
        let model = loaded_label(job.info, &job.model);
        (Outcome::NoModel, String::new(), Some(NoModel::LoadFailed { model, error }.to_string()))
      }
      Err(e) => (Outcome::Failed, String::new(), Some(e.to_string())),
    }
  }

  // MARK: Dictation

  /// The microphone first, and the shell shows the recording at once; then the focus check (up to
  /// a second) and the engine loads the model while the person speaks: that of hotkey `which`.
  fn start_recording(&mut self, take: Take, which: usize) {
    let model = self.settings.hotkey_model(which).map(str::to_owned);
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
        // Its day folder goes if nothing else is in it.
        let _ = self.history.recordings.remove(&audio);
        return self.feed(Input::RecordingFailed { take });
      }
    };

    info!("dictation {take}: recording from {:?}", recorder.device());
    self.dictation = Some(Dictation {
      last: Last { take, ..Last::default() },
      target: None,
      created_at,
      audio,
      started,
      stopped: None,
      warnings: recorder.warning().map(str::to_owned).into_iter().collect(),
      recorder: Some(recorder),
      model,
      prewarmed: None,
      cold: Vec::new(),
      job: None,
      duration_ms: 0,
    });
    self.publish();

    let target = self.output.target();
    debug!("dictation {take}: focus noted after {:.0?}", started.elapsed());
    self.dictation.as_mut().unwrap().target = Some(target);
    self.prewarm();
  }

  /// The engine of the dictation's model loads it while the person speaks. One that loaded another
  /// model for it before a key picked this one lets go, and drops a model the dictation loaded.
  fn prewarm(&mut self) {
    let d = self.dictation.as_ref().unwrap();
    let (take, choice) = (d.last.take, d.model.clone());
    if let Some(choice) = &choice {
      info!("dictation {take}: with {}", label(choice));
    }

    let prewarm = match self.engine_for(choice.as_deref()) {
      Ok(loaded) => {
        // Its engine has no model: the load is the dictation's own.
        let cold = loaded.engine.status().phase == engine::Phase::Stopped;
        loaded.engine.prewarm();
        Some((loaded.path.clone(), cold))
      }
      Err(e) => {
        info!("dictation {take}: recording without a model: {e}");
        None
      }
    };

    let d = self.dictation.as_mut().unwrap();
    if let Some((path, true)) = &prewarm
      && !d.cold.contains(path)
    {
      d.cold.push(path.clone());
    }

    let path = prewarm.map(|(path, _)| path);
    let before = mem::replace(&mut d.prewarmed, path.clone());
    let Some(before) = before.filter(|b| path.as_ref() != Some(b)) else { return };
    let undo = d.cold.contains(&before);
    self.let_go(&before, undo);
  }

  /// A dictation's engine at `path` may unload its model when idle again; with `undo` the model
  /// goes at once.
  fn let_go(&self, path: &Path, undo: bool) {
    let Some(l) = self.loaded(path) else { return };
    l.engine.release();
    if undo {
      l.engine.unload();
    }
  }

  /// Model key `index` while the hotkey is held: the dictation goes to the key's model, which its
  /// engine loads meanwhile.
  fn pick(&mut self, index: usize) {
    let Some(choice) = self.settings.held_key_model(index).map(str::to_owned) else {
      return debug!("no model key {index}");
    };

    let Some(d) = self.dictation.as_mut() else { return };
    if d.model.as_ref() != Some(&choice) {
      d.model = Some(choice);
      self.prewarm();
    }
  }

  fn stop_recording(&mut self) {
    let d = self.dictation.as_mut().unwrap();
    d.stopped = Some(Instant::now());
    if d.recorder.as_ref().is_some_and(Recorder::failed) {
      self.transcribe();
    }
  }

  /// The tail's timer: the recording ends once [tail_end] is due, else the timer is set again.
  fn on_tail(&mut self) {
    let d = self.dictation.as_ref().unwrap();
    let (Some(recorder), Some(stopped)) = (&d.recorder, d.stopped) else { return };
    let (now, pause) = (Instant::now(), recorder.meter().pause());
    if tail_end(stopped, now, pause) <= now {
      // Without the pause the cap ended it.
      let capped = if pause < TAIL_PAUSE { ", at its cap" } else { "" };
      let tail = (now - stopped).as_millis();
      info!("dictation {}: the tail ran {tail} ms after the stop{capped}", d.last.take);
      self.transcribe();
    }
  }

  fn on_mic_failure(&mut self, failure: String) {
    let d = self.dictation.as_ref().unwrap();
    let take = d.last.take;
    warn!("dictation {take}: the recording stopped: {failure}");
    match d.stopped {
      Some(_) => self.transcribe(),
      None => self.feed(Input::MicStopped { take }),
    }
  }

  /// Closes the microphone and hands the recording to the engine: with speech, its entry first.
  fn transcribe(&mut self) {
    let d = self.dictation.as_mut().unwrap();
    let take = d.last.take;
    let recorder = d.recorder.take().unwrap();
    let rec = match recorder.stop() {
      Ok(rec) => rec,
      Err(e) => {
        self.discard();
        return self.end(Outcome::Failed, Some(format!("the recording failed: {e:#}")));
      }
    };

    for w in rec.warnings {
      if !d.warnings.contains(&w) {
        warn!("dictation {take}: {w}");
        d.warnings.push(w);
      }
    }

    d.duration_ms = rec.clip.duration_ms();
    let choice = d.model.clone();
    let Some(input) = rec.clip.engine_input() else {
      self.discard();
      return self.end(Outcome::NoSpeech, None);
    };

    if !rec.saved {
      // Transcribed and pasted all the same, but no entry keeps a broken file.
      warn!("dictation {take}: its recording could not be saved: no entry");
      self.discard();
    }

    let job = self.new_job(choice.as_deref());
    // A model not on disk still has a language: that of wavo's model of the name.
    let info = match &job {
      Ok(job) => job.info,
      Err(_) => {
        choice.as_deref().or(self.settings.model.as_deref()).and_then(|c| hub::lookup(c).ok())
      }
    };

    let language = language_for(info, &self.settings);
    let model = job.as_ref().ok().map(|j| j.model.as_str());
    let entry = if rec.saved { self.add_entry(model, language.as_deref()) } else { None };
    self.dictation.as_mut().unwrap().last.entry = entry;
    let mut job = match job {
      Ok(job) => job,
      Err(e) => return self.end(Outcome::NoModel, Some(e.to_string())),
    };

    job.language = language.clone();
    job.engine.transcribe(job.id, input.pcm, language, false);
    self.dictation.as_mut().unwrap().job = Some(job);
  }

  /// The entry of the dictation, in its language: without text, busy if a model transcribes it.
  fn add_entry(&self, model: Option<&str>, language: Option<&str>) -> Option<i64> {
    let d = self.dictation.as_ref().unwrap();
    let entry = Entry {
      created_at: d.created_at,
      duration_ms: d.duration_ms,
      audio_path: Some(d.audio.clone()),
      language: language.map(str::to_owned),
      tried_model: model.map(str::to_owned),
      busy: model.is_some(),
      ..Entry::default()
    };

    // The text still reaches the person; the recording becomes an entry on the next start.
    let ids = self.history.db.insert(&[entry]).inspect_err(|e| warn!("history: {e:#}"));
    ids.ok().map(|ids| ids[0])
  }

  fn on_transcribed(&mut self, result: Result<String, EngineError>) {
    let job = self.dictation.as_mut().unwrap().job.take().unwrap();
    let (outcome, text, error) = self.outcome(result, &job);
    let d = self.dictation.as_mut().unwrap();
    match (outcome, d.last.entry) {
      (Outcome::Done, Some(id)) => {
        let language = job.language.as_deref();
        log_history(self.history.db.update_text(id, &text, Some(&job.model), language));
      }
      // The model did not load: it tried nothing, and takes the entry by itself once it does.
      (Outcome::NoModel, Some(id)) => {
        log_history(self.history.db.mark_untried(id));
        self.untried = true;
      }
      (_, Some(id)) => log_history(self.history.db.mark_idle(id)),
      (_, None) => {}
    }

    if let Some(e) = &error {
      warn!("dictation {}: {outcome:?}: {e}", d.last.take);
    }

    (d.last.outcome, d.last.text, d.last.error) = (outcome, text, error);
    if outcome == Outcome::Done {
      self.deliver();
    }

    let take = self.dictation.as_ref().unwrap().last.take;
    self.feed(Input::Finished { take });
    self.complete();
  }

  fn deliver(&mut self) {
    let d = self.dictation.as_mut().unwrap();
    let auto_paste = self.settings.auto_paste;
    let refused = match (&d.target, auto_paste) {
      (Some(target), true) => {
        let keep = self.settings.keep_in_clipboard;
        self.output.paste(&d.last.text, target, keep).err().map(|e| e.to_string())
      }
      (None, true) => Some("the app in focus when the dictation started is unknown".into()),
      // Without auto-paste the clipboard is how the text gets out.
      (_, false) => Some(match self.output.copy(&d.last.text) {
        Ok(()) => "auto-paste is off; the text is on the clipboard".to_owned(),
        Err(e) => format!("auto-paste is off, and copying failed: {e:#}"),
      }),
    };

    if let Some(why) = &refused {
      info!("dictation {}: not pasted: {why}", d.last.take);
    }

    (d.last.pasted, d.last.not_pasted) = (refused.is_none(), refused);
    if let Some(id) = d.last.entry {
      // Without auto-paste nothing was refused: the history keeps no reason.
      let why = d.last.not_pasted.as_deref().filter(|_| auto_paste);
      log_history(self.history.db.mark_pasted(id, d.last.pasted, why));
    }
  }

  /// Ends the dictation before it reached the engine.
  fn end(&mut self, outcome: Outcome, error: Option<String>) {
    let d = self.dictation.as_mut().unwrap();
    if let Some(e) = &error {
      warn!("dictation {}: {e}", d.last.take);
    }

    (d.last.outcome, d.last.error) = (outcome, error);
    let take = d.last.take;
    self.feed(Input::Finished { take });
    self.complete();
  }

  /// The person cancelled, while recording or transcribing: nothing of the dictation is kept.
  fn cancel(&mut self) {
    let d = self.dictation.as_mut().unwrap();
    if let Some(recorder) = d.recorder.take() {
      let _ = recorder.stop();
    }

    if let Some(job) = d.job.take() {
      job.engine.cancel(job.id);
    }

    d.last.outcome = Outcome::Cancelled;
    self.discard();
    self.complete();
  }

  /// Nothing of the dictation is kept: its entry, if it has one, and its recording go.
  fn discard(&mut self) {
    let d = self.dictation.as_mut().unwrap();
    if let Err(e) = self.history.recordings.remove(&d.audio) {
      warn!("dictation {}: {e:#}", d.last.take);
    }

    if let Some(id) = d.last.entry.take() {
      log_history(self.history.db.delete_entries(&[id]));
    }
  }

  /// The dictation becomes the last one, and the retention runs.
  fn complete(&mut self) {
    let mut d = self.dictation.take().unwrap();
    d.last.took_ms = d.stopped.map(|s| s.elapsed().as_millis() as u64);
    if let Some(path) = &d.prewarmed {
      self.let_go(path, false);
    }

    let after_stop = d.stopped.map(|s| format!(", {} ms after the stop", s.elapsed().as_millis()));
    let seconds = d.duration_ms as f64 / 1000.0;
    let (take, outcome) = (d.last.take, d.last.outcome);
    info!("dictation {take}: {outcome:?} ({seconds:.1} s{})", after_stop.unwrap_or_default());
    (self.last, self.last_warnings) = (Some(d.last), d.warnings);
    self.cleanup();
  }

  // MARK: History

  fn is_busy(&self, id: i64) -> bool {
    let current = self.dictation.as_ref().is_some_and(|d| d.last.entry == Some(id));
    current || self.retranscribing.iter().any(|r| r.entry == id)
  }

  /// `entry`'s recording through the engine of `model` again (a choice, as [Settings::model];
  /// `None`: the one found), behind the dictations on that engine: in its own language when it
  /// goes by itself or has no text, else as a dictation now would be. One without text whose
  /// recording has no speech (or is gone) is deleted: there is nothing to keep.
  fn retranscribe(&mut self, entry: Entry, model: Option<&str>, auto: bool) -> Result<()> {
    let id = entry.id;
    if entry.busy || self.is_busy(id) {
      bail!("entry {id} is being transcribed");
    }

    let clip = match self.history.audio_file(&entry) {
      Some(wav) => audio::read_wav(&wav).map(Clip::analyze),
      None => Err(anyhow!("the recording of entry {id} is gone")),
    };

    let Some(input) = clip.as_ref().ok().and_then(Clip::engine_input) else {
      let why = clip.map_or_else(|e| format!("{e:#}"), |_| "no speech in its recording".into());
      if !entry.text.is_empty() {
        bail!("entry {id}: {why}; it stays as it is");
      }

      info!("entry {id}: {why}; it goes");
      return self.delete(id);
    };

    let mut job = self.new_job(model)?;
    job.language = if auto || entry.text.is_empty() {
      entry.language
    } else {
      language_for(job.info, &self.settings)
    };

    // Marked tried before it runs: a crash or a failure is not tried again by the same model.
    self.history.db.mark_busy(id, &job.model)?;
    info!("entry {id}: transcribing {:.1} s again with {}", input.seconds(), job.model);
    job.engine.transcribe(job.id, input.pcm, job.language.clone(), true);
    self.retranscribing.push(Retranscription { job, entry: id, auto });
    Ok(())
  }

  /// The model Transcribe Again takes (a choice; `None`: the one found): the one `entry`'s text
  /// came from if that is the one found or a key's; for an entry without text the one of its
  /// language ([route]); else the one found.
  fn model_again(&self, entry: &Entry) -> Option<String> {
    let models = self.models();
    let name = entry.model.as_deref();
    let own = name.and_then(|name| models.iter().find(|m| file_stem(&m.path) == name));
    let routed = || route(&models, entry.language.as_deref()).filter(|_| entry.text.is_empty());
    own.or_else(routed).and_then(|m| m.choice.clone())
  }

  /// One entry without text at a time, when no dictation runs, by the model its language goes to
  /// ([route]; a detected one no model of more than one takes, to the one found) unless that one
  /// tried it; while a model does not load its entries wait, for the next dictation that loads it.
  fn transcribe_untried(&mut self) {
    while self.untried && self.dictation.is_none() && self.retranscribing.is_empty() {
      let (found, waiting) = self.untried_entry();
      let Some((entry, model)) = found else {
        self.untried = waiting;
        return;
      };

      let id = entry.id;
      if let Err(e) = self.retranscribe(entry, model.as_deref(), true) {
        warn!("entry {id}: {e:#}");
        self.untried = false;
      }
    }
  }

  /// The newest entry without text whose model ([route]) has not tried it, and that model's
  /// choice; and whether one waits for a model that does not load. One model per entry: two don't
  /// take turns at an entry that gives no text.
  fn untried_entry(&self) -> (Option<(Entry, Option<String>)>, bool) {
    let models = self.models();
    let (mut passed, mut to) = (false, None);
    let found = self.history.db.get_untried(|language, tried| {
      let Some(m) = route(&models, language) else { return false };
      if tried == Some(file_stem(&m.path).as_str()) {
        return false;
      }

      if self.loaded(&m.path).is_some_and(|l| l.engine.status().load_error.is_some()) {
        passed = true;
        return false;
      }

      to = Some(m);
      true
    });

    let entry = found.inspect_err(|e| warn!("history: {e:#}")).ok().flatten();
    (entry.zip(to).map(|(entry, m)| (entry, m.choice.clone())), passed)
  }

  fn on_retranscribed(&mut self, r: Retranscription, result: Result<String, EngineError>) {
    let (outcome, text, error) = self.outcome(result, &r.job);
    info!("entry {}: transcribed again: {outcome:?}", r.entry);
    match outcome {
      Outcome::Done => {
        // The text it had first, for a restore while the app runs.
        if let Ok(Some(e)) = self.history.db.get_entry(r.entry)
          && !e.text.is_empty()
        {
          let original = Original { text: e.text, model: e.model, language: e.language };
          self.originals.entry(r.entry).or_insert(original);
        }

        let language = r.job.language.as_deref();
        let update = self.history.db.update_text(r.entry, &text, Some(&r.job.model), language);
        log_history(update);
      }
      // Tried again by itself once the model loads (see transcribe_untried).
      Outcome::NoModel if r.auto => log_history(self.history.db.mark_untried(r.entry)),
      _ => log_history(self.history.db.mark_idle(r.entry)),
    }

    if r.auto {
      self.cleanup();
    } else if outcome != Outcome::Done {
      let why = match (outcome, error) {
        (Outcome::NoText, None) => "nothing recognized".into(),
        (Outcome::NoText, Some(note)) => format!("nothing recognized: {note}"),
        (_, error) => error.unwrap_or_default(),
      };

      self.error = Some(format!("entry {} could not be transcribed again: {why}", r.entry));
    }
  }

  /// Deletes entry `id` with its recording; the last dictation's text goes with it.
  fn delete(&mut self, id: i64) -> Result<()> {
    if self.is_busy(id) {
      bail!("entry {id} is being transcribed");
    }

    if !self.history.delete(id)? {
      bail!("no history entry {id}");
    }

    info!("entry {id}: deleted with its recording");
    self.originals.remove(&id);
    if self.last.as_ref().is_some_and(|l| l.entry == Some(id)) {
      self.last = None;
    }

    Ok(())
  }

  /// The retention spares entries without text while no model can transcribe them, or while they
  /// are being transcribed by themselves.
  fn cleanup(&mut self) {
    // Also those waiting for a model on disk (a start, an unload in the middle of them) or for one
    // that does not load; the history keeps those no model has tried anyway.
    let (waiting, passed) = self.untried_entry();
    let spare = self.no_model().is_some() || self.untried || waiting.is_some() || passed;
    match self.history.cleanup(self.settings.history_retention, now_ms(), spare) {
      Ok(0) => {}
      Ok(n) => info!("history: {n} old entries removed"),
      Err(e) => warn!("history cleanup: {e:#}"),
    }
  }

  fn status(&self) -> Status {
    let d = self.dictation.as_ref();
    let mut warnings: Vec<String> =
      self.settings_warning.iter().chain(&self.model_fallback).cloned().collect();
    warnings.extend(d.map_or(&self.last_warnings, |d| &d.warnings).iter().cloned());
    let engine = self.shown_engine().map_or_else(EngineStatus::default, |l| {
      let s = l.engine.status();
      let model = Some(l.name.clone());
      EngineStatus { model, phase: s.phase, device: s.device, progress: s.progress }
    });

    Status {
      state: match self.machine.state {
        State::Idle => Phase::Idle,
        State::Recording { .. } => Phase::Recording,
        State::Transcribing { .. } => Phase::Transcribing,
      },
      recording: d.and_then(|d| {
        let device = d.recorder.as_ref()?.device().to_owned();
        Some(RecordingStatus { device, seconds: d.started.elapsed().as_secs_f64() })
      }),
      engine,
      last: self.last.clone(),
      current: d.and_then(|d| d.last.entry),
      retranscribing: self.retranscribing.iter().map(|r| r.entry).collect(),
      restorable: self.originals.keys().copied().collect(),
      error: self.error.clone(),
      warnings,
      no_model: self.no_model(),
    }
  }

  /// The engine of the dictation in progress: its job's, else its prewarm's.
  fn dictation_path(&self) -> Option<&Path> {
    let d = self.dictation.as_ref()?;
    let job = d.job.as_ref().map(|j| j.path.as_path());
    job.or(d.prewarmed.as_deref())
  }

  /// The engine the status shows: the dictation's, else one at work, else one with its model in
  /// memory, else the one found's.
  fn shown_engine(&self) -> Option<&Loaded> {
    if let Some(l) = self.dictation_path().and_then(|path| self.loaded(path)) {
      return Some(l);
    }

    let working = |l: &&Loaded| {
      matches!(l.engine.status().phase, engine::Phase::Running | engine::Phase::Loading)
    };

    let in_memory = |l: &&Loaded| l.engine.status().phase != engine::Phase::Stopped;
    (self.engines.iter().find(working))
      .or_else(|| self.engines.iter().find(in_memory))
      .or_else(|| self.loaded(self.model_path.as_deref()?))
  }
}

fn log_history(result: Result<()>) {
  if let Err(e) = result {
    warn!("history: {e:#}");
  }
}

/// A model file's name without extension, as entries keep it: `gigaam-v3-e2e-rnnt-Q8_0`.
fn file_stem(path: &Path) -> String {
  path.file_stem().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into())
}

/// What a model choice (wavo's name, or a file's path) is called, also once its file is gone:
/// wavo's full name (`parakeet-tdt-0.6b-v3`), else the file's name without `.gguf`.
pub fn label(choice: &str) -> String {
  if let Ok(info) = hub::lookup(choice) {
    return info.full_name.into();
  }

  let file = Path::new(choice).file_name().and_then(|f| f.to_str()).unwrap_or(choice);
  file.strip_suffix(".gguf").unwrap_or(file).to_owned()
}

/// The model on disk an entry without text goes to: the first that takes its `language`, wavo's
/// models before files of the person's own, which take any; one without a language (detected) to
/// the first of more than one or a file of the person's own, else to the one found.
fn route<'a>(models: &'a [OnDisk], language: Option<&str>) -> Option<&'a OnDisk> {
  let takes = |m: &&OnDisk| match (m.info, language) {
    (None, _) => true,
    (Some(info), Some(code)) => info.langs.iter().any(|l| l.eq_ignore_ascii_case(code)),
    (Some(info), None) => info.langs.len() > 1,
  };

  let wavo = models.iter().filter(|m| m.info.is_some());
  let own = models.iter().filter(|m| m.info.is_none());
  let found = || models.iter().find(|m| m.choice.is_none()).filter(|_| language.is_none());
  wavo.chain(own).find(takes).or_else(found)
}

/// The language a model is asked in: its own for a model of one language, else the setting's
/// (`None`: the model detects it).
fn language_for(info: Option<&ModelInfo>, settings: &Settings) -> Option<String> {
  match info.map(|m| m.langs) {
    Some([code]) => Some((*code).to_owned()),
    _ => settings.engine_language(),
  }
}

fn loaded_label(info: Option<&ModelInfo>, name: &str) -> String {
  info.map_or_else(|| name.to_owned(), |m| m.full_name.to_owned())
}

/// `parakeet-tdt-0.6b-v2 understands English only`, for a model meant for one language.
fn understands_only(info: &ModelInfo) -> Option<String> {
  let [code] = info.langs else { return None };
  Some(format!("{} understands {} only", info.full_name, language_name(code)))
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use super::*;
  use crate::audio::SAMPLE_RATE;
  use crate::history::{RECORDINGS_DIR, Retention};
  use crate::settings::ModelKey;
  use crate::testing::{FakeModels, TempDir, download, poll, tone, wav_count, write_wav};

  // MARK: Machine

  struct Drive {
    m: Machine,
    t0: Instant,
  }

  impl Drive {
    fn new(activation: Activation) -> Self {
      Self { m: Machine::new(activation), t0: Instant::now() }
    }

    /// The main hotkey's press.
    fn down(&mut self, ms: u64) -> Option<Effect> {
      self.press(ms, 0)
    }

    fn press(&mut self, ms: u64, which: usize) -> Option<Effect> {
      let at = self.t0 + Duration::from_millis(ms);
      self.m.handle(Input::KeyDown { at, repeat: false, which })
    }

    fn repeat(&mut self, ms: u64) -> Option<Effect> {
      let at = self.t0 + Duration::from_millis(ms);
      self.m.handle(Input::KeyDown { at, repeat: true, which: 0 })
    }

    /// The main hotkey's release.
    fn up(&mut self, ms: u64) -> Option<Effect> {
      self.release(ms, 0)
    }

    fn release(&mut self, ms: u64, which: usize) -> Option<Effect> {
      self.m.handle(Input::KeyUp { at: self.t0 + Duration::from_millis(ms), which })
    }

    fn key(&mut self, index: usize) -> Option<Effect> {
      self.m.handle(Input::ModelKey { index })
    }

    fn latched(&self) -> bool {
      matches!(self.m.state, State::Recording { hold: Hold::Latched, .. })
    }

    fn finish(&mut self, take: Take) {
      assert_eq!(self.m.handle(Input::Finished { take }), None);
      assert_eq!(self.m.state, State::Idle);
    }
  }

  use Effect::{
    AbortRecording, CancelRecording, CancelTranscription, PickModel, StartRecording, StopRecording,
  };

  #[test]
  fn hold_or_toggle_holds_or_latches_on_a_tap() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.down(0), Some(StartRecording(1, 0)));
    assert_eq!(d.up(300), Some(StopRecording(1)), "a hold of the threshold");
    d.finish(1);
    assert_eq!(d.down(1_000), Some(StartRecording(2, 0)));
    assert_eq!(d.up(1_299), None, "a tap latches");
    assert!(d.latched());
    assert_eq!(d.down(4_000), Some(StopRecording(2)), "the next press stops");
    assert_eq!(d.up(6_000), None);
  }

  #[test]
  fn push_to_talk_and_toggle() {
    let mut d = Drive::new(Activation::PushToTalk);
    assert_eq!(d.down(0), Some(StartRecording(1, 0)));
    assert_eq!(d.up(40), Some(StopRecording(1)), "however short");

    let mut d = Drive::new(Activation::Toggle);
    assert_eq!(d.down(0), Some(StartRecording(1, 0)));
    assert_eq!(d.up(2_000), None, "a long hold changes nothing");
    assert_eq!(d.down(5_000), Some(StopRecording(1)));
    assert_eq!(d.down(5_100), None, "transcribing ignores the hotkey");
  }

  /// Whatever the activation, a toggle starts and the next one stops; the hotkey stops it too.
  #[test]
  fn a_toggle_latches_in_every_activation() {
    for activation in [Activation::HoldOrToggle, Activation::PushToTalk, Activation::Toggle] {
      let mut d = Drive::new(activation);
      assert_eq!(d.m.handle(Input::Toggle), Some(StartRecording(1, 0)), "{activation:?}");
      assert!(d.latched());
      assert_eq!(d.m.handle(Input::Toggle), Some(StopRecording(1)));
      assert_eq!(d.m.handle(Input::Toggle), None, "transcribing ignores it");
    }

    let mut d = Drive::new(Activation::HoldOrToggle);
    d.m.handle(Input::Toggle);
    assert_eq!(d.down(1_000), Some(StopRecording(1)));
  }

  #[test]
  fn autorepeat_flagged_or_not_is_ignored() {
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    assert_eq!(d.repeat(600), None);
    for ms in (700..4_000).step_by(33) {
      assert_eq!(d.down(ms), None, "unflagged repeat at {ms} ms");
    }

    assert!(d.latched());
    d.up(4_000);
    assert_eq!(d.down(4_200), Some(StopRecording(1)), "a press after a release counts");

    // A release that never came costs at most the window.
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    assert_eq!(d.down(1_000), None);
    let later = REPEAT_WINDOW.as_millis() as u64 + 1_000;
    assert_eq!(d.down(later), Some(StopRecording(1)));
  }

  #[test]
  fn escape_cancels_a_recording_or_its_transcription_and_late_news_is_ignored() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.m.handle(Input::Escape), None);
    d.down(0);
    assert_eq!(d.m.handle(Input::Escape), Some(CancelRecording(1)));
    assert_eq!(d.up(500), None, "the release after a cancel");
    d.down(1_000);
    d.up(2_000);
    assert_eq!(d.m.handle(Input::Escape), Some(CancelTranscription(2)));
    d.down(3_000);
    for late in [Input::Finished { take: 2 }, Input::MicStopped { take: 2 }] {
      assert_eq!(d.m.handle(late), None);
    }

    assert_eq!(d.m.handle(Input::MicStopped { take: 3 }), Some(StopRecording(3)));
  }

  /// A model key counts while the hotkey is held: as it goes down, and later, the last one; the
  /// release then stops the recording, however short the press. Not while transcribing, nor once a
  /// tap latched it; in toggle mode while the hotkey is still down.
  #[test]
  fn a_model_key_counts_while_the_hotkey_is_held() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    assert_eq!(d.key(0), Some(PickModel(1, 0)), "at the press");
    assert_eq!(d.key(1), Some(PickModel(1, 1)), "while held");
    assert_eq!(d.up(100), Some(StopRecording(1)), "a short press with a key is a hold");
    assert_eq!(d.key(0), None, "while transcribing");
    d.finish(1);
    assert_eq!(d.key(0), None, "idle");

    d.down(1_000);
    d.up(1_100);
    assert!(d.latched());
    assert_eq!(d.key(0), None, "latched by a tap");
    assert_eq!(d.down(3_000), Some(StopRecording(2)));

    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    assert_eq!(d.key(0), Some(PickModel(1, 0)), "toggle mode, held");
    assert_eq!(d.up(100), None);
    assert_eq!(d.key(0), None, "toggle mode, released");
  }

  /// Another hotkey's press and release leave the press in progress as it is: its chord still
  /// drops it; and a press of another hotkey while it is down is no autorepeat of it.
  #[test]
  fn another_hotkeys_release_leaves_the_press_in_progress() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    assert_eq!(d.press(100, 1), None);
    assert_eq!(d.release(150, 1), None);
    assert_eq!(d.m.handle(Input::KeyAborted { which: 0 }), Some(AbortRecording(1)));

    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    assert_eq!(d.press(500, 1), Some(StopRecording(1)), "any press stops a latched one");
    d.release(600, 1);
    d.finish(1);
    assert_eq!(d.down(1_000), Some(StartRecording(2, 0)));
  }

  /// A model's own hotkey starts a dictation as the main one does: held, or latched by a tap; only
  /// its own release counts, any press stops it once latched.
  #[test]
  fn a_models_own_hotkey_starts_its_dictation() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.press(0, 1), Some(StartRecording(1, 1)));
    assert_eq!(d.up(400), None, "another hotkey's release");
    assert_eq!(d.release(500, 1), Some(StopRecording(1)));
    d.finish(1);

    assert_eq!(d.press(1_000, 2), Some(StartRecording(2, 2)));
    assert_eq!(d.release(1_100, 2), None, "a tap latches");
    assert!(d.latched());
    assert_eq!(d.down(4_000), Some(StopRecording(2)));
  }

  /// fn alone and fn+1 as hotkeys: the chord drops what fn's press started, then starts its own.
  #[test]
  fn a_chord_on_a_lone_hotkeys_modifier_starts_its_own_dictation() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.down(0), Some(StartRecording(1, 0)));
    assert_eq!(d.m.handle(Input::KeyAborted { which: 1 }), None, "not the press's hotkey");
    assert_eq!(d.m.handle(Input::KeyAborted { which: 0 }), Some(AbortRecording(1)));
    assert_eq!(d.press(50, 1), Some(StartRecording(2, 1)));
    assert_eq!(d.release(500, 1), Some(StopRecording(2)));
  }

  #[test]
  fn a_press_that_was_a_chord_drops_the_recording_it_started() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    assert_eq!(d.m.handle(Input::KeyAborted { which: 0 }), Some(AbortRecording(1)));
    assert_eq!(d.up(80), None);

    // A press that stopped a latched recording: what it stopped stays stopped.
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    d.up(100);
    assert_eq!(
      d.m.handle(Input::KeyAborted { which: 0 }),
      None,
      "after the release the press is over"
    );
    assert_eq!(d.down(3_000), Some(StopRecording(1)));
    assert_eq!(d.m.handle(Input::KeyAborted { which: 0 }), None);
  }

  // MARK: Session

  #[derive(Clone, Default)]
  struct FakeOutput(Arc<Mutex<FakeLog>>);

  #[derive(Default)]
  struct FakeLog {
    /// Targets taken so far; a target is its number.
    targets: u32,
    /// `(text, target, keep_in_clipboard)` of each paste.
    pasted: Vec<(String, u32, bool)>,
    copied: Vec<String>,
    refuse: Option<NotPasted>,
  }

  impl FakeOutput {
    fn log(&self) -> std::sync::MutexGuard<'_, FakeLog> {
      self.0.lock().unwrap()
    }
  }

  impl TextOutput for FakeOutput {
    type Target = u32;

    fn target(&mut self) -> u32 {
      self.log().targets += 1;
      self.log().targets
    }

    fn paste(&mut self, text: &str, target: &u32, keep: bool) -> Result<(), NotPasted> {
      let mut log = self.log();
      if let Some(refused) = log.refuse.clone() {
        return Err(refused);
      }

      log.pasted.push((text.to_owned(), *target, keep));
      Ok(())
    }

    fn copy(&mut self, text: &str) -> Result<()> {
      self.log().copied.push(text.to_owned());
      Ok(())
    }
  }

  /// A session in a temp home recording `mic`, with `models` of wavo's in its Hugging Face cache.
  struct InProcess {
    home: TempDir,
    hub: Hub,
    models: FakeModels,
    output: FakeOutput,
    handle: Handle,
    first: Update,
    updates: Receiver<Update>,
    _session: Session,
  }

  impl InProcess {
    fn start(
      name: &str,
      settings: &Settings,
      mic: Option<&[f32]>,
      text: &str,
      models: &[&str],
    ) -> Self {
      let home = TempDir::new(name);
      let hub = Hub::at(home.path().join("hub"));
      for name in models {
        download(&hub, name);
      }

      settings.save(&home.path().join(settings::FILE)).unwrap();
      let mic_path = home.path().join("mic.wav");
      if let Some(pcm) = mic {
        write_wav(&mic_path, pcm);
      }

      let models = FakeModels::new(text);
      let config = Config {
        home: home.path().to_owned(),
        hub: hub.clone(),
        loader: Arc::new(models.clone()),
        fake_mic: Some(mic_path),
      };

      let output = FakeOutput::default();
      let (tx, updates) = unbounded();
      let observer = Box::new(move |update| _ = tx.send(update));
      let session = Session::start(config, output.clone(), observer).unwrap();
      let first = updates.recv_timeout(Duration::from_secs(10)).expect("the first update");
      let handle = session.handle();
      Self { home, hub, models, output, handle, first, updates, _session: session }
    }

    fn update_where(&self, what: &str, matches: impl Fn(&Update) -> bool) -> Update {
      loop {
        let update = self.updates.recv_timeout(Duration::from_secs(10));
        let update = update.unwrap_or_else(|_| panic!("no update: {what}"));
        if matches(&update) {
          return update;
        }
      }
    }

    fn press(&self) {
      self.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
      self.handle.input(Input::KeyUp { at: Instant::now(), which: 0 });
    }

    /// Starts a recording and waits until its audio arrives.
    fn record(&self, level: f32) {
      self.press();
      let recording = self.update_where("recording", |u| u.status.state == Phase::Recording);
      let meter = recording.meter.unwrap();
      poll("the level", Duration::from_secs(10), || (meter.level() > level).then_some(()));
    }

    fn finished(&self, take: Take) -> Status {
      let done = |u: &Update| u.status.last.as_ref().is_some_and(|l| l.take == take);
      self
        .update_where("the end of the dictation", |u| u.status.state == Phase::Idle && done(u))
        .status
    }

    /// Records the tone for a moment as dictation `take`.
    fn dictate(&self, take: Take) -> Last {
      self.record(0.3);
      thread::sleep(Duration::from_millis(300));
      self.press();
      self.finished(take).last.unwrap()
    }

    /// The same with hotkey `which` held, and model keys `keys` ([Settings::held_keys]) pressed
    /// meanwhile.
    fn dictate_held(&self, take: Take, which: usize, keys: &[usize]) -> Last {
      self.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which });
      let recording = self.update_where("recording", |u| u.status.state == Phase::Recording);
      for &index in keys {
        self.handle.input(Input::ModelKey { index });
      }

      let meter = recording.meter.unwrap();
      poll("the level", Duration::from_secs(10), || (meter.level() > 0.3).then_some(()));
      thread::sleep(Duration::from_millis(300));
      self.handle.input(Input::KeyUp { at: Instant::now(), which });
      self.finished(take).last.unwrap()
    }

    fn history(&self) -> History {
      History::open(self.home.path()).unwrap()
    }

    fn entry(&self, id: i64) -> Option<Entry> {
      self.history().db.get_entry(id).unwrap()
    }

    fn recordings(&self) -> usize {
      wav_count(&self.home.path().join(RECORDINGS_DIR))
    }

    /// The history entry of `last`, which keeps its recording.
    fn kept(&self, last: &Last) -> Entry {
      let entry = self.entry(last.entry.expect("an entry")).unwrap();
      assert!(self.history().audio_file(&entry).is_some(), "the recording is kept");
      entry
    }

    fn retranscribed(&self, id: i64) -> Entry {
      self.handle.request(Request::Retranscribe(id)).unwrap();
      poll("transcribed again", Duration::from_secs(10), || self.entry(id).filter(|e| !e.busy))
    }

    /// An entry with `text` and a recording of `pcm` (16 kHz), `age_ms` old.
    fn add_entry(&self, age_ms: i64, text: &str, pcm: &[f32]) -> i64 {
      let entry = Entry { text: text.to_owned(), ..Entry::default() };
      self.insert(age_ms, entry, pcm)
    }

    /// An entry without text in `language`, `age_ms` old, of a second of the tone.
    fn add_untried(&self, age_ms: i64, language: Option<&str>) -> i64 {
      let entry = Entry { language: language.map(str::to_owned), ..Entry::default() };
      self.insert(age_ms, entry, &loud(1.0))
    }

    fn insert(&self, age_ms: i64, entry: Entry, pcm: &[f32]) -> i64 {
      let history = self.history();
      let created_at = now_ms() - age_ms;
      let (name, path) = history.recordings.new_file(created_at);
      write_wav(&path, pcm);
      let duration_ms = pcm.len() as i64 * 1000 / i64::from(SAMPLE_RATE);
      let entry = Entry { created_at, duration_ms, audio_path: Some(name), ..entry };
      history.db.insert(&[entry]).unwrap()[0]
    }
  }

  fn loud(seconds: f64) -> Vec<f32> {
    tone(seconds, 0.5)
  }

  /// Taken for speech by the VAD; dictations of it are transcribed, kept and delivered where they
  /// started. Then a paste refused (only in the history), and auto-paste off (copied).
  #[test]
  fn dictations_are_transcribed_kept_and_delivered() {
    let s = InProcess::start(
      "session-done",
      &Settings::default(),
      Some(&loud(10.0)),
      "hello there",
      &["parakeet-v3"],
    );
    for take in 1..=3 {
      match take {
        2 => s.output.log().refuse = Some(NotPasted::FocusChanged),
        3 => {
          s.output.log().refuse = None;
          s.handle.edit_settings(|s| s.auto_paste = false);
          s.update_where("auto-paste off", |u| !u.settings.auto_paste);
        }
        _ => {}
      }

      let last = s.dictate(take);
      assert_eq!((last.outcome, last.text.as_str()), (Outcome::Done, "hello there"), "{take}");
      let entry = s.kept(&last);
      assert_eq!(
        (entry.text.as_str(), entry.busy, entry.pasted),
        ("hello there", false, last.pasted)
      );
      assert_eq!(entry.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
      let log = s.output.log();
      match take {
        1 => assert_eq!(log.pasted, [("hello there".into(), 1, false)]),
        2 => assert!(last.not_pasted.unwrap().contains("focus")),
        _ => {
          assert_eq!((log.pasted.len(), log.copied.len()), (1, 1));
          assert!(last.not_pasted.unwrap().contains("clipboard"));
        }
      }
    }
  }

  #[test]
  fn silence_is_no_speech_and_never_reaches_the_engine() {
    let s = InProcess::start(
      "session-silence",
      &Settings::default(),
      Some(&tone(10.0, 0.001)),
      "",
      &["parakeet-v3"],
    );
    s.record(0.0);
    s.press();
    let status = s.finished(1);
    let last = status.last.unwrap();
    assert_eq!((last.outcome, last.pasted, last.entry), (Outcome::NoSpeech, false, None));
    assert_eq!((s.models.runs().len(), s.recordings()), (0, 0), "nothing kept");
    assert_eq!(s.output.log().targets, 1, "focus noted at the start");
  }

  /// Escape while recording or transcribing keeps nothing; the next dictation has the same warm
  /// engine.
  /// The update closes the session before the app quits: a dictation that runs, or whose key came
  /// first, ends first; once closed, no key starts one.
  #[test]
  fn closing_waits_for_the_dictation_then_starts_none() {
    let s = InProcess::start("session-close", &Settings::default(), Some(&loud(20.0)), "hi", &[]);
    s.press();
    assert!(s.handle.request(Request::Close).is_err(), "the key came first");
    s.handle.input(Input::Escape);
    s.finished(1);
    assert_eq!(s.handle.request(Request::Close), Ok(()));
    s.press();
    s.handle.input(Input::Toggle);
    assert_eq!(s.handle.request(Request::Close), Ok(()), "no dictation started");
  }

  #[test]
  fn escape_keeps_nothing() {
    let s = InProcess::start(
      "session-cancel",
      &Settings::default(),
      Some(&loud(20.0)),
      "hello there",
      &["parakeet-v3"],
    );
    s.record(0.3);
    s.handle.input(Input::Escape);
    assert_eq!(s.finished(1).last.unwrap().outcome, Outcome::Cancelled);

    let gate = s.models.hold();
    // Enough of the tone for speech: the tail no longer adds it.
    s.record(0.3);
    thread::sleep(Duration::from_millis(300));
    s.press();
    s.update_where("a run", |u| u.status.engine.phase == engine::Phase::Running);
    s.handle.input(Input::Escape);
    let last = s.finished(2).last.unwrap();
    assert_eq!((last.outcome, last.entry), (Outcome::Cancelled, None));
    drop(gate);
    assert_eq!(s.dictate(3).text, "hello there");
    assert_eq!((s.recordings(), s.models.loads(), s.output.log().pasted.len()), (1, 1, 1));
  }

  #[test]
  fn the_tail_waits_for_a_pause() {
    let (stop, ms) = (Instant::now(), Duration::from_millis);
    assert_eq!(tail_end(stop, stop, ms(200)), stop + POST_ROLL, "silent at the stop");
    assert_eq!(tail_end(stop, stop + ms(100), ms(0)), stop + ms(220), "speaking: a pause on");
    assert_eq!(tail_end(stop, stop + ms(250), ms(50)), stop + ms(320));
    assert_eq!(tail_end(stop, stop + ms(400), ms(0)), stop + TAIL_MAX, "at most");
  }

  /// A dictation of `voice` seconds of speech and then silence, stopped `stop` seconds after it
  /// started: how long its recording is.
  fn recorded(name: &str, voice: f64, stop: f64) -> Duration {
    let mut pcm = loud(voice);
    pcm.extend(tone(5.0, 0.0));
    let s = InProcess::start(name, &Settings::default(), Some(&pcm), "hi", &["parakeet-v3"]);
    s.press();
    let started = Instant::now();
    s.update_where("recording", |u| u.status.state == Phase::Recording);
    thread::sleep(Duration::from_secs_f64(stop).saturating_sub(started.elapsed()));
    s.press();
    let last = s.finished(1).last.unwrap();
    Duration::from_millis(s.kept(&last).duration_ms as u64)
  }

  /// `got` within `-early..late` of `want`: the fake microphone starts a little after the press,
  /// and the VAD scores a little behind the audio.
  fn about(got: Duration, want: Duration, what: &str) {
    let (early, late) = (Duration::from_millis(150), Duration::from_millis(350));
    assert!(got + early >= want && got <= want + late, "{what}: {got:?}, not about {want:?}");
  }

  /// A word finished after the stop is kept: the recording goes on until the voice pauses.
  #[test]
  fn the_recording_goes_on_while_the_voice_does() {
    let got = recorded("session-tail-voice", 1.2, 1.0);
    about(got, Duration::from_secs_f64(1.2) + TAIL_PAUSE, "speech 200 ms past the stop");
  }

  #[test]
  fn a_silent_stop_ends_after_the_post_roll() {
    let got = recorded("session-tail-silent", 0.5, 1.0);
    about(got, Duration::from_secs(1) + POST_ROLL, "silent at the stop");
  }

  #[test]
  fn the_tail_ends_at_its_cap() {
    let got = recorded("session-tail-cap", 5.0, 1.0);
    about(got, Duration::from_secs(1) + TAIL_MAX, "speech past the cap");
  }

  /// The tail after real speech, as the session's timer runs on the meter ([audio::pauses]):
  /// stopped as the voice ends, it closes once the word has ended; silent at the stop, after the
  /// post-roll; speaking on, at the cap; a hiss after speech, by the cap at the latest.
  #[test]
  #[ignore = "needs the fixtures: make test-models"]
  fn the_tail_after_speech() {
    let ms = Duration::from_millis;
    // White noise at -40 dBFS RMS: alone the VAD scores it 0.2, after speech above 0.5.
    let mut seed = 0x2545_f491_u32;
    let hiss: Vec<f32> = (0..48_000)
      .map(|_| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * 0.01 * 3f32.sqrt()
      })
      .collect();
    for name in ["ru-short", "en-short", "ru-typical", "en-paragraph"] {
      let speech = crate::testing::fixture_pcm(name);
      let last = speech.iter().rposition(|s| s.abs() > 0.02).unwrap();
      let end = ms(last as u64 * 1000 / u64::from(audio::SAMPLE_RATE));
      let with = |after: &[f32]| [&speech[..last], after].concat();
      let (quiet, hissing) = (audio::pauses(&with(&[0.0; 48_000])), audio::pauses(&with(&hiss)));
      // The first poll at or after the stop at which the tail is due.
      let tail = |pauses: &[(Duration, Duration)], stop: Duration| {
        let at = Instant::now();
        let mut polls = pauses.iter().filter(|(t, _)| *t >= stop);
        let (t, _) = polls.find(|(t, p)| tail_end(at + stop, at + *t, *p) <= at + *t).unwrap();
        *t - stop
      };
      let got = [
        tail(&quiet, end),
        tail(&quiet, end + ms(500)),
        tail(&quiet, end - ms(1000)),
        tail(&hissing, end),
      ];
      println!(
        "{name}: as the voice ends {:?}, silent {:?}, speaking on {:?}, hiss {:?}",
        got[0], got[1], got[2], got[3]
      );
      // A slow voice's scores take up to 200 ms to fall to a pause.
      assert!(got[0] >= TAIL_PAUSE && got[0] < TAIL_MAX, "{name}: {:?}", got[0]);
      assert!(got[1] >= POST_ROLL && got[1] <= POST_ROLL + ms(20), "{name}: {:?}", got[1]);
      assert!(got[2] >= TAIL_MAX, "{name}: {got:?}");
      assert!(got.iter().all(|&t| t <= TAIL_MAX + ms(10)), "{name}: {got:?}");
    }
  }

  /// Escape gets no tail.
  #[test]
  fn escape_ends_at_once() {
    let pcm = loud(10.0);
    let s = InProcess::start("session-tail-esc", &Settings::default(), Some(&pcm), "", &[]);
    s.record(0.3);
    let pressed = Instant::now();
    s.handle.input(Input::Escape);
    assert_eq!(s.finished(1).last.unwrap().outcome, Outcome::Cancelled);
    assert!(pressed.elapsed() < POST_ROLL, "{:?}", pressed.elapsed());
  }

  /// No model: said from the start; the recording is kept without text and transcribed by itself
  /// once one is there, as is an older entry; one without speech goes, one with text stays.
  #[test]
  fn without_a_model_recordings_wait_for_one() {
    let s = InProcess::start(
      "session-no-model",
      &Settings::default(),
      Some(&loud(10.0)),
      "hello there",
      &[],
    );
    assert_eq!(s.first.status.no_model, Some(NoModel::NoneDownloaded));
    let last = s.dictate(1);
    assert_eq!(
      (last.outcome, last.error.as_deref()),
      (Outcome::NoModel, Some("no model is downloaded"))
    );
    let entry = s.kept(&last);
    assert_eq!((entry.text.as_str(), entry.tried_model, entry.busy), ("", None, false));
    let refused = s.handle.request(Request::Retranscribe(entry.id)).unwrap_err();
    assert!(refused.contains("no model"), "{refused}");

    let older = s.add_entry(2_000, "", &loud(1.0));
    let silent = s.add_entry(3_000, "", &tone(0.5, 0.0));
    let with_text = s.add_entry(4_000, "текст", &loud(1.0));
    download(&s.hub, "parakeet-v3");
    s.handle.check_model();
    for id in [entry.id, older] {
      let done = poll("transcribed", Duration::from_secs(10), || {
        s.entry(id).filter(|e| !e.text.is_empty() && !e.busy)
      });
      assert_eq!(done.text, "hello there");
    }

    poll("the silent one gone", Duration::from_secs(10), || {
      s.entry(silent).is_none().then_some(())
    });
    assert_eq!(s.entry(with_text).unwrap().tried_model, None, "one with text is left alone");
    assert!(s.output.log().pasted.is_empty());
  }

  /// A model that does not load tries no entry: they wait for a dictation that loads it.
  #[test]
  fn entries_without_text_wait_for_a_model_that_loads() {
    let s = InProcess::start(
      "session-broken-model",
      &Settings::default(),
      Some(&loud(10.0)),
      "hello there",
      &[],
    );
    let waiting = s.add_entry(2_000, "", &loud(1.0));
    let model = download(&s.hub, "parakeet-v3");
    fs::write(&model, b"BROKEN").unwrap();
    s.handle.check_model();
    s.update_where("the load failed", |u| {
      matches!(u.status.no_model, Some(NoModel::LoadFailed { .. }))
        && u.status.retranscribing.is_empty()
    });
    let entry = s.entry(waiting).unwrap();
    assert_eq!((entry.text.as_str(), entry.tried_model, entry.busy), ("", None, false));

    let failed = s.dictate(1);
    assert_eq!(failed.outcome, Outcome::NoModel);
    assert_eq!(s.kept(&failed).tried_model, None);

    fs::write(&model, b"GGUF").unwrap();
    assert_eq!(s.dictate(2).outcome, Outcome::Done);
    for id in [waiting, failed.entry.unwrap()] {
      let done = poll("transcribed", Duration::from_secs(10), || {
        s.entry(id).filter(|e| !e.text.is_empty() && !e.busy)
      });
      assert_eq!(done.text, "hello there");
    }
  }

  /// A failed retranscription keeps the text and says why; new text replaces it, and a restore
  /// puts back the first text, once.
  #[test]
  fn a_retranscription_replaces_the_text_only_with_text_and_restores() {
    let s = InProcess::start(
      "session-again",
      &Settings::default(),
      None,
      "hello there",
      &["parakeet-v3"],
    );
    let id = s.add_entry(1_000, "текст", &loud(1.0));
    s.models.panic_next();
    assert_eq!(s.retranscribed(id).text, "текст");
    let error = s.update_where("the error", |u| u.status.error.is_some()).status.error.unwrap();
    assert!(
      error.starts_with(&format!("entry {id} could not")) && error.contains("panicked"),
      "{error}"
    );

    let restore = || s.handle.request(Request::Restore(id));
    assert!(restore().unwrap_err().contains("no text before"));
    assert_eq!(s.retranscribed(id).text, "hello there");
    assert_eq!(s.retranscribed(id).text, "hello there", "twice: the first text stays the one");
    s.update_where("restorable", |u| u.status.restorable == [id]);
    restore().unwrap();
    assert_eq!(s.entry(id).unwrap().text, "текст");
    assert!(restore().is_err(), "once");
  }

  const PARAKEET_V3: &str = "parakeet-tdt-0.6b-v3-Q8_0";
  const PARAKEET_V2: &str = "parakeet-tdt-0.6b-v2-Q8_0";
  const GIGAAM: &str = "gigaam-v3-e2e-rnnt-Q8_0";
  const PARAKEET_HE: &str = "parakeet-tdt-0.6b-v3-he-Q8_0";
  const WHISPER: &str = "whisper-large-v3-turbo-Q8_0";

  /// A model of one language alone takes the entries without text in it and the detected ones; one
  /// in another language waits for a model that knows it.
  #[test]
  fn a_model_of_one_language_alone_takes_its_entries_and_the_detected_ones() {
    let s = InProcess::start("session-untried-language", &Settings::default(), None, "hi", &[]);
    let ru = s.add_untried(1_000, Some("ru"));
    let en = s.add_untried(2_000, Some("en"));
    let detected = s.add_untried(3_000, None);
    download(&s.hub, "gigaam-v3");
    s.handle.check_model();
    for (id, language) in [(ru, Some("ru")), (detected, None)] {
      let done = poll("transcribed", Duration::from_secs(10), || {
        s.entry(id).filter(|e| !e.text.is_empty() && !e.busy)
      });
      assert_eq!((done.model.as_deref(), done.language.as_deref()), (Some(GIGAAM), language));
    }

    thread::sleep(Duration::from_millis(100));
    let entry = s.entry(en).unwrap();
    assert_eq!((entry.text.as_str(), entry.tried_model), ("", None));
  }

  fn model_key(key: &str, model: &str) -> ModelKey {
    ModelKey { key: key.into(), model: model.into() }
  }

  /// A model key sends the dictation to its model, in the model's language when it has one: the
  /// last key pressed, or the model's own hotkey; a key's model not on disk gets no stand-in, its
  /// recording waits in its language.
  #[test]
  fn a_model_key_sends_the_dictation_to_its_model() {
    let keys = vec![
      model_key("z", "gigaam-v3"),
      model_key("f1", "parakeet-v2"),
      model_key("x", "moonshine-tiny"),
    ];
    let settings = Settings { model: Some("parakeet-v3".into()), keys, ..Settings::default() };
    let models = ["parakeet-v3", "gigaam-v3", "parakeet-v2"];
    let s = InProcess::start("session-keys", &settings, Some(&loud(10.0)), "hi", &models);
    // Dictation, hotkey, keys pressed, its model and language.
    let cases = [
      (1, 0, &[1, 0][..], GIGAAM, Some("ru")),
      (2, 1, &[][..], PARAKEET_V2, Some("en")),
      (3, 0, &[][..], PARAKEET_V3, None),
    ];
    for (take, which, keys, model, language) in cases {
      let last = s.dictate_held(take, which, keys);
      assert_eq!((last.outcome, last.text.as_str()), (Outcome::Done, "hi"), "{take}");
      let entry = s.kept(&last);
      assert_eq!((entry.model.as_deref(), entry.language.as_deref()), (Some(model), language));
    }

    let last = s.dictate_held(4, 0, &[1]);
    assert_eq!(last.outcome, Outcome::NoModel);
    assert_eq!(last.error.as_deref(), Some("moonshine-tiny is not on disk"));
    let entry = s.kept(&last);
    let got = (entry.text.as_str(), entry.tried_model.as_deref(), entry.language.as_deref());
    assert_eq!(got, ("", None, Some("en")));
    // The first press loaded the default model for nothing: it went with the key, and came back.
    assert_eq!(s.models.loads(), 4, "an engine per model");
  }

  /// A key while recording loads its model and lets go of the one loaded for the default: with an
  /// unload right after each dictation, one model stays in memory.
  #[test]
  fn a_model_key_moves_the_prewarm_to_its_model() {
    let settings = Settings {
      keys: vec![model_key("z", "gigaam-v3")],
      unload_after_minutes: Some(0),
      ..Settings::default()
    };
    let models = ["parakeet-v3", "gigaam-v3"];
    let s = InProcess::start("session-key-prewarm", &settings, Some(&loud(10.0)), "hi", &models);
    s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
    poll("the default model", Duration::from_secs(10), || (s.models.live() == 1).then_some(()));
    s.handle.input(Input::ModelKey { index: 0 });
    poll("the key's model alone", Duration::from_secs(10), || {
      (s.models.loads() == 2 && s.models.live() == 1).then_some(())
    });
    s.handle.input(Input::Escape);
    s.finished(1);
  }

  /// A key lets go of the model its press loaded for the default one, and leaves one loaded before.
  #[test]
  fn a_model_key_unloads_only_what_its_press_loaded() {
    let settings = Settings { keys: vec![model_key("z", "gigaam-v3")], ..Settings::default() };
    let models = ["parakeet-v3", "gigaam-v3"];
    let s = InProcess::start("session-key-cold", &settings, Some(&loud(10.0)), "hi", &models);
    let down = || s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
    let up = || s.handle.input(Input::KeyUp { at: Instant::now(), which: 0 });
    let loaded = |what: &str, loads: usize, live: usize| {
      poll(what, Duration::from_secs(10), || {
        (s.models.loads() == loads && s.models.live() == live).then_some(())
      })
    };

    down();
    loaded("the default model", 1, 1);
    s.handle.input(Input::ModelKey { index: 0 });
    loaded("the key's model alone", 2, 1);
    s.handle.input(Input::Escape);
    s.finished(1);
    up();

    s.dictate(2);
    loaded("both models", 3, 2);
    down();
    s.handle.input(Input::ModelKey { index: 0 });
    thread::sleep(Duration::from_millis(200));
    assert_eq!((s.models.loads(), s.models.live()), (3, 2), "the default one stays");
    s.handle.input(Input::Escape);
    s.finished(3);
  }

  /// A key of the model the press already loads keeps that load the dictation's own: a key of
  /// another model then drops it.
  #[test]
  fn a_key_of_the_same_model_keeps_the_presss_load_its_own() {
    let settings = Settings {
      keys: vec![model_key("z", "parakeet-v3"), model_key("x", "gigaam-v3")],
      ..Settings::default()
    };
    let models = ["parakeet-v3", "gigaam-v3"];
    let s = InProcess::start("session-key-same", &settings, Some(&loud(10.0)), "hi", &models);
    s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
    poll("the default model", Duration::from_secs(10), || (s.models.live() == 1).then_some(()));
    for index in [0, 1] {
      s.handle.input(Input::ModelKey { index });
    }

    poll("the key's model alone", Duration::from_secs(10), || {
      (s.models.loads() == 2 && s.models.live() == 1).then_some(())
    });
    s.handle.input(Input::Escape);
    s.finished(1);
  }

  /// A press that was a chord (fn+←) keeps the model it loaded, as without model keys: the next
  /// press needs it.
  #[test]
  fn a_chord_keeps_the_model_its_press_loaded() {
    let mic = loud(10.0);
    let s =
      InProcess::start("session-chord", &Settings::default(), Some(&mic), "", &["parakeet-v3"]);
    s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
    s.update_where("recording", |u| u.status.state == Phase::Recording);
    poll("the model", Duration::from_secs(10), || (s.models.live() == 1).then_some(()));
    s.handle.input(Input::KeyAborted { which: 0 });
    s.update_where("the press dropped", |u| u.status.state == Phase::Idle);
    thread::sleep(Duration::from_millis(100));
    assert_eq!((s.models.loads(), s.models.live()), (1, 1));
  }

  /// With an unload right after each dictation, a Transcribe Again while recording leaves the
  /// dictation's model in memory.
  #[test]
  fn transcribe_again_while_recording_keeps_the_dictations_model() {
    let settings = Settings {
      keys: vec![model_key("z", "gigaam-v3")],
      unload_after_minutes: Some(0),
      ..Settings::default()
    };
    let models = ["parakeet-v3", "gigaam-v3"];
    let mic = loud(10.0);
    let s = InProcess::start("session-again-recording", &settings, Some(&mic), "hi", &models);
    let id = s.add_entry(1_000, "текст", &loud(1.0));
    s.history().db.update_text(id, "текст", Some(GIGAAM), Some("ru")).unwrap();
    s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false, which: 0 });
    poll("the dictation's model", Duration::from_secs(10), || (s.models.live() == 1).then_some(()));

    assert_eq!(s.retranscribed(id).model.as_deref(), Some(GIGAAM));
    poll("the other model gone", Duration::from_secs(10), || (s.models.live() == 1).then_some(()));

    thread::sleep(Duration::from_millis(100));
    assert_eq!((s.models.loads(), s.models.live()), (2, 1), "the dictation's stays");
    s.handle.input(Input::Escape);
    s.finished(1);
  }

  /// Unload frees every model in memory: the default one's and a key's.
  #[test]
  fn unload_frees_every_model() {
    let settings = Settings { keys: vec![model_key("z", "gigaam-v3")], ..Settings::default() };
    let models = ["parakeet-v3", "gigaam-v3"];
    let s = InProcess::start("session-unload", &settings, Some(&loud(10.0)), "hi", &models);
    s.dictate(1);
    s.dictate_held(2, 0, &[0]);
    assert_eq!((s.models.loads(), s.models.live()), (2, 2));
    s.handle.request(Request::Unload).unwrap();
    poll("no model", Duration::from_secs(10), || (s.models.live() == 0).then_some(()));
  }

  /// Entries without text go to the first model on disk of their language, the default one or a
  /// key's; those without a language (detected) to the first of more than one.
  #[test]
  fn entries_without_text_go_to_a_model_of_their_language() {
    let settings = Settings { model: Some("parakeet-v2".into()), ..Settings::default() };
    let models = ["parakeet-v2", "gigaam-v3", "parakeet-v3"];
    let s = InProcess::start("session-untried-languages", &settings, None, "hi", &models);
    let ru = s.add_untried(1_000, Some("ru"));
    let en = s.add_untried(2_000, Some("en"));
    let detected = s.add_untried(3_000, None);
    s.handle.edit_settings(|s| {
      s.keys = vec![model_key("z", "gigaam-v3"), model_key("x", "parakeet-v3")];
    });

    for (id, model) in [(ru, GIGAAM), (en, PARAKEET_V2), (detected, PARAKEET_V3)] {
      let entry = poll("transcribed", Duration::from_secs(10), || {
        s.entry(id).filter(|e| !e.text.is_empty() && !e.busy)
      });
      assert_eq!(entry.model.as_deref(), Some(model), "{id}");
    }
  }

  /// An entry that gives no text is not passed back and forth between two models: it goes to the
  /// first one that takes it.
  #[test]
  fn two_models_do_not_take_turns_at_an_entry_without_text() {
    let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
    let models = ["parakeet-v3", "whisper-turbo"];
    let s = InProcess::start("session-untried-turns", &settings, None, "", &models);
    let id = s.add_untried(1_000, None);
    s.handle.edit_settings(|s| s.keys = vec![model_key("z", "whisper-turbo")]);
    poll("tried", Duration::from_secs(10), || {
      s.entry(id).filter(|e| e.tried_model.is_some() && !e.busy)
    });

    thread::sleep(Duration::from_millis(200));
    let entry = s.entry(id).unwrap();
    assert_eq!((entry.text.as_str(), entry.tried_model.as_deref()), ("", Some(PARAKEET_V3)));
    assert_eq!(s.models.runs().len(), 1, "tried once");
  }

  /// Transcribe Again goes to the model the text came from when that is the one found or a key's,
  /// else to the one found; an entry without text to the model of its language.
  #[test]
  fn transcribe_again_goes_to_the_entrys_own_model() {
    let settings = Settings {
      model: Some("parakeet-v3".into()),
      keys: vec![model_key("z", "gigaam-v3"), model_key("x", "parakeet-he")],
      ..Settings::default()
    };
    let models = ["parakeet-v3", "gigaam-v3", "parakeet-he", "whisper-turbo"];
    let s = InProcess::start("session-again-own", &settings, None, "hi", &models);
    let id = s.add_entry(1_000, "текст", &loud(1.0));
    let again = |model: &str, language: Option<&str>| {
      s.history().db.update_text(id, "текст", Some(model), language).unwrap();
      s.retranscribed(id)
    };

    let entry = again(GIGAAM, Some("ru"));
    assert_eq!((entry.model.as_deref(), entry.language.as_deref()), (Some(GIGAAM), Some("ru")));
    assert_eq!(again(WHISPER, None).model.as_deref(), Some(PARAKEET_V3), "a model of no key");
    assert_eq!(again("gone-Q8_0", None).model.as_deref(), Some(PARAKEET_V3));

    let untried = s.add_untried(2_000, Some("he"));
    let entry = s.retranscribed(untried);
    assert_eq!(
      (entry.model.as_deref(), entry.language.as_deref()),
      (Some(PARAKEET_HE), Some("he"))
    );
  }

  #[test]
  fn a_microphone_that_will_not_open_leaves_nothing_behind() {
    let settings = Settings { history_retention: Retention::Count(1), ..Settings::default() };
    let s = InProcess::start("session-no-mic", &settings, None, "", &[]);
    s.press();
    let failed = s.update_where("the error", |u| u.status.error.is_some()).status;
    assert!(failed.error.unwrap().contains("could not start recording"));
    assert_eq!((failed.state, failed.last), (Phase::Idle, None));
    let dir = s.home.path().join(RECORDINGS_DIR);
    assert_eq!(fs::read_dir(dir).unwrap().count(), 0, "no file, nor its day folder");
  }

  /// An entry without text goes to the first model on disk of its language, wavo's before a file of
  /// the person's own; a detected one to the first of more than one or such a file, else to the one
  /// found.
  #[test]
  fn an_entry_without_text_goes_to_the_model_of_its_language() {
    let on = |name: &str, key: bool| OnDisk {
      choice: key.then(|| name.to_owned()),
      path: PathBuf::from(name),
      info: hub::lookup(name).ok(),
    };
    let to = |models: &[OnDisk], language: Option<&str>| {
      route(models, language).map(|m| m.path.display().to_string())
    };

    let two = [on("gigaam-v3", false), on("parakeet-v2", true)];
    assert_eq!(to(&two, Some("ru")).as_deref(), Some("gigaam-v3"));
    assert_eq!(to(&two, Some("EN")).as_deref(), Some("parakeet-v2"));
    assert_eq!(to(&two, Some("de")), None);
    assert_eq!(to(&two, None).as_deref(), Some("gigaam-v3"), "detected: the one found");

    let own = [on("/models/own.gguf", false), on("gigaam-v3", true), on("parakeet-v3", true)];
    assert_eq!(to(&own, Some("ru")).as_deref(), Some("gigaam-v3"), "the person's file last");
    assert_eq!(to(&own, None).as_deref(), Some("parakeet-v3"), "detected: the first of more");
    assert_eq!(to(&own, Some("he")).as_deref(), Some("/models/own.gguf"));
  }

  #[test]
  fn models_are_labelled_and_a_model_for_one_language_says_which() {
    assert_eq!(label("parakeet-v3"), "parakeet-tdt-0.6b-v3");
    assert_eq!(label("/models/broken.gguf"), "broken");
    let note = |name| understands_only(hub::lookup(name).unwrap());
    let v2 = "parakeet-tdt-0.6b-v2 understands English only";
    assert_eq!(note("parakeet-v2").as_deref(), Some(v2));
    assert_eq!(note("parakeet-v3"), None);
  }
}
