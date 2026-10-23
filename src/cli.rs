// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Command-line tools that run without the GUI: `wavo-app devices` and `record`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::audio::capture::Source;
use crate::audio::devices::{self, Choice};
use crate::audio::recorder::Recorder;
use crate::audio::{SAMPLE_RATE, vad};

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
