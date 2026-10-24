// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Command-line tools that run without the GUI: `wavo-app transcribe`, `devices`, `record`,
//! `export`, `import`, the headless `daemon` and its `ctl` client, the settings window's shots
//! (`settings`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{RecvTimeoutError, bounded};
use tracing::{info, warn};
use wavo::hub::Hub;

use crate::audio::capture::{self, Source};
use crate::audio::devices::{self, Choice};
use crate::audio::recorder::Recorder;
use crate::audio::vad::Clip;
use crate::audio::{SAMPLE_RATE, vad, wav};
use crate::ctl::{self, Claim, Reply, Request};
use crate::engine::{Engine, EngineConfig};
use crate::history::Retention;
use crate::session::output::Stdout;
use crate::session::{self, Session};
use crate::settings::{self, Settings};
use crate::transfer::{self, Report, archive, handy};
use crate::window::downloads::Download;
use crate::window::{self, Access, Page, Prefs, Shell};

/// `wavo-app transcribe`: the dictation pipeline on a file, without capture and paste. The text
/// goes to stdout, timings and warnings to stderr. Without speech nothing is printed and the engine
/// is not started. Model and language default to the settings; `--language auto` leaves it to the
/// model.
pub fn transcribe(wav: &Path, model: Option<&str>, language: Option<&str>) -> Result<()> {
  let (settings, warning) = Settings::load(&crate::home().join(settings::FILE));
  if let Some(w) = warning {
    warn!("{w}");
  }

  // As the app does, the settings' model gone from the disk gives way to the first one found.
  let hub = Hub::new();
  let model = match (model, settings.model.as_deref()) {
    (Some(model), _) => hub.resolve(model)?,
    (None, Some(chosen)) => match hub.resolve(chosen) {
      Ok(path) => path,
      Err(e) => {
        warn!("the settings' model: {e}");
        first_model(&hub)?
      }
    },
    (None, None) => first_model(&hub)?,
  };

  let language = language.map_or_else(|| settings.engine_language(), settings::engine_language);

  let clip = Clip::analyze(wav::read(wav)?);
  let Some(input) = clip.engine_input() else {
    warn!("no speech in {} ({:.1} s): the engine is not called", wav.display(), clip.seconds());
    return Ok(());
  };

  let engine = Engine::start(EngineConfig::new(model))?;
  let seconds = input.seconds();
  let result = engine.transcribe(input.pcm, language).wait()?;
  let status = engine.status();
  info!(
    "{:.1} s of audio ({seconds:.1} s to the engine in {} piece(s)) in {:.2?} on {} (engine thread \
     {}, start to loaded {:.2?})",
    clip.seconds(),
    result.pieces,
    result.elapsed,
    result.device,
    status.thread.map_or("-".into(), |n| n.to_string()),
    status.last_load.unwrap_or_default(),
  );
  println!("{}", result.text);
  Ok(())
}

/// The first of wavo's models downloaded to `hub`.
fn first_model(hub: &Hub) -> Result<PathBuf> {
  let first = hub.downloaded().into_iter().next();
  let (info, path) = first.with_context(|| {
    format!("no speech models in {}: download one with `wavo pull <name>`", hub.root().display())
  })?;

  info!("using {}, the first model found", info.name);
  Ok(path)
}

/// `wavo-app record`: `seconds` of the microphone (or `WAVO_FAKE_MIC`) into `out`, then a summary
/// on stderr. A device error ends the recording early; what was recorded is kept.
pub fn record(length: Duration, device: Option<&str>, out: &Path) -> Result<()> {
  let source = Source::new(capture::fake_mic(), device.map(Choice::id_or_name));
  let recorder = Recorder::start(&source, out)?;
  let fallback = recorder.warning().map(str::to_owned);
  if let Some(w) = &fallback {
    warn!("{w}");
  }

  info!("recording {length:.1?} from {:?} into {}", recorder.device(), out.display());

  // Samples the level meter at its own rate (~30 Hz) until the time is up or the device fails.
  let started = Instant::now();
  let mut meter = 0.0f32;
  while let Some(left) = length.checked_sub(started.elapsed()) {
    match recorder.failures().recv_timeout(left.min(Duration::from_millis(33))) {
      Ok(failure) => {
        warn!("the recording stopped early: {failure}");
        break;
      }
      // The audio thread is gone without a word (it panicked): `stop` says so.
      Err(RecvTimeoutError::Disconnected) => break,
      Err(RecvTimeoutError::Timeout) => {}
    }

    meter = meter.max(recorder.level());
  }

  let stopping = Instant::now();
  let rec = recorder.stop()?;
  let stop = stopping.elapsed();
  for w in rec.warnings.iter().skip(usize::from(fallback.is_some())) {
    warn!("{w}");
  }

  let pcm = &rec.clip.pcm;
  let rms = (pcm.iter().map(|&s| (s * s) as f64).sum::<f64>() / pcm.len().max(1) as f64).sqrt();
  let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
  let voiced = rec.clip.voiced_frames();
  info!(
    "{}: {:.2} s from {:?}, RMS {:.1} dBFS (meter max {:.1}), peak {:.1} dBFS, voiced {:.2} s, mic \
     open {}, stop {stop:.1?}",
    rec.wav.display(),
    rec.clip.seconds(),
    rec.device,
    dbfs(rms as f32),
    dbfs(meter),
    dbfs(peak),
    voiced as f64 * vad::FRAME as f64 / SAMPLE_RATE as f64,
    rec.open_latency.map_or("-".into(), |d| format!("{d:.1?}")),
  );
  if !rec.saved {
    bail!("{} does not hold the whole recording", rec.wav.display());
  }

  Ok(())
}

/// `--seconds`: a positive number of seconds.
pub fn seconds(s: &str) -> Result<Duration, String> {
  let length = s.parse::<f64>().ok().and_then(|s| Duration::try_from_secs_f64(s).ok());
  length
    .filter(|l| !l.is_zero())
    .ok_or_else(|| format!("{s:?} is not a positive number of seconds"))
}

fn dbfs(level: f32) -> f32 {
  20.0 * level.max(1e-10).log10()
}

/// `wavo-app daemon`: dictation without a GUI, controlled by `wavo-app ctl`; each text is a line on
/// stdout. A second one for the same home leaves the first alone and exits.
pub fn daemon() -> Result<()> {
  let config = session::Config::from_env();
  let home = config.home.clone();
  std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
  let socket = ctl::socket_path(&home)?;
  let server = match ctl::claim(&home).with_context(|| format!("claim {}", socket.display()))? {
    Claim::Server(server) => server,
    Claim::Running => {
      eprintln!("wavo-app is already running for {}", home.display());
      return Ok(());
    }
  };

  let session = Session::start(config, Stdout)?;
  let handle = session.handle();
  let (quit, quitting) = bounded(1);
  let serving = server.spawn(move |request| match request {
    Request::Quit => {
      let _ = quit.try_send(());
      Reply::ok(serde_json::json!({ "state": "quitting" }))
    }
    Request::OpenSettings | Request::OpenHistory | Request::Autostart { .. } => {
      Reply::error("the daemon has no GUI")
    }
    request => handle.request(request),
  })?;

  info!("wavo-app daemon for {}: control socket {}", home.display(), socket.display());
  let _ = quitting.recv();
  // The session first: the lock goes with the socket, and the next instance's recovery must not
  // find this one's recording or transcription unfinished.
  drop(session);
  drop(serving);
  info!("wavo-app daemon: quit");
  Ok(())
}

/// `wavo-app ctl`: one request to the running app. Commands print the state they leave, `status`
/// the whole status as JSON.
pub fn ctl(request: Request) -> Result<()> {
  let home = crate::home();
  let Ok(socket) = ctl::socket_path(&home) else {
    bail!("wavo-app is not running (no {})", home.display())
  };

  let reply = ctl::request(&socket, &request)?;

  if let Some(error) = reply.error {
    bail!("{error}");
  }

  match (&request, reply.result.get("state").and_then(|s| s.as_str())) {
    (Request::Status, _) | (_, None) => {
      println!("{}", serde_json::to_string_pretty(&reply.result)?)
    }
    (_, Some(state)) => println!("{state}"),
  }

  Ok(())
}

/// `wavo-app settings --shot <png>`: the settings window on `page`, rendered headless by nimbo's
/// harness (`harness`: its flags) over the saved settings, the models in the Hugging Face cache
/// and the history; whatever it would change goes nowhere.
pub fn settings(page: Page, harness: &[String]) -> Result<()> {
  if !harness.iter().any(|a| a == "--shot") {
    bail!("it renders headless only (--shot <png>); `wavo-app ctl open-settings` opens the window");
  }

  let home = crate::home();
  let (saved, warning) = Settings::load(&home.join(settings::FILE));
  if let Some(w) = warning {
    warn!("{w}");
  }

  let mut prefs = Prefs::new(Box::new(Headless), saved, Hub::new(), &home);
  prefs.page = page;
  // A few frames first: the window takes its height from the one before.
  let settle = |h: &mut nimbo::Harness<Prefs>| {
    for _ in 0..3 {
      h.frame();
    }
  };
  nimbo::run_args(window::config(page), prefs, settle, harness);
  Ok(())
}

/// The settings window's shell for a shot: it shows what is there and does nothing.
struct Headless;

impl Shell for Headless {
  fn edit(&self, _: Box<dyn FnOnce(&mut Settings) + Send>) {}
  fn pause_hotkey(&self, _: bool) {}

  fn devices(&self) -> Vec<devices::Info> {
    devices::list()
  }

  fn download(&self, _: &'static wavo::hub::ModelInfo) {}
  fn cancel_download(&self, _: &str) {}

  fn download_state(&self, _: &str) -> Option<Download> {
    None
  }

  fn delete_model(&self, _: &'static wavo::hub::ModelInfo) {}
  fn check_model(&self) {}
  fn retranscribe(&self, _: i64) {}
  fn delete_entry(&self, _: i64) {}

  fn autostart(&self) -> Result<nimbo::autostart::State, String> {
    Err(String::new())
  }

  fn set_autostart(&self, _: bool) {}

  fn access(&self) -> Access {
    Access { accessibility: nimbo::permissions::accessibility(false), microphone: None }
  }

  fn folder_size(&self, dir: PathBuf, done: Box<dyn FnOnce(u64) + Send>) {
    done(window::folder_size(&dir));
  }

  fn open_pane(&self, _: nimbo::permissions::Pane) {}
  fn open_url(&self, _: &str) {}
  fn reveal(&self, _: &Path) {}

  fn handy(&self, done: Box<dyn FnOnce(Option<handy::Summary>) + Send>) {
    done(handy::found());
  }

  fn import_handy(&self, _: PathBuf, _: Box<dyn Fn(window::data::Import) + Send>) {}

  fn export(&self, _: PathBuf, _: Box<dyn FnOnce(Result<archive::Exported, String>) + Send>) {}
}

/// `wavo-app export`: the data home into the archive `file`, a summary on stdout.
pub fn export(file: &Path) -> Result<()> {
  let done = archive::export(&crate::home(), file)?;
  println!(
    "{}: {} entries, {} recordings, {}",
    file.display(),
    done.entries,
    done.recordings,
    transfer::size(done.bytes)
  );
  Ok(())
}

/// `wavo-app import`: an archive of `wavo-app export`, or `handy`; a summary on stdout.
pub fn import(source: &Path, take_settings: bool, dry_run: bool) -> Result<()> {
  let home = crate::home();
  let would = if dry_run { "would be " } else { "" };
  if source == Path::new("handy") {
    if take_settings {
      bail!("Handy's settings are not imported: leave out --settings");
    }

    let (found, report) = handy::import(&home, &handy::dir(), dry_run)?;
    let days =
      |at: Option<i64>| at.and_then(crate::history::recordings::day_of).unwrap_or_default();
    println!(
      "Handy in {}: {} entries with text from {} to {}, {} recordings ({})",
      found.dir.display(),
      found.entries,
      days(found.first),
      days(found.last),
      found.recordings,
      transfer::size(found.bytes)
    );
    print_report(&home, &report, would);
    return Ok(());
  }

  let done = archive::import(&home, source, take_settings, dry_run)?;
  print_report(&home, &done.report, would);
  match (done.settings, take_settings) {
    (true, _) => println!("settings {would}replaced with the archive's"),
    (false, true) => println!("no settings in the archive: these stay"),
    (false, false) => {}
  }

  Ok(())
}

fn print_report(home: &Path, r: &Report, would: &str) {
  println!(
    "{} entries with text: {} {would}added, {} there already; {} recordings {would}copied",
    r.entries,
    r.added,
    r.entries - r.added,
    r.copied
  );
  if r.failed > 0 {
    println!(
      "{} recordings could not be copied: their entries name a missing file until an import \
       copies it",
      r.failed
    );
  }

  // The cleanup applies it to imported entries as to any. Read as it is: `Settings::load` would
  // set a bad file aside, and a dry run writes nothing.
  let file = std::fs::read(home.join(settings::FILE)).unwrap_or_default();
  let retention = serde_json::from_slice::<serde_json::Value>(&file)
    .ok()
    .and_then(|v| serde_json::from_value::<Retention>(v.get("history_retention")?.clone()).ok())
    .unwrap_or_default();
  if r.added > 0 && retention != Retention::Unlimited {
    println!("the history keeps {retention:?}: older entries go at the next cleanup");
  }
}

/// `wavo-app devices`: input devices, the system default marked with `*`.
pub fn devices() -> Result<()> {
  let list = devices::list();
  if list.is_empty() {
    eprintln!("no input devices");
    return Ok(());
  }

  let name = list.iter().map(|d| d.name.chars().count()).max().unwrap_or(0);
  println!("  {:<name$}  {:>8}  {:>8}  {:<6}  ID", "NAME", "CHANNELS", "RATE", "FORMAT");
  for d in &list {
    let mark = if d.default { '*' } else { ' ' };
    let channels = d.channels.map_or("-".into(), |c| c.to_string());
    let rate = d.rate.map_or("-".into(), |r| r.to_string());
    let format = d.format.as_deref().unwrap_or("-");
    let id = d.id.as_deref().unwrap_or("-");
    println!("{mark} {:<name$}  {channels:>8}  {rate:>8}  {format:<6}  {id}", d.name);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn seconds_are_positive_and_fit_a_duration() {
    assert_eq!(seconds("1.5"), Ok(Duration::from_millis(1_500)));
    for bad in ["0", "-1", "nan", "inf", "1e300", "ten"] {
      assert!(seconds(bad).is_err(), "{bad}");
    }
  }
}
