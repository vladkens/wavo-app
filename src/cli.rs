// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Command-line tools that run without the GUI: `wavo-app devices`, `record`, `models` and `pull`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use wavo::hub::{self, Hub};

use crate::audio::capture::Source;
use crate::audio::devices::{self, Choice};
use crate::audio::recorder::Recorder;
use crate::audio::{SAMPLE_RATE, vad};
use crate::models::{self, download};

/// `wavo-app record`: `seconds` of the microphone (or `WAVO_FAKE_MIC`) into `out`, then a summary
/// on stderr. A device error ends the recording early; what was recorded is kept.
pub fn record(length: Duration, device: Option<&str>, out: &Path) -> Result<()> {
  let source = Source::from_env(device.map(Choice::id_or_name));
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
    if let Ok(failure) = recorder.failures().recv_timeout(left.min(Duration::from_millis(33))) {
      warn!("the microphone stopped early: {failure}");
      break;
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

/// `wavo-app models`: every speech model found, in the order of wavo's list.
pub fn models() -> Result<()> {
  let hub = Hub::new();
  let models = models::discover(&hub);
  if models.is_empty() {
    eprintln!("no speech models in {hub}: download one with `wavo-app pull <name>`");
    return Ok(());
  }

  let name = models.iter().map(|m| m.info.name.len()).max().unwrap_or(0).max(4);
  println!("{:<name$}  {:>8}  PATH", "NAME", "SIZE");
  for m in &models {
    println!("{:<name$}  {:>8}  {}", m.info.name, human_size(m.size), m.path.display());
  }

  Ok(())
}

/// `wavo-app pull`: one of wavo's models into the Hugging Face cache, its progress on stderr and
/// then its path on stdout; an interrupted download resumes.
pub fn pull(name: &str) -> Result<()> {
  let model = hub::lookup(name)?;
  let mut shown = None;
  let path = download::pull(&Hub::new(), model, |p| {
    let percent = p.received * 100 / p.total.max(1);
    if shown != Some(percent) {
      eprintln!("{}: {percent}% of {}", p.file, human_size(p.total));
      shown = Some(percent);
    }
  });

  let path = path.with_context(|| format!("download {}", model.file()))?;
  println!("{}", path.display());
  Ok(())
}

fn human_size(bytes: u64) -> String {
  const MB: f64 = 1_000_000.0;
  match bytes as f64 / MB {
    mb if mb >= 1_000.0 => format!("{:.1} GB", mb / 1_000.0),
    mb => format!("{mb:.0} MB"),
  }
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
