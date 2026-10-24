// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Playing a recording (the History page's Play) on the default output device through cpal: the
//! file is read and converted to the device's rate first, so the device's callback only copies
//! samples (it never allocates, locks or blocks). All of it happens on a thread of its own, which
//! also waits for the end and says so: the caller (the settings window's frame) never waits for
//! the file or the device, which takes a while to open.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};

use super::{resample, wav};

/// How often the end is looked for.
const WATCH: Duration = Duration::from_millis(50);
/// Silence played after the recording before it ends: the device's buffers still hold its last
/// samples when the callback has handed them over.
const TAIL: Duration = Duration::from_millis(250);

/// What the device's callback and the player's thread share.
struct State {
  samples: Vec<f32>,
  /// Samples played (mono, at the device's rate), the tail's silence included.
  played: AtomicUsize,
  /// Silent samples after the recording.
  tail: usize,
  ended: AtomicBool,
}

/// What the player's thread and the [Playback] share.
#[derive(Default)]
struct Control {
  /// Dropped: the thread stops the stream and says nothing.
  stopped: AtomicBool,
  /// Played to its end, stopped, or failed.
  done: AtomicBool,
  error: Mutex<Option<String>>,
}

/// A recording that plays; dropping it stops it.
pub struct Playback {
  control: Arc<Control>,
}

impl Playback {
  /// Plays the WAV at `path` on a thread of its own, which reads it, opens the output device and
  /// waits for the end; `ended` runs on that thread when it has played to its end or could not
  /// play ([Playback::error]), not when it is stopped.
  pub fn start(path: &Path, ended: impl FnOnce() + Send + 'static) -> Result<Self> {
    let control = Arc::new(Control::default());
    let shared = control.clone();
    let path = path.to_owned();
    thread::Builder::new().name("playback".into()).spawn(move || {
      if let Err(e) = play(&path, &shared) {
        tracing::warn!("play {}: {e:#}", path.display());
        *shared.error.lock().unwrap() = Some(format!("{e:#}"));
      }

      shared.done.store(true, Ordering::Release);
      if !shared.stopped.load(Ordering::Acquire) {
        ended();
      }
    })?;
    Ok(Self { control })
  }

  /// Not done yet: it opens or plays.
  pub fn playing(&self) -> bool {
    !self.control.done.load(Ordering::Acquire)
  }

  /// Why it could not play, once it is done.
  pub fn error(&self) -> Option<String> {
    self.control.error.lock().unwrap().clone()
  }
}

impl Drop for Playback {
  fn drop(&mut self) {
    self.control.stopped.store(true, Ordering::Release);
  }
}

/// Plays the WAV at `path` to its end, or until `control` is stopped. On the player's thread.
fn play(path: &Path, control: &Control) -> Result<()> {
  let wav::Interleaved { samples, rate, channels } = wav::read_interleaved(path)?;
  let mono: Vec<f32> = match channels as usize {
    1 => samples,
    n => samples.chunks_exact(n).map(|f| f.iter().sum::<f32>() / n as f32).collect(),
  };
  let device = cpal::default_host().default_output_device().context("no output device")?;
  let supported = device.default_output_config().context("output format")?;
  let config = supported.config();
  let samples = resample::convert(mono, rate, config.sample_rate)?;
  let state = Arc::new(State {
    samples,
    played: AtomicUsize::new(0),
    tail: (TAIL.as_secs_f64() * config.sample_rate as f64) as usize,
    ended: AtomicBool::new(false),
  });
  macro_rules! build {
    ($($variant:ident => $t:ty),*) => {
      match supported.sample_format() {
        $(SampleFormat::$variant => build::<$t>(&device, config, state.clone()),)*
        other => bail!("unsupported sample format {other}"),
      }
    };
  }
  // Every PCM format; DSD is a bit stream, not samples.
  let stream = build!(
    F32 => f32, F64 => f64, I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
    U8 => u8, U16 => u16, U24 => cpal::U24, U32 => u32, U64 => u64
  )?;
  stream.play().context("start the output stream")?;
  while !state.ended.load(Ordering::Acquire) && !control.stopped.load(Ordering::Acquire) {
    thread::sleep(WATCH);
  }

  Ok(())
}

fn build<T>(
  device: &cpal::Device,
  config: cpal::StreamConfig,
  state: Arc<State>,
) -> Result<cpal::Stream>
where
  T: SizedSample + FromSample<f32>,
{
  let channels = config.channels as usize;
  let gone = state.clone();
  let stream = device.build_output_stream(
    config,
    move |out: &mut [T], _: &cpal::OutputCallbackInfo| fill(&state, out, channels),
    move |e: cpal::Error| {
      tracing::debug!("output: {e}");
      // The device went: nothing plays any more, so it is the end.
      if matches!(e.kind(), ErrorKind::DeviceNotAvailable | ErrorKind::HostUnavailable) {
        gone.ended.store(true, Ordering::Release);
      }
    },
    None,
  )?;
  Ok(stream)
}

/// The next samples into `out` (every channel the same), silence past the end; it ends after the
/// tail.
fn fill<T: SizedSample + FromSample<f32>>(state: &State, out: &mut [T], channels: usize) {
  let end = state.samples.len() + state.tail;
  let mut at = state.played.load(Ordering::Relaxed);
  for frame in out.chunks_mut(channels.max(1)) {
    let sample = state.samples.get(at).copied().unwrap_or(0.0);
    frame.iter_mut().for_each(|s| *s = T::from_sample(sample));
    at = (at + 1).min(end);
  }
  state.played.store(at, Ordering::Relaxed);
  if at >= end {
    state.ended.store(true, Ordering::Release);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fills_every_channel_then_silence_and_ends() {
    let state = State {
      samples: vec![0.5, -0.5, 1.0],
      played: AtomicUsize::new(0),
      tail: 1,
      ended: AtomicBool::new(false),
    };
    let mut out = [0f32; 4];
    fill(&state, &mut out, 2);
    assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
    assert!(!state.ended.load(Ordering::Relaxed));
    fill(&state, &mut out, 2);
    assert_eq!(out, [1.0, 1.0, 0.0, 0.0]);
    assert!(state.ended.load(Ordering::Relaxed), "after the tail");
    assert_eq!(state.played.load(Ordering::Relaxed), 4);
  }

  /// A file that can't be read ends on the player's thread with the reason, never in the caller.
  #[test]
  fn a_missing_file_fails_on_the_thread() {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = Playback::start(Path::new("/nonexistent/take.wav"), move || _ = tx.send(()));
    let p = p.expect("the thread starts");
    rx.recv_timeout(Duration::from_secs(5)).expect("ended");
    assert!(!p.playing());
    assert!(p.error().is_some_and(|e| e.contains("take.wav")), "{:?}", p.error());
  }
}
