// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Session tests.
//!
//! - In-process, on [`FakeModels`] with models in a Hugging Face cache in the temp home (none by
//!   default), a fake microphone (a WAV file) and [`FakeOutput`]: dictation end to end; the paths
//!   that never reach the engine (no speech, cancel, a microphone that will not open, retention,
//!   hotkey input); no model at all, a chosen one gone from the disk, one that does not load; a
//!   recording with speech is always kept, its text possibly empty, and transcribed by itself once
//!   another model is there, never twice by the same one; a transcription again and its undo;
//!   pastes refused, kept on the clipboard or off; a model switch; what a crash left. With
//!   [`Faults`]: a run that panics, one that hangs past its deadline, a cancel while one is held.
//! - `#[ignore]`, `make test-models`: the headless app (`wavo-app daemon`) as its own process with
//!   a fake microphone, driven through `wavo-app ctl` and the socket, with GigaAM: a full
//!   dictation, a cancel, no speech, a second instance.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, thread};

use crossbeam_channel::{Receiver, bounded, unbounded};
use wavo::hub::Hub;

use super::controller::{Config, Handle, NoModel, Outcome, Phase, Session, Status, Update};
use super::machine::{Command, Input};
use super::output::{FakeOutput, NotPasted};
use crate::audio::SAMPLE_RATE;
use crate::ctl::{self, Reply, Request};
use crate::engine::supervisor::{Deadlines, Phase as EnginePhase};
use crate::history::db::{Entry, NewEntry};
use crate::history::{History, Retention, now_ms, recordings};
use crate::settings::{self, Activation, Settings};
use crate::testing::{
  FakeModels, Fault, Faults, TempDir, app_exe, download, entry_ids, fixture, fixture_wav, model_ru,
  poll, wav_count, wer, write_wav,
};

const MAX_WER: f64 = 0.15;

fn status_of(reply: Reply) -> Status {
  assert_eq!(reply.error, None);
  serde_json::from_value(reply.result).unwrap()
}

/// Every entry of the history in `home`, oldest first.
fn entries_in(home: &Path) -> Vec<Entry> {
  let history = History::open(home).unwrap();
  entry_ids(home).into_iter().map(|id| history.db.get_entry(id).unwrap().unwrap()).collect()
}

/// How many recordings are in the home `home`, at any depth.
fn recordings_in(home: &Path) -> usize {
  wav_count(&home.join(recordings::DIR))
}

/// Polls `status` until dictation `take` has ended.
fn finished(take: u64, status: impl Fn() -> Status) -> Status {
  poll("end of the dictation", Duration::from_secs(60), || {
    let s = status();
    (s.state == Phase::Idle && s.last.as_ref().is_some_and(|l| l.take == take)).then_some(s)
  })
}

/// What transcribes in an [`InProcess`] session, through its [`Faults`]: [`FakeModels`] answering
/// this text, with these of wavo's models in a Hugging Face cache in the temp home
/// ([`InProcess::hub`]).
struct Models(&'static str, &'static [&'static str]);

/// No model at all: nothing can reach an engine.
const NO_MODELS: Models = Models("", &[]);
/// One model, which makes "hello there" of anything.
const ONE_MODEL: Models = Models("hello there", &["parakeet-v3"]);

/// A session in a temp home, recording `mic`, with the fake output; the observer's first update
/// (after the start's recovery) is `first`, what it gets later goes to `updates`.
struct InProcess {
  home: TempDir,
  /// Where the session finds its models.
  hub: Hub,
  /// Injected into the engine's loads and runs.
  faults: Faults,
  output: FakeOutput,
  handle: Handle,
  first: Update,
  updates: Receiver<Update>,
  _session: Session,
}

impl InProcess {
  fn start(name: &str, settings: &Settings, mic: Option<&[f32]>, models: Models) -> Self {
    Self::start_in(TempDir::new(name), settings, mic, models, Deadlines::default())
  }

  /// In `home`, which may have a history already.
  fn start_in(
    home: TempDir,
    settings: &Settings,
    mic: Option<&[f32]>,
    models: Models,
    deadlines: Deadlines,
  ) -> Self {
    let faults = Faults::new();
    let Models(text, names) = models;
    let hub = Hub::at(home.path().join("hub"));
    for name in names {
      download(&hub, name);
    }

    settings.save(&home.path().join(settings::FILE)).unwrap();
    let mic_path = home.path().join("mic.wav");
    if let Some(pcm) = mic {
      write_wav(&mic_path, pcm);
    }

    let config = Config {
      home: home.path().to_owned(),
      hub: hub.clone(),
      loader: faults.wrap(FakeModels::new(text)),
      deadlines,
      fake_mic: Some(mic_path),
    };
    let output = FakeOutput::default();
    let (tx, updates) = unbounded();
    let observer = Box::new(move |update| {
      let _ = tx.send(update);
    });
    let session = Session::start_observed(config, output.clone(), observer).unwrap();
    let first = updates.recv_timeout(Duration::from_secs(10)).expect("the first update");
    let handle = session.handle();
    Self { home, hub, faults, output, handle, first, updates, _session: session }
  }

  /// The next update that `matches`, skipping the others.
  fn update_where(&self, what: &str, matches: impl Fn(&Update) -> bool) -> Update {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let left = deadline.saturating_duration_since(Instant::now());
      let update = self.updates.recv_timeout(left).unwrap_or_else(|_| panic!("no update: {what}"));
      if matches(&update) {
        return update;
      }
    }
  }

  fn status(&self) -> Status {
    status_of(self.handle.request(Request::Status))
  }

  fn send(&self, r: Request) -> Reply {
    self.handle.request(r)
  }

  fn finished(&self, take: u64) -> Status {
    finished(take, || self.status())
  }

  fn history(&self) -> History {
    History::open(self.home.path()).unwrap()
  }

  fn entry(&self, id: i64) -> Option<Entry> {
    self.history().db.get_entry(id).unwrap()
  }

  fn entries(&self) -> Vec<Entry> {
    entries_in(self.home.path())
  }

  fn recordings(&self) -> usize {
    recordings_in(self.home.path())
  }
}

fn silence(seconds: f64) -> Vec<f32> {
  vec![0.0; (seconds * SAMPLE_RATE as f64) as usize]
}

/// A 440 Hz sine of amplitude 0.5: a level of about 0.35.
fn tone(seconds: f64) -> Vec<f32> {
  crate::testing::tone(seconds, 0.5)
}

/// The tone at -60 dBFS: a quiet room, with a level above zero that shows the audio arrives.
fn quiet(seconds: f64) -> Vec<f32> {
  crate::testing::tone(seconds, 0.001)
}

/// Waits until the recording's level is above `level`: the fake microphone's audio arrives.
fn wait_for_level(s: &InProcess, level: f32) {
  poll("the level", Duration::from_secs(10), || {
    s.status().recording.is_some_and(|r| r.level > level).then_some(())
  });
}

#[test]
fn silence_is_no_speech_and_never_reaches_the_engine() {
  let s = InProcess::start("session-silence", &Settings::default(), Some(&quiet(10.0)), NO_MODELS);
  let started = Instant::now();
  assert_eq!(s.send(Request::Start).result["state"], "recording");
  let recording = s.status().recording.unwrap();
  assert!(recording.device.contains("mic.wav"), "{}", recording.device);
  wait_for_level(&s, 0.0);
  assert_eq!(s.send(Request::Stop).result["state"], "transcribing", "the post-roll");
  let status = s.finished(1);
  let wall = started.elapsed().as_secs_f64();
  let last = status.last.unwrap();
  assert_eq!((last.outcome, last.pasted, last.entry), (Outcome::NoSpeech, false, None));
  // The fake microphone delivers in real time, so never more than the time it was open.
  assert!(last.seconds > 0.0 && last.seconds <= wall, "{} s recorded in {wall} s", last.seconds);
  assert_eq!(status.engine.phase, EnginePhase::Stopped, "no engine without a model");
  assert!(s.entries().is_empty(), "no speech is not kept");
  assert_eq!(s.recordings(), 0, "nor is its recording");
  let log = s.output.log();
  assert_eq!((log.targets, log.pasted.len()), (1, 0), "focus noted at the start, nothing pasted");
}

#[test]
fn cancel_while_recording_keeps_nothing_and_pastes_nothing() {
  let s = InProcess::start("session-cancel", &Settings::default(), Some(&tone(10.0)), NO_MODELS);
  let started = Instant::now();
  s.send(Request::Toggle);
  wait_for_level(&s, 0.3);
  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  let last = s.finished(1).last.unwrap();
  let wall = started.elapsed().as_secs_f64();
  assert_eq!((last.outcome, last.entry), (Outcome::Cancelled, None));
  assert!(last.seconds > 0.0 && last.seconds <= wall, "{} s recorded in {wall} s", last.seconds);
  assert!(s.entries().is_empty(), "a cancelled dictation is not kept");
  assert_eq!(s.recordings(), 0, "nor is its recording");
  assert!(s.output.log().pasted.is_empty());
  assert_eq!(s.send(Request::Cancel).result["state"], "idle", "nothing left to cancel");
}

#[test]
fn the_microphone_opens_before_the_focus_check() {
  let s =
    InProcess::start("session-mic-first", &Settings::default(), Some(&silence(0.1)), NO_MODELS);
  let (go, gate) = bounded(0);
  s.output.log().target_gate = Some(gate);
  let handle = s.handle.clone();
  let start = thread::spawn(move || handle.request(Request::Start));
  // The focus check waits for the test; the recording runs meanwhile.
  poll("a recording during the focus check", Duration::from_secs(10), || {
    (s.recordings() == 1).then_some(())
  });
  go.send(()).unwrap();
  assert_eq!(start.join().unwrap().result["state"], "recording");
  assert_eq!(s.output.log().targets, 1);
}

#[test]
fn a_model_still_loading_never_holds_up_the_recording() {
  let s = InProcess::start("session-slow-load", &Settings::default(), Some(&tone(10.0)), ONE_MODEL);
  s.faults.load(Fault::Hang);
  let (go, gate) = bounded(0);
  s.output.log().target_gate = Some(gate);
  s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false });
  // The shell shows the recording while the focus check still waits…
  let recording = s.update_where("recording", |u| u.status.state == Phase::Recording);
  assert!(recording.status.recording.is_some() && recording.meter.is_some());
  assert_eq!(recording.status.engine.model, None, "no model lookup before the focus is noted");
  go.send(()).unwrap();
  // …and the audio arrives while the engine has not loaded the model.
  wait_for_level(&s, 0.3);
  let status = poll("the model loading", Duration::from_secs(10), || {
    Some(s.status()).filter(|s| s.engine.phase == EnginePhase::Loading)
  });
  assert_eq!(status.state, Phase::Recording);
  assert_eq!(s.output.log().targets, 1, "the focus noted after the recording is shown");
  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  s.faults.release();
}

#[test]
fn the_post_roll_records_after_the_stop_and_a_cancel_in_it_keeps_nothing() {
  let s =
    InProcess::start("session-post-roll", &Settings::default(), Some(&silence(0.1)), NO_MODELS);
  s.send(Request::Start);
  thread::sleep(Duration::from_millis(300));
  let stopped = Instant::now();
  assert_eq!(s.send(Request::Stop).result["state"], "transcribing");
  let status = s.finished(1);
  // The microphone stays open after the stop.
  assert_eq!(status.last.unwrap().outcome, Outcome::NoSpeech);
  assert!(stopped.elapsed() >= Duration::from_millis(48));

  // Stop and cancel in one go: the cancel finds the post-roll.
  s.send(Request::Start);
  thread::sleep(Duration::from_millis(200));
  s.handle.input(Input::Command(Command::Stop));
  s.handle.input(Input::Command(Command::Cancel));
  let last = s.finished(2).last.unwrap();
  assert_eq!((last.outcome, last.entry), (Outcome::Cancelled, None));
  assert!(s.entries().is_empty());
  assert_eq!(s.recordings(), 0, "the recording is deleted");
}

#[test]
fn speech_past_the_stop_keeps_the_post_roll_to_its_maximum() {
  // The VAD takes the tone for speech: it goes on past the stop.
  let s =
    InProcess::start("session-post-speech", &Settings::default(), Some(&tone(20.0)), NO_MODELS);
  s.send(Request::Start);
  thread::sleep(Duration::from_millis(300));
  let stopped = Instant::now();
  s.send(Request::Stop);
  let last = s.finished(1).last.unwrap();
  assert!(stopped.elapsed() >= Duration::from_millis(300), "{:?}", stopped.elapsed());
  assert_eq!(last.outcome, Outcome::NoModel, "speech, and no model for it");
}

/// Records the tone for a moment as dictation `take` and waits for its end.
fn dictate(s: &InProcess, take: u64) -> Status {
  assert_eq!(s.send(Request::Start).result["state"], "recording");
  wait_for_level(s, 0.3);
  thread::sleep(Duration::from_millis(300));
  s.send(Request::Stop);
  s.finished(take)
}

/// The history entry of the dictation that ended last, which keeps its recording.
fn kept(s: &InProcess, status: &Status) -> Entry {
  let id = status.last.as_ref().and_then(|l| l.entry).expect("an entry");
  let entry = s.entry(id).unwrap();
  assert!(s.history().audio_file(&entry).is_some(), "the recording is kept");
  entry
}

/// Entry `id` transcribed again, once it is done.
fn retranscribed(s: &InProcess, id: i64) -> Entry {
  let reply = s.send(Request::Retranscribe { id });
  assert_eq!(reply.result["state"], "transcribing", "{reply:?}");
  poll("transcribed again", Duration::from_secs(10), || s.entry(id).filter(|e| !e.busy))
}

/// Dictations transcribed and kept with their recordings, the text pasted where each started; the
/// observer sees the meter while it records. Then a paste refused (the focus moved: the text is
/// only in the history), the text kept on the clipboard, and auto-paste off (copied, whatever
/// "keep" says).
#[test]
fn dictations_are_transcribed_kept_and_delivered() {
  let s = InProcess::start("session-done", &Settings::default(), Some(&tone(10.0)), ONE_MODEL);
  s.send(Request::Start);
  let recording = s.update_where("recording", |u| u.status.state == Phase::Recording);
  let meter = recording.meter.expect("a meter while recording");
  poll("the tone's level", Duration::from_secs(10), || (meter.level() > 0.3).then_some(()));
  assert!(!meter.silent(), "the tone is sound");
  thread::sleep(Duration::from_millis(300));
  s.send(Request::Stop);
  let status = s.finished(1);
  assert_eq!(status.engine.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
  assert_eq!(status.engine.device.as_deref(), Some("fake"));
  let entry = kept(&s, &status);
  assert_eq!((entry.model.as_deref(), entry.language), (Some("parakeet-tdt-0.6b-v3-Q8_0"), None));
  assert!(entry.duration_ms > 0);

  for take in 1..=4 {
    match take {
      1 => {}
      2 => s.output.log().refuse = Some(NotPasted::FocusChanged),
      3 => {
        s.output.log().refuse = None;
        s.handle.edit_settings(|s| s.keep_in_clipboard = true);
        s.update_where("keep in clipboard", |u| u.settings.keep_in_clipboard);
      }
      _ => {
        s.handle.edit_settings(|s| s.auto_paste = false);
        s.update_where("auto-paste off", |u| !u.settings.auto_paste);
      }
    }

    let status = if take == 1 { status.clone() } else { dictate(&s, take) };
    let last = status.last.clone().unwrap();
    assert_eq!((last.outcome, last.text.as_str()), (Outcome::Done, "hello there"), "{take}");
    let entry = kept(&s, &status);
    assert_eq!((entry.text.as_str(), entry.busy), ("hello there", false));
    assert_eq!(entry.tried_model, entry.model);
    assert_eq!(last.pasted, entry.pasted);
    let log = s.output.log();
    let text = || last.text.clone();
    match take {
      1 => {
        assert!(last.pasted);
        assert_eq!(log.pasted, [(text(), 1, false)], "where the dictation started");
      }
      2 => {
        assert!(!last.pasted);
        assert!(last.not_pasted.as_deref().unwrap().contains("focus"), "{:?}", last.not_pasted);
        assert_eq!(log.pasted.len(), 1);
      }
      3 => {
        assert!(last.pasted);
        assert_eq!(log.pasted[1], (text(), 3, true), "pasted, and kept");
      }
      _ => {
        assert!(!last.pasted);
        assert_eq!(log.pasted.len(), 2, "not pasted");
        assert_eq!(log.copied, [text()]);
        assert!(last.not_pasted.as_deref().unwrap().contains("clipboard"), "{:?}", last.not_pasted);
      }
    }
  }
}

/// Another model in the settings replaces the engine once it is idle.
#[test]
fn another_model_replaces_the_engine_once_it_is_idle() {
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let models = Models("hello there", &["parakeet-v3", "whisper-tiny"]);
  let s = InProcess::start("session-model-switch", &settings, Some(&tone(10.0)), models);
  let first = dictate(&s, 1).engine.thread.expect("an engine thread");
  s.handle.edit_settings(|s| s.model = Some("whisper-tiny".into()));
  s.update_where("the new model", |u| u.settings.model.as_deref() == Some("whisper-tiny"));
  assert_eq!(s.status().engine.thread, None, "the old engine is gone");
  let second = dictate(&s, 2);
  assert_eq!(second.last.as_ref().unwrap().outcome, Outcome::Done);
  assert_eq!(second.engine.model.as_deref(), Some("whisper-tiny-Q8_0"));
  assert_ne!(second.engine.thread, Some(first));
}

/// Speech of which the model makes nothing is no success: the dictation ends as nothing
/// recognized, with the one language the model is for (shown once, not kept), nothing pasted nor
/// copied; the entry stays without text with its recording, and transcribing it again keeps it so
/// while the model still makes nothing of it, which the status says.
#[test]
fn speech_without_text_is_nothing_recognized_and_kept() {
  let models = Models("  ", &["parakeet-v2"]);
  let s = InProcess::start("session-no-text", &Settings::default(), Some(&tone(10.0)), models);
  let status = dictate(&s, 1);
  let last = status.last.clone().unwrap();
  assert_eq!((last.outcome, last.text.as_str(), last.pasted), (Outcome::NoText, "", false));
  let note = "parakeet-tdt-0.6b-v2 understands English only";
  assert_eq!(last.error.as_deref(), Some(note));
  let log = s.output.log();
  assert!(log.pasted.is_empty() && log.copied.is_empty(), "nothing pasted nor copied");
  drop(log);

  // Kept, with its recording, for another model.
  let entry = kept(&s, &status);
  assert_eq!((entry.text.as_str(), entry.model.as_deref(), entry.busy), ("", None, false));
  assert_eq!(entry.tried_model.as_deref(), Some("parakeet-tdt-0.6b-v2-Q8_0"));

  // Transcribed again: the same model makes nothing of it again; it stays.
  let again = retranscribed(&s, entry.id);
  assert_eq!(again, entry);
  assert!(s.history().audio_file(&again).is_some());
  let status = s.status();
  assert!(status.retranscribing.is_empty());
  let error = status.error.unwrap_or_default();
  assert!(error.contains("nothing recognized"), "{error}");
}

/// No model at all: the status says so from the start; it records anyway and keeps the dictation
/// with its recording, waiting for a model. Once one is downloaded, transcribing it again makes its
/// text.
#[test]
fn without_any_model_the_recording_waits_for_one() {
  let models = Models("hello there", &[]);
  let s = InProcess::start("session-no-model", &Settings::default(), Some(&tone(10.0)), models);
  let first = &s.first;
  assert_eq!(first.status.no_model, Some(NoModel::NoneDownloaded));
  let status = dictate(&s, 1);
  let last = status.last.clone().unwrap();
  assert_eq!((last.outcome, last.pasted), (Outcome::NoModel, false));
  assert_eq!(last.error.as_deref(), Some("no model is downloaded"));
  let entry = kept(&s, &status);
  assert_eq!((entry.text.as_str(), entry.tried_model.as_deref(), entry.busy), ("", None, false));
  assert!(s.output.log().pasted.is_empty());
  let refused = s.send(Request::Retranscribe { id: entry.id }).error.unwrap();
  assert!(refused.contains("no model"), "{refused}");
  assert_eq!(s.entry(entry.id).unwrap(), entry);

  // A model arrives: the entry gets its text, its recording stays.
  download(&s.hub, "parakeet-v3");
  let again = retranscribed(&s, entry.id);
  assert_eq!((again.text.as_str(), again.prev_text.as_deref()), ("hello there", None));
  assert!(s.history().audio_file(&again).is_some());
  assert_eq!(s.status().no_model, None);
}

/// The chosen model's file deleted behind the app's back while its engine has it loaded: the next
/// dictation finds it gone, that engine goes and the first model left transcribes in its place, the
/// status saying so; the choice stays, and once the model is back it transcribes again.
#[test]
fn a_chosen_model_gone_behind_the_apps_back_gives_way_to_the_next() {
  let settings = Settings { model: Some("parakeet-tdt-0.6b-v3".into()), ..Settings::default() };
  let models = Models("hello there", &["parakeet-v3", "whisper-tiny"]);
  let s = InProcess::start("session-model-gone", &settings, Some(&tone(10.0)), models);
  let first = dictate(&s, 1);
  assert_eq!(first.last.as_ref().unwrap().outcome, Outcome::Done);
  assert_eq!(first.engine.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
  let thread = first.engine.thread.expect("an engine thread");

  fs::remove_file(s.hub.find("parakeet-v3").unwrap()).unwrap();
  let second = dictate(&s, 2);
  let last = second.last.as_ref().unwrap();
  assert_eq!((last.outcome, last.text.as_str()), (Outcome::Done, "hello there"));
  assert_eq!(kept(&s, &second).model.as_deref(), Some("whisper-tiny-Q8_0"));
  assert_eq!(second.engine.model.as_deref(), Some("whisper-tiny-Q8_0"));
  assert_ne!(second.engine.thread, Some(thread), "the gone model's engine went");
  assert_eq!(second.no_model, None);
  let note = "parakeet-tdt-0.6b-v3 is not on disk: whisper-tiny transcribes instead";
  assert!(second.warnings.iter().any(|w| w == note), "{:?}", second.warnings);
  let (saved, _) = Settings::load(&s.home.path().join(settings::FILE));
  assert_eq!(saved.model.as_deref(), Some("parakeet-tdt-0.6b-v3"), "the choice stays");

  download(&s.hub, "parakeet-v3");
  let third = dictate(&s, 3);
  assert_eq!(kept(&s, &third).model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"), "back");
  assert!(!third.warnings.iter().any(|w| w == note), "{:?}", third.warnings);
}

/// The models on disk changed (the settings window deleted or downloaded one): the session looks
/// again at once, and an engine whose file is gone lets go of it without waiting for a recording.
#[test]
fn a_check_of_the_models_lets_a_deleted_one_go_and_finds_a_new_one() {
  let s =
    InProcess::start("session-check-model", &Settings::default(), Some(&tone(10.0)), ONE_MODEL);
  let first = dictate(&s, 1);
  assert_eq!(first.engine.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));

  fs::remove_file(s.hub.find("parakeet-v3").unwrap()).unwrap();
  s.handle.check_model();
  let none = Some(NoModel::NoneDownloaded);
  let gone = s.update_where("none left", |u| u.status.no_model == none);
  assert_eq!(gone.status.engine.model, None, "its engine went");

  download(&s.hub, "whisper-tiny");
  s.handle.check_model();
  s.update_where("a model again", |u| u.status.no_model.is_none());
  let tiny = dictate(&s, 2);
  assert_eq!(tiny.engine.model.as_deref(), Some("whisper-tiny-Q8_0"));

  // Without a choice the first of wavo's models downloaded transcribes: a new one before it
  // takes over at once.
  download(&s.hub, "parakeet-v3");
  s.handle.check_model();
  poll("the engine gone", Duration::from_secs(10), || {
    s.status().engine.model.is_none().then_some(())
  });
  let next = dictate(&s, 3);
  assert_eq!(next.engine.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
}

/// The chosen model's file deleted while it loads for a recording: that engine cannot serve the
/// recording, so the first model left transcribes it instead.
#[test]
fn a_model_gone_while_it_loads_for_a_recording_gives_way_at_once() {
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let models = Models("hello there", &["parakeet-v3", "whisper-tiny"]);
  let s = InProcess::start("session-model-gone-loading", &settings, Some(&tone(10.0)), models);
  s.faults.load(Fault::Hang);
  s.send(Request::Start);
  poll("a held load", Duration::from_secs(10), || (s.faults.hanging() == 1).then_some(()));
  fs::remove_file(s.hub.find("parakeet-v3").unwrap()).unwrap();
  wait_for_level(&s, 0.3);
  s.send(Request::Stop);
  let status = s.finished(1);
  assert_eq!(status.last.as_ref().unwrap().text, "hello there");
  assert_eq!(kept(&s, &status).model.as_deref(), Some("whisper-tiny-Q8_0"));
  s.faults.release();
}

/// The chosen model gone with none left: the choice stays, the status says it is not on disk, the
/// recording is kept; downloaded again, it is the choice again and transcribes the recording.
#[test]
fn a_chosen_model_gone_with_none_left_keeps_the_recording_until_it_is_back() {
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let models = Models("hello there", &[]);
  let s = InProcess::start("session-model-missing", &settings, Some(&tone(10.0)), models);
  let first = &s.first;
  let gone = NoModel::Gone { model: "parakeet-tdt-0.6b-v3".into() };
  assert_eq!(first.status.no_model, Some(gone.clone()));
  assert_eq!(first.settings.model.as_deref(), Some("parakeet-v3"), "the choice stays");
  s.send(Request::Start);
  let recording = s.update_where("recording", |u| u.status.state == Phase::Recording);
  assert_eq!(recording.status.no_model, Some(gone));
  wait_for_level(&s, 0.3);
  thread::sleep(Duration::from_millis(300));
  s.send(Request::Stop);
  let status = s.finished(1);
  let last = status.last.clone().unwrap();
  assert_eq!(last.outcome, Outcome::NoModel);
  assert_eq!(last.error.as_deref(), Some("parakeet-tdt-0.6b-v3 is not on disk"));
  let entry = kept(&s, &status);

  download(&s.hub, "parakeet-v3");
  let again = retranscribed(&s, entry.id);
  assert_eq!(again.text, "hello there");
  assert_eq!(again.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
  let (saved, _) = Settings::load(&s.home.path().join(settings::FILE));
  assert_eq!(saved.model.as_deref(), Some("parakeet-v3"), "still the choice");
}

/// A model that is there but does not load: the status says so as soon as the prewarm fails, while
/// recording, with nothing left loading; the recording is kept.
#[test]
fn a_model_that_does_not_load_says_so_and_keeps_the_recording() {
  let s =
    InProcess::start("session-load-fails", &Settings::default(), Some(&tone(10.0)), NO_MODELS);
  let broken = s.home.path().join("broken.gguf");
  fs::write(&broken, b"BROKEN").unwrap();
  let model = broken.display().to_string();
  s.handle.edit_settings(move |s| s.model = Some(model.clone()));
  s.update_where("the choice", |u| u.settings.model.is_some() && u.status.no_model.is_none());
  s.send(Request::Start);
  let failed = s.update_where("the load failed", |u| {
    matches!(u.status.no_model, Some(NoModel::LoadFailed { .. }))
  });
  let why = NoModel::LoadFailed { model: "broken".into(), error: "not a speech model".into() };
  assert_eq!(failed.status.no_model, Some(why));
  assert_eq!(failed.status.state, Phase::Recording, "it records on");
  assert_eq!(failed.status.engine.phase, EnginePhase::Stopped, "nothing left loading");
  wait_for_level(&s, 0.3);
  s.send(Request::Stop);
  let status = s.finished(1);
  let last = status.last.clone().unwrap();
  assert_eq!(last.outcome, Outcome::NoModel);
  let error = "broken could not be loaded: not a speech model";
  assert_eq!(last.error.as_deref(), Some(error));
  let entry = kept(&s, &status);
  assert_eq!(
    (entry.text.as_str(), entry.tried_model.as_deref(), entry.busy),
    ("", None, false),
    "a model that does not load has not tried it"
  );
}

/// The shell sees the engine's phase as it changes with nothing else happening: a model still
/// loading when its recording was cancelled shows loaded once it is, never stuck loading.
#[test]
fn the_shell_sees_a_load_end_after_the_recording_is_cancelled() {
  let s =
    InProcess::start("session-load-shown", &Settings::default(), Some(&tone(10.0)), ONE_MODEL);
  s.faults.load(Fault::Hang);
  s.send(Request::Start);
  s.update_where("loading", |u| u.status.engine.phase == EnginePhase::Loading);
  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  s.update_where("idle, loading", |u| {
    u.status.state == Phase::Idle && u.status.engine.phase == EnginePhase::Loading
  });
  s.faults.release();
  let loaded = s.update_where("loaded", |u| u.status.engine.phase == EnginePhase::Idle);
  assert_eq!((loaded.status.state, loaded.status.no_model), (Phase::Idle, None));
}

/// A session with the fake engine answering "hello there" and one model in its cache.
fn with_a_model(name: &str, deadlines: Deadlines) -> InProcess {
  let home = TempDir::new(name);
  InProcess::start_in(home, &Settings::default(), Some(&tone(10.0)), ONE_MODEL, deadlines)
}

/// Starts and stops a recording of the tone, then waits until the engine holds its run in
/// [`Fault::Hang`].
fn record_into_a_held_run(s: &InProcess) -> Status {
  s.send(Request::Start);
  wait_for_level(s, 0.3);
  s.send(Request::Stop);
  poll("a held run", Duration::from_secs(10), || {
    let status = s.status();
    (status.engine.phase == EnginePhase::Running && s.faults.hanging() == 1).then_some(status)
  })
}

/// A run that panics fails its dictation as an engine crash (the entry and its recording are
/// kept); the next dictation loads a fresh model and is done, and the failed one is transcribed
/// again.
#[test]
fn a_panic_in_a_run_fails_the_dictation_and_the_next_one_is_done() {
  let s = with_a_model("session-panic", Deadlines::default());
  let first = dictate(&s, 1);
  assert_eq!(first.last.as_ref().unwrap().outcome, Outcome::Done);
  s.faults.run(Fault::Panic);
  let status = dictate(&s, 2);
  let last = status.last.clone().unwrap();
  assert_eq!((last.outcome, last.pasted), (Outcome::Failed, false));
  let error = last.error.clone().unwrap_or_default();
  assert!(error.contains("crashed") && error.contains("panicked"), "{error}");
  assert_eq!(status.engine.phase, EnginePhase::Stopped, "the broken model is gone");
  let entry = kept(&s, &status);
  assert_eq!((entry.text.as_str(), entry.busy), ("", false));

  let third = dictate(&s, 3);
  assert_eq!(third.last.as_ref().unwrap().text, "hello there");
  assert_ne!(third.engine.thread, first.engine.thread, "a fresh engine thread");
  assert_eq!(s.entry(entry.id).unwrap(), entry, "the same model is not tried again by itself");
  let again = retranscribed(&s, entry.id);
  assert_eq!(again.text, "hello there");
  assert_eq!(s.output.log().pasted.len(), 2, "the failed one pasted nothing");
}

/// A run past its deadline fails its dictation as timed out; the next dictation is done on a fresh
/// engine thread while the stuck one is left to end.
#[test]
fn a_hung_run_times_out_and_the_next_dictation_is_done() {
  let deadline = Duration::from_millis(300);
  let deadlines =
    Deadlines { load: Duration::from_secs(10), request_min: deadline, request_factor: 0.0 };
  let s = with_a_model("session-hang", deadlines);
  s.faults.run(Fault::Hang);
  let status = dictate(&s, 1);
  let last = status.last.clone().unwrap();
  assert_eq!(last.outcome, Outcome::Failed);
  assert!(last.error.as_deref().is_some_and(|e| e.contains("timed out")), "{:?}", last.error);
  kept(&s, &status);
  let next = dictate(&s, 2);
  assert_eq!(next.last.as_ref().unwrap().text, "hello there");
  s.faults.release();
}

/// A cancel while the engine runs ends the dictation at once and keeps nothing; the run goes on for
/// nothing, and the next dictation has the same engine thread warm. One that needs the engine
/// while a cancelled run still holds it waits for that run instead of loading a second model.
#[test]
fn cancel_while_transcribing_drops_the_result_and_keeps_the_engine() {
  let s = with_a_model("session-cancel-run", Deadlines::default());
  let warm = dictate(&s, 1).engine.thread;
  assert!(warm.is_some());

  s.faults.run(Fault::Hang);
  let busy = record_into_a_held_run(&s);
  assert_eq!(busy.state, Phase::Transcribing);
  assert_eq!(s.send(Request::Toggle).result["state"], "transcribing", "toggle waits");
  let entry = busy.entry.expect("the entry is there while it is transcribed");
  let refused = s.send(Request::Delete { id: entry }).error.unwrap();
  assert!(refused.contains("being transcribed"), "{refused}");
  let times: Vec<Duration> = (0..50)
    .map(|_| {
      let started = Instant::now();
      assert_eq!(s.status().state, Phase::Transcribing);
      started.elapsed()
    })
    .collect();
  assert!(times.iter().all(|t| *t < Duration::from_millis(200)), "{times:?}");

  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  let last = s.finished(2).last.unwrap();
  assert_eq!((last.outcome, last.entry), (Outcome::Cancelled, None));
  assert_eq!((s.entries().len(), s.recordings()), (1, 1), "the first dictation's only");
  s.faults.release();
  poll("the end of the cancelled run", Duration::from_secs(10), || {
    (s.status().engine.phase == EnginePhase::Idle).then_some(())
  });
  let next = dictate(&s, 3);
  assert_eq!(next.last.as_ref().unwrap().text, "hello there");
  assert_eq!(next.engine.thread, warm, "the same engine thread");

  s.faults.run(Fault::Hang);
  record_into_a_held_run(&s);
  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  assert_eq!(s.finished(4).last.unwrap().outcome, Outcome::Cancelled);
  s.send(Request::Start);
  wait_for_level(&s, 0.3);
  s.send(Request::Stop);
  thread::sleep(Duration::from_millis(500));
  let waiting = s.status();
  assert_eq!(waiting.state, Phase::Transcribing, "it waits for the cancelled run");
  assert_eq!(waiting.engine.thread, warm);
  s.faults.release();
  let last = s.finished(5);
  assert_eq!(last.last.unwrap().text, "hello there");
  assert_eq!(last.engine.thread, warm, "no second model");
  assert_eq!(s.output.log().pasted.len(), 3, "the cancelled ones pasted nothing");
}

/// `ctl unload`: an idle engine goes at once and the next dictation loads the model on a new
/// thread; a busy one finishes its dictation first.
#[test]
fn unload_ends_the_engine_now_or_after_its_run() {
  let s = with_a_model("session-unload", Deadlines::default());
  let warm = dictate(&s, 1).engine.thread;
  assert!(warm.is_some());

  assert_eq!(s.send(Request::Unload).result["state"], "unloaded");
  assert_eq!(s.status().engine.phase, EnginePhase::Stopped);
  let next = dictate(&s, 2);
  assert_eq!(next.last.as_ref().unwrap().text, "hello there");
  assert!(next.engine.thread > warm, "a new engine thread");

  s.faults.run(Fault::Hang);
  record_into_a_held_run(&s);
  assert_eq!(s.send(Request::Unload).result["state"], "unloading");
  assert_eq!(s.status().engine.phase, EnginePhase::Running);
  s.faults.release();
  assert_eq!(s.finished(3).last.unwrap().text, "hello there");
  poll("the engine gone after its run", Duration::from_secs(10), || {
    (s.status().engine.phase == EnginePhase::Stopped).then_some(())
  });
}

/// `ctl unload` or a settings change while the model loads for a recording: the engine stays for
/// that recording (no second load) and goes once it is done.
#[test]
fn an_unload_during_a_recording_waits_for_its_dictation() {
  let s = with_a_model("session-unload-recording", Deadlines::default());
  for (take, unload) in [(1, true), (2, false)] {
    s.faults.load(Fault::Hang);
    s.send(Request::Start);
    let loading = s.update_where("loading", |u| u.status.engine.phase == EnginePhase::Loading);
    poll("a held load", Duration::from_secs(10), || (s.faults.hanging() == 1).then_some(()));
    if unload {
      assert_eq!(s.send(Request::Unload).result["state"], "unloading");
    } else {
      s.handle.edit_settings(move |s| s.unload_after_minutes = Some(take));
      s.update_where("the new setting", |u| u.settings.unload_after_minutes == Some(take));
    }

    assert_eq!(s.status().engine.thread, loading.status.engine.thread, "still loading for it");
    s.faults.release();
    wait_for_level(&s, 0.3);
    s.send(Request::Stop);
    let done = s.finished(take as u64);
    assert_eq!(done.last.unwrap().text, "hello there");
    assert_eq!(s.status().engine.thread, None, "gone after its dictation");
  }
}

/// A recording cancelled while its model loads (or one without speech): the engine prewarmed for
/// it unloads as the settings say, not minutes later.
#[test]
fn a_prewarm_without_a_job_unloads_after_the_settings_time() {
  let settings = Settings { unload_after_minutes: Some(0), ..Settings::default() };
  let s = InProcess::start("session-prewarm-unload", &settings, Some(&tone(10.0)), ONE_MODEL);
  s.send(Request::Start);
  s.update_where("loaded", |u| u.status.engine.phase == EnginePhase::Idle);
  assert_eq!(s.send(Request::Cancel).result["state"], "idle");
  s.update_where("the unload", |u| u.status.engine.phase == EnginePhase::Stopped);
}

/// The idle unload of the settings: the engine goes after its dictation.
#[test]
fn an_idle_engine_unloads_after_the_settings_time() {
  let settings = Settings { unload_after_minutes: Some(0), ..Settings::default() };
  let s = InProcess::start("session-idle-unload", &settings, Some(&tone(10.0)), ONE_MODEL);
  assert_eq!(dictate(&s, 1).last.unwrap().outcome, Outcome::Done);
  s.update_where("the unload", |u| u.status.engine.phase == EnginePhase::Stopped);
}

#[test]
fn hotkey_tap_latches_and_the_second_tap_stops() {
  let s = InProcess::start("session-hotkey", &Settings::default(), Some(&silence(0.2)), NO_MODELS);
  let t0 = Instant::now();
  s.handle.input(Input::KeyDown { at: t0, repeat: false });
  s.handle.input(Input::KeyDown { at: t0 + Duration::from_millis(60), repeat: true });
  s.handle.input(Input::KeyUp { at: t0 + Duration::from_millis(120) });
  assert_eq!(s.status().state, Phase::Recording, "a tap latches; autorepeat is ignored");
  thread::sleep(Duration::from_millis(300));
  s.handle.input(Input::KeyDown { at: Instant::now(), repeat: false });
  assert_eq!(s.finished(1).last.unwrap().outcome, Outcome::NoSpeech);
  s.handle.input(Input::KeyUp { at: Instant::now() });
  assert_eq!(s.status().state, Phase::Idle, "the release of the stopping tap");
}

#[test]
fn a_microphone_that_will_not_open_leaves_nothing_behind() {
  let s = InProcess::start("session-no-mic", &Settings::default(), None, NO_MODELS);
  let reply = s.send(Request::Start);
  let error = reply.error.unwrap();
  assert!(error.contains("could not start recording"), "{error}");
  let status = s.status();
  assert_eq!((status.state, status.last), (Phase::Idle, None));
  assert!(status.error.unwrap().contains("mic.wav"));
  assert!(s.entries().is_empty());
  assert_eq!(s.recordings(), 0, "no empty WAV file left");
  let dir = s.home.path().join(recordings::DIR);
  assert_eq!(fs::read_dir(dir).unwrap().count(), 0, "nor its day folder");
}

/// An entry with `text` and a recording of `pcm`, `age_ms` old, in the history of `home` (another
/// connection than the session's: WAL lets both write).
fn add_entry(home: &Path, age_ms: i64, text: &str, pcm: &[f32]) -> i64 {
  let history = History::open(home).unwrap();
  let created_at = now_ms() - age_ms;
  let (name, path) = history.recordings.new_file(created_at);
  write_wav(&path, pcm);
  let entry = NewEntry {
    created_at,
    duration_ms: pcm.len() as i64 * 1000 / i64::from(SAMPLE_RATE),
    audio_path: Some(&name),
    text,
    ..NewEntry::default()
  };
  history.db.insert_entry(&entry).unwrap()
}

#[test]
fn retention_runs_after_each_dictation() {
  let settings = Settings { history_retention: Retention::Count(1), ..Settings::default() };
  let s = InProcess::start("session-retention", &settings, Some(&silence(0.1)), NO_MODELS);
  // The first retention (in the background, on the empty history) is long done by then.
  thread::sleep(Duration::from_millis(300));
  let ids: Vec<i64> =
    (1..=3).map(|i| add_entry(s.home.path(), i * 1_000, "текст", &silence(0.1))).collect();
  // A dictation without speech keeps nothing, and the retention runs after it.
  s.send(Request::Start);
  thread::sleep(Duration::from_millis(150));
  s.send(Request::Stop);
  s.finished(1);
  let entries = s.entries();
  assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), [ids[0]], "the newest stays");
  assert_eq!(s.recordings(), 1, "older recordings are deleted with their entries");
}

#[test]
fn retranscribe_without_speech_and_unknown_entries() {
  let s = InProcess::start("session-again", &Settings::default(), Some(&silence(0.1)), NO_MODELS);
  // After the start's recovery (the first update): it would take a WAV written meanwhile for an
  // orphan.
  // An entry without text whose recording has no speech: nothing to keep, it goes.
  let failed = add_entry(s.home.path(), 1_000, "", &silence(0.2));
  let reply = s.send(Request::Retranscribe { id: failed });
  assert_eq!(reply.result["state"], "deleted");
  assert_eq!(s.entry(failed), None);
  assert_eq!(s.recordings(), 0);

  // One with text stays as it is.
  let kept = add_entry(s.home.path(), 2_000, "текст", &silence(0.2));
  let error = s.send(Request::Retranscribe { id: kept }).error.unwrap();
  assert!(error.contains("no speech") && error.contains("stays"), "{error}");
  assert_eq!(s.entry(kept).unwrap().text, "текст");
  let unknown = s.send(Request::Retranscribe { id: kept + 1 }).error.unwrap();
  assert!(unknown.contains("no history entry"));
  fs::remove_file(s.history().audio_file(&s.entries()[0]).unwrap()).unwrap();
  assert!(s.send(Request::Retranscribe { id: kept }).error.unwrap().contains("gone"));
}

/// Transcribing an entry with text again fails: it keeps its text and model, and the status says
/// what went wrong. Then new text replaces it, an undo puts it back, and a second undo the new one
/// again.
#[test]
fn a_retranscription_replaces_the_text_only_with_text_and_undoes() {
  let s = with_a_model("session-again", Deadlines::default());
  let id = add_entry(s.home.path(), 1_000, "текст", &tone(1.0));
  s.faults.run(Fault::Panic);
  let entry = retranscribed(&s, id);
  assert_eq!((entry.text.as_str(), entry.model, entry.prev_text), ("текст", None, None));
  assert_eq!(entry.tried_model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
  let error = s.status().error.unwrap_or_default();
  assert!(
    error.starts_with(&format!("entry {id} could not")) && error.contains("crashed"),
    "{error}"
  );

  let undo = || s.send(Request::UndoTranscribe { id });
  assert!(undo().error.unwrap().contains("no text before"), "nothing replaced yet");
  let again = retranscribed(&s, id);
  assert_eq!((again.text.as_str(), again.prev_text.as_deref()), ("hello there", Some("текст")));
  assert_eq!(again.model.as_deref(), Some("parakeet-tdt-0.6b-v3-Q8_0"));
  assert_eq!(undo().result["state"], "undone");
  let undone = s.entry(id).unwrap();
  assert_eq!((undone.text.as_str(), undone.model), ("текст", None));
  assert_eq!(undone.prev_text.as_deref(), Some("hello there"));
  assert_eq!(undo().result["state"], "undone");
  assert_eq!(s.entry(id).unwrap(), again);
  assert!(s.output.log().pasted.is_empty(), "nothing transcribed again is pasted");
}

/// Dictations without a model are kept without text; once a model is there (a choice made here)
/// they are transcribed by themselves, the newest first, nothing pasted.
#[test]
fn entries_without_text_are_transcribed_once_a_model_is_there() {
  let models = Models("hello there", &[]);
  let s = InProcess::start("session-untried", &Settings::default(), Some(&tone(10.0)), models);
  let ids: Vec<i64> = (1..=2).map(|take| kept(&s, &dictate(&s, take)).id).collect();
  let gone = add_entry(s.home.path(), 3_000, "", &silence(0.2));
  let with_text = add_entry(s.home.path(), 4_000, "текст", &tone(1.0));

  download(&s.hub, "parakeet-v3");
  s.handle.edit_settings(|s| s.model = Some("parakeet-v3".into()));
  let model = Some("parakeet-tdt-0.6b-v3-Q8_0");
  for id in &ids {
    let entry = poll("transcribed", Duration::from_secs(10), || {
      s.entry(*id).filter(|e| !e.text.is_empty() && !e.busy)
    });
    assert_eq!((entry.text.as_str(), entry.model.as_deref()), ("hello there", model));
  }

  // No speech: nothing to keep.
  poll("the one without speech gone", Duration::from_secs(10), || {
    s.entry(gone).is_none().then_some(())
  });
  assert_eq!(s.entry(with_text).unwrap().tried_model, None, "one with text is left alone");
  let log = s.output.log();
  assert!(log.pasted.is_empty() && log.copied.is_empty(), "nothing pasted nor copied");
}

/// A model that fails to load for the entries without text has not tried them: they wait until it
/// loads (here for a dictation), and the retention spares them until they are transcribed, then
/// keeps the newest entry only.
#[test]
fn entries_without_text_wait_for_a_model_that_loads() {
  let settings = Settings { history_retention: Retention::Count(1), ..Settings::default() };
  let models = Models("hello there", &[]);
  let s = InProcess::start("session-untried-load", &settings, Some(&tone(10.0)), models);
  let ids: Vec<i64> = (1..=2).map(|take| kept(&s, &dictate(&s, take)).id).collect();
  assert_eq!(s.entries().len(), 2, "no model: both spared");

  download(&s.hub, "parakeet-v3");
  s.faults.load(Fault::Fail("out of memory"));
  s.handle.edit_settings(|s| s.model = Some("parakeet-v3".into()));
  let failed = poll("a load that failed", Duration::from_secs(10), || {
    let status = s.status();
    let failed = matches!(status.no_model, Some(NoModel::LoadFailed { .. }));
    (failed && status.retranscribing.is_empty()).then_some(status)
  });
  assert_eq!(failed.error, None);
  for id in &ids {
    let entry = s.entry(*id).unwrap();
    assert_eq!((entry.text.as_str(), entry.tried_model, entry.busy), ("", None, false));
  }

  let last = kept(&s, &dictate(&s, 3));
  poll("the others transcribed, the retention after them", Duration::from_secs(10), || {
    (entry_ids(s.home.path()) == [last.id]).then_some(())
  });
}

/// A model that makes nothing of the entries without text tries each once.
#[test]
fn a_model_that_makes_nothing_tries_each_entry_once() {
  let models = Models("  ", &[]);
  let s = InProcess::start("session-untried-once", &Settings::default(), Some(&tone(10.0)), models);
  let id = kept(&s, &dictate(&s, 1)).id;
  download(&s.hub, "parakeet-v3");
  s.handle.edit_settings(|s| s.model = Some("parakeet-v3".into()));
  let tried = poll("tried", Duration::from_secs(10), || {
    s.entry(id).filter(|e| e.tried_model.is_some() && !e.busy)
  });
  assert_eq!(
    (tried.text.as_str(), tried.tried_model.as_deref()),
    ("", Some("parakeet-tdt-0.6b-v3-Q8_0"))
  );
  assert_eq!(s.status().error, None, "no news of a transcription not asked for");

  // Another dictation (nothing recognized either) and a settings change find the same model: no
  // second try.
  assert_eq!(dictate(&s, 2).last.unwrap().outcome, Outcome::NoText);
  s.handle.edit_settings(|s| s.model = Some("parakeet-tdt-0.6b-v3".into()));
  s.update_where("the new choice", |u| u.settings.model.as_deref() == Some("parakeet-tdt-0.6b-v3"));
  thread::sleep(Duration::from_millis(300));
  let status = s.status();
  assert!(status.retranscribing.is_empty(), "{:?}", status.retranscribing);
  assert_eq!(s.entry(id).unwrap(), tried);
}

#[test]
fn delete_takes_the_entry_and_its_recording_and_moves_the_history() {
  let s = InProcess::start("session-delete", &Settings::default(), Some(&silence(0.1)), NO_MODELS);
  // After the start's recovery (the first update): it would take a WAV written meanwhile for an
  // orphan.
  let first = s.first.history;
  let id = add_entry(s.home.path(), 1_000, "текст", &silence(0.1));
  let wav = s.history().audio_file(&s.entries()[0]).unwrap();

  let reply = s.send(Request::Delete { id });
  assert_eq!(reply.error, None);
  assert_eq!(reply.result["state"], "deleted");
  s.update_where("the deletion", |u| u.history > first);
  assert!(s.entries().is_empty() && !wav.exists());
  assert!(s.send(Request::Delete { id }).error.unwrap().contains("no history entry"));
  // An entry being transcribed cannot be deleted: `cancel_while_transcribing_…`.
  assert!(s.send(Request::Quit).error.unwrap().contains("not handled"));
}

/// A session on its own, without an observer, with no models.
fn bare(home: &TempDir) -> Session {
  let config = Config {
    home: home.path().to_owned(),
    hub: Hub::at(home.path().join("hub")),
    loader: Arc::new(FakeModels::new("")),
    deadlines: Deadlines::default(),
    fake_mic: None,
  };
  Session::start(config, FakeOutput::default()).unwrap()
}

/// A crash during a transcription leaves its entry, without text, and one while recording a
/// recording without an entry: both are entries without text after the start, which transcribes
/// nothing by itself even with a model there. A recording without speech goes.
#[test]
fn start_recovers_what_a_crash_left() {
  let home = TempDir::new("session-recover");
  let history = History::open(home.path()).unwrap();
  let (name, path) = history.recordings.new_file(now_ms() - 2_000);
  write_wav(&path, &tone(1.0));
  let new = NewEntry { audio_path: Some(&name), busy: true, ..NewEntry::default() };
  let id = history.db.insert_entry(&new).unwrap();
  let (_, orphan) = history.recordings.new_file(now_ms() - 1_000);
  write_wav(&orphan, &tone(1.0));
  let (_, silent) = history.recordings.new_file(now_ms());
  write_wav(&silent, &silence(1.0));
  drop(history);

  let s = InProcess::start_in(home, &Settings::default(), None, ONE_MODEL, Deadlines::default());
  let entries = s.entries();
  assert_eq!(entries.len(), 2);
  assert_eq!((entries[0].id, entries[0].busy, entries[0].text.as_str()), (id, false, ""));
  assert_eq!((entries[1].busy, entries[1].text.as_str()), (false, ""));
  assert!(!silent.exists());
  thread::sleep(Duration::from_millis(300));
  assert!(s.status().retranscribing.is_empty());
  assert_eq!(s.entries(), entries, "nothing transcribed at the start");
}

#[test]
fn the_observer_sees_each_change_once() {
  let s = InProcess::start("session-observer", &Settings::default(), Some(&tone(10.0)), NO_MODELS);
  let first = &s.first;
  assert_eq!((first.status.state, &first.settings), (Phase::Idle, &Settings::default()));
  assert!(first.meter.is_none());
  // Questions change nothing: no update.
  s.status();
  s.status();
  assert!(s.updates.try_recv().is_err(), "an update without a change");
  s.send(Request::Start);
  s.update_where("recording", |u| u.status.state == Phase::Recording && u.meter.is_some());
  s.send(Request::Stop);
  let idle = s.update_where("the end", |u| u.status.state == Phase::Idle);
  assert_eq!(idle.status.last.unwrap().take, 1);
  assert!(idle.meter.is_none());
}

#[test]
fn new_settings_are_saved_and_used() {
  let s =
    InProcess::start("session-settings", &Settings::default(), Some(&silence(0.5)), NO_MODELS);
  s.handle.edit_settings(|s| s.activation = Activation::Toggle);
  let toggle = Settings { activation: Activation::Toggle, ..Settings::default() };
  let update = s.update_where("new settings", |u| u.settings == toggle);
  assert_eq!(update.status.activation, Activation::Toggle);
  let saved = Settings::load(&s.home.path().join(settings::FILE));
  assert_eq!(saved, (toggle, None), "saved to settings.json");
  // Toggle: a long press latches, its release does not stop.
  let t0 = Instant::now();
  s.handle.input(Input::KeyDown { at: t0, repeat: false });
  s.handle.input(Input::KeyUp { at: t0 + Duration::from_secs(1) });
  assert_eq!(s.status().state, Phase::Recording);
}

/// The first retention, in the background; without a model it spares the entry without text.
#[test]
fn the_first_cleanup_runs_in_the_background() {
  let home = TempDir::new("session-first-cleanup");
  let ten_days = 10 * 24 * 60 * 60 * 1000;
  for i in 0..40 {
    add_entry(home.path(), ten_days + i, "текст", &silence(0.01));
  }

  let untranscribed = add_entry(home.path(), ten_days, "", &tone(1.0));
  let settings = Settings { history_retention: Retention::Days3, ..Settings::default() };
  settings.save(&home.path().join(settings::FILE)).unwrap();
  let session = bare(&home);
  status_of(session.handle().request(Request::Status));
  poll("the old entries gone", Duration::from_secs(10), || {
    (entry_ids(home.path()) == [untranscribed]).then_some(())
  });
  assert_eq!(recordings_in(home.path()), 1, "their recordings too");
}

// MARK: The headless app with a real model

/// `wavo-app daemon` in its own temp home, recording fixture `mic`, GigaAM for a model.
struct Daemon {
  home: TempDir,
  child: Option<Child>,
  socket: PathBuf,
  lines: Receiver<String>,
  log: PathBuf,
}

impl Daemon {
  fn start(name: &str, mic: &str) -> Self {
    let home = TempDir::new(name);
    let settings =
      Settings { model: Some(model_ru().to_string_lossy().into_owned()), ..Settings::default() };
    settings.save(&home.path().join(settings::FILE)).unwrap();
    let log = home.path().join("daemon.log");
    let mut child = Process::new(app_exe())
      .arg("daemon")
      .env("WAVO_HOME", home.path())
      .env("WAVO_FAKE_MIC", fixture_wav(mic))
      .env("WAVO_LOG", "warn,wavo_app=info")
      .stdout(Stdio::piped())
      .stderr(fs::File::create(&log).unwrap())
      .spawn()
      .unwrap();
    let (tx, lines) = unbounded();
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
      for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let _ = tx.send(line);
      }
    });

    let socket = ctl::socket_path(home.path()).unwrap();
    let daemon = Self { home, child: Some(child), socket, lines, log };
    poll("daemon", Duration::from_secs(10), || ctl::request(&daemon.socket, &Request::Status).ok());
    daemon
  }

  fn request(&self, r: Request) -> Reply {
    ctl::request(&self.socket, &r).unwrap()
  }

  fn status(&self) -> Status {
    status_of(self.request(Request::Status))
  }

  /// `wavo-app ctl <args>` as its own process.
  fn ctl(&self, args: &[&str]) -> Output {
    let mut ctl = Process::new(app_exe());
    ctl.arg("ctl").args(args).env("WAVO_HOME", self.home.path()).output().unwrap()
  }

  fn ctl_ok(&self, args: &[&str]) -> String {
    let out = self.ctl(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "wavo-app ctl {args:?}: {stderr}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
  }

  fn finished(&self, take: u64) -> Status {
    finished(take, || self.status())
  }

  fn entry(&self, id: i64) -> Entry {
    History::open(self.home.path()).unwrap().db.get_entry(id).unwrap().unwrap()
  }

  /// Records fixture `mic` (its length plus a margin) between `start` and `stop` over `wavo-app
  /// ctl`.
  fn dictate(&self, mic: &str) -> Instant {
    assert_eq!(self.ctl_ok(&["start"]), "recording");
    thread::sleep(Duration::from_secs_f64(fixture(mic).seconds() + 0.4));
    let stopped = Instant::now();
    let state = self.ctl_ok(&["stop"]);
    assert!(["transcribing", "pasting", "idle"].contains(&state.as_str()), "{state}");
    stopped
  }

  fn no_output_for(&self, wait: Duration) {
    if let Ok(line) = self.lines.recv_timeout(wait) {
      panic!("unexpected output: {line:?}");
    }
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    let Some(mut child) = self.child.take() else { return };
    let _ = ctl::request(&self.socket, &Request::Quit);
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
      thread::sleep(Duration::from_millis(10));
    }

    let _ = child.kill(); // if it did not quit
    let _ = child.wait();
    if thread::panicking() {
      eprintln!("--- daemon log\n{}", fs::read_to_string(&self.log).unwrap_or_default());
    }
  }
}

fn median(mut times: Vec<Duration>) -> Duration {
  times.sort();
  times[times.len() / 2]
}

#[test]
#[ignore = "needs models and the release binary: make test-models"]
fn daemon_full_cycle_via_ctl() {
  let d = Daemon::start("daemon-cycle", "ru-short");
  let reference = fixture("ru-short");
  // Warm up: the first dictation also loads the model.
  for take in 1..=2 {
    let stopped = d.dictate("ru-short");
    let line = d.lines.recv_timeout(Duration::from_secs(60)).expect("text on stdout");
    let to_text = stopped.elapsed();
    let status = d.finished(take);
    let last = status.last.unwrap();
    eprintln!("take {take}: ctl stop → text on stdout {to_text:.0?}: {line}");
    assert_eq!((last.outcome, last.pasted), (Outcome::Done, true));
    assert_eq!(line, last.text);
    let w = wer(&reference.text, &line);
    assert!(w <= MAX_WER, "WER {:.1}%: {line}", w * 100.0);
    let entry = d.entry(last.entry.unwrap());
    let kept = (entry.busy, entry.pasted, entry.text.as_str());
    assert_eq!(kept, (false, true, line.as_str()));
    assert_eq!(entry.model.as_deref(), Some("gigaam-v3-e2e-rnnt-Q8_0"));
    let recorded = entry.duration_ms as f64 / 1000.0;
    assert!(recorded >= reference.seconds(), "{recorded} s recorded");
    let audio = History::open(d.home.path()).unwrap().audio_file(&entry);
    assert!(audio.is_some(), "the recording is kept");
  }

  // `toggle` works the same way.
  assert_eq!(d.ctl_ok(&["toggle"]), "recording");
  assert_eq!(d.status().state, Phase::Recording);
  thread::sleep(Duration::from_secs_f64(reference.seconds() + 0.4));
  assert_eq!(d.ctl_ok(&["toggle"]), "transcribing");
  d.lines.recv_timeout(Duration::from_secs(10)).unwrap();
  assert_eq!(d.finished(3).last.unwrap().outcome, Outcome::Done);

  // Round trips of `wavo-app ctl status`: over the socket from here, and as a process.
  let socket: Vec<Duration> = (0..200)
    .map(|_| {
      let started = Instant::now();
      d.status();
      started.elapsed()
    })
    .collect();
  let process: Vec<Duration> = (0..20)
    .map(|_| {
      let started = Instant::now();
      d.ctl_ok(&["status"]);
      started.elapsed()
    })
    .collect();
  let (socket, process) = (median(socket), median(process));
  eprintln!(
    "ctl status round trip: socket median {socket:.2?}, `wavo-app ctl` median {process:.2?}"
  );
  assert!(socket < Duration::from_millis(20), "{socket:?}");
  assert!(process < Duration::from_millis(200), "{process:?}");
}

#[test]
#[ignore = "needs models and the release binary: make test-models"]
fn daemon_cancel_while_recording() {
  let d = Daemon::start("daemon-cancel", "ru-short");
  assert_eq!(d.ctl_ok(&["start"]), "recording");
  thread::sleep(Duration::from_secs(1));
  assert_eq!(d.ctl_ok(&["cancel"]), "idle");
  let last = d.finished(1).last.unwrap();
  assert_eq!((last.outcome, last.entry), (Outcome::Cancelled, None));
  assert_eq!((entries_in(d.home.path()).len(), recordings_in(d.home.path())), (0, 0));
  d.no_output_for(Duration::from_millis(300));
}

#[test]
#[ignore = "needs models and the release binary: make test-models"]
fn daemon_silence_is_no_speech() {
  let d = Daemon::start("daemon-silence", "silence");
  assert_eq!(d.ctl_ok(&["start"]), "recording");
  thread::sleep(Duration::from_secs(2));
  assert_eq!(d.ctl_ok(&["stop"]), "transcribing");
  let status = d.finished(1);
  let last = status.last.unwrap();
  assert_eq!((last.outcome, last.entry), (Outcome::NoSpeech, None));
  let kept = (entries_in(d.home.path()).len(), recordings_in(d.home.path()));
  assert_eq!(kept, (0, 0), "nothing is kept");
  assert_eq!(status.engine.progress, None, "no job reached the engine");
  d.no_output_for(Duration::from_millis(300));
}

#[test]
#[ignore = "needs models and the release binary: make test-models"]
fn daemon_second_instance_exits_and_quit_ends_the_first() {
  let d = Daemon::start("daemon-single", "silence");
  let started = Instant::now();
  let second =
    Process::new(app_exe()).arg("daemon").env("WAVO_HOME", d.home.path()).output().unwrap();
  let took = started.elapsed();
  let stderr = String::from_utf8_lossy(&second.stderr);
  assert!(second.status.success(), "{stderr}");
  assert!(stderr.contains("already running"), "{stderr}");
  assert!(took < Duration::from_secs(2), "{took:?}");
  assert_eq!(d.status().state, Phase::Idle, "the first instance goes on");

  // Quit removes the socket; then `wavo-app ctl` says nothing is running.
  assert_eq!(d.ctl_ok(&["quit"]), "quitting");
  poll("socket removed", Duration::from_secs(5), || (!d.socket.exists()).then_some(()));
  let out = d.ctl(&["status"]);
  assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
}

/// An entry an import is adding (busy in the history, its recording maybe not copied yet) is
/// neither transcribed again nor deleted.
#[test]
fn entries_being_imported_are_left_alone() {
  let s = with_a_model("session-importing", Deadlines::default());
  let entry = Entry { created_at: 1_000, audio_path: Some("1000.wav".into()), ..Entry::default() };
  let id = s.history().db.insert_imported(&[entry]).unwrap()[0];
  for request in [Request::Retranscribe { id }, Request::Delete { id }] {
    let refused = s.send(request).error.unwrap();
    assert!(refused.contains("being imported"), "{refused}");
  }

  assert!(s.entry(id).is_some());
  s.history().db.mark_imported(&[id]).unwrap();
  assert_eq!(s.send(Request::Delete { id }).result["state"], "deleted");
}
