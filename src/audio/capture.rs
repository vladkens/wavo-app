// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Capture sources. Each one pushes interleaved `f32` samples into an `rtrb` ring that the
//! recorder's processing thread drains: a cpal input stream, or, with `WAVO_FAKE_MIC=<wav>`, a WAV
//! file played in real time (then digital silence, like a microphone in a quiet room).
//!
//! The cpal data callback runs on the audio thread: it only converts samples into the ring and
//! counts what did not fit, no locks and no allocation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};
use tracing::{debug, info};

use super::devices::{self, Choice, Cpal};
use super::wav;

/// Path of a WAV file that replaces the microphone.
pub const FAKE_MIC_ENV: &str = "WAVO_FAKE_MIC";
/// The ring holds this much audio; the recorder drains it every few milliseconds.
const RING_SECONDS: usize = 2;
/// How often the fake microphone delivers a block.
const FAKE_MIC_BLOCK: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
  pub rate: u32,
  pub channels: u16,
}

/// Written by a source (its callback or thread), read by the recorder.
#[derive(Debug, Default)]
pub struct Signals {
  first_data: OnceLock<Instant>,
  /// Samples lost because the ring was full.
  dropped: AtomicU64,
  /// Buffer overruns the backend reported.
  xruns: AtomicU64,
  failed: AtomicBool,
  failure: Mutex<Option<String>>,
}

impl Signals {
  /// When the first non-empty buffer arrived.
  pub fn first_data(&self) -> Option<Instant> {
    self.first_data.get().copied()
  }

  pub fn dropped(&self) -> u64 {
    self.dropped.load(Ordering::Relaxed)
  }

  pub fn xruns(&self) -> u64 {
    self.xruns.load(Ordering::Relaxed)
  }

  /// A fatal stream error: capture has stopped, the audio so far is kept.
  pub fn fail(&self, message: String) {
    if let Ok(mut failure) = self.failure.lock()
      && failure.is_none()
    {
      *failure = Some(message);
    }

    self.failed.store(true, Ordering::Release);
  }

  pub fn failed(&self) -> bool {
    self.failed.load(Ordering::Acquire)
  }

  pub fn failure(&self) -> Option<String> {
    self.failure.lock().ok().and_then(|f| f.clone())
  }

  /// Pushes as many whole frames of `samples` as fit, counting the rest as dropped.
  pub fn push<T>(&self, ring: &mut Producer<f32>, samples: &[T], channels: usize)
  where
    T: Sample + Copy,
    f32: FromSample<T>,
  {
    if samples.is_empty() {
      return;
    }

    self.first_data.get_or_init(Instant::now);
    let fits = ring.slots().min(samples.len()) / channels * channels;
    if let Ok(chunk) = ring.write_chunk_uninit(fits) {
      chunk.fill_from_iter(samples.iter().map(|s| s.to_sample::<f32>()));
    }

    if fits < samples.len() {
      self.dropped.fetch_add((samples.len() - fits) as u64, Ordering::Relaxed);
    }
  }
}

/// A running capture; its samples arrive in the ring [`start`] returns with it. Dropping it stops
/// the source: the microphone is closed.
pub struct Capture {
  pub format: Format,
  /// Name of the device actually opened.
  pub device: String,
  /// Why it is not the device asked for, if it is not.
  pub warning: Option<String>,
  /// This source's alone: a device that failed to open before it had its own.
  pub signals: Arc<Signals>,
  _feed: Feed,
}

/// Only held: dropping it stops capture.
enum Feed {
  Mic { _stream: cpal::Stream },
  File { _feeder: FileSource },
}

/// What a recording captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
  /// A microphone: the chosen one (falling back to the system default if it is missing), or the
  /// system default for `None`.
  Mic(Option<Choice>),
  /// A WAV file played in real time instead of a microphone.
  File(PathBuf),
}

impl Source {
  /// The fake microphone if [`FAKE_MIC_ENV`] is set, else the microphone `choice`.
  pub fn from_env(choice: Option<Choice>) -> Self {
    let fake = std::env::var_os(FAKE_MIC_ENV).filter(|p| !p.is_empty()).map(PathBuf::from);
    match fake {
      Some(path) => {
        if let Some(choice) = choice {
          info!("{FAKE_MIC_ENV} is set: recording from the file instead of {:?}", choice.name);
        }

        Self::File(path)
      }
      None => Self::Mic(choice),
    }
  }
}

/// Starts capturing from `source`.
pub fn start(source: &Source) -> Result<(Capture, Consumer<f32>)> {
  let choice = match source {
    Source::File(path) => return start_file(path),
    Source::Mic(choice) => choice.as_ref(),
  };

  let opened = devices::open(&Cpal::new(), choice, |name, device| {
    let started = Instant::now();
    let opened = open_stream(device);
    debug!("opened {name:?} in {:.1?}", started.elapsed());
    opened
  })?;
  let (stream, ring, format, signals) = opened.value;
  let capture = Capture {
    format,
    device: opened.name,
    warning: opened.warning,
    signals,
    _feed: Feed::Mic { _stream: stream },
  };
  Ok((capture, ring))
}

/// A stream with signals of its own: a failed attempt's error callback may still fire.
fn open_stream(
  device: &cpal::Device,
) -> Result<(cpal::Stream, Consumer<f32>, Format, Arc<Signals>)> {
  let signals = Arc::new(Signals::default());
  let supported = device.default_input_config().context("input format")?;
  let config = supported.config();
  let format = Format { rate: config.sample_rate, channels: config.channels };
  let (producer, ring) = ring(format);
  macro_rules! build {
    ($($variant:ident => $t:ty),*) => {
      match supported.sample_format() {
        $(SampleFormat::$variant => build::<$t>(device, config, producer, &signals),)*
        other => bail!("unsupported sample format {other}"),
      }
    };
  }

  let stream = build!(
    F32 => f32, F64 => f64, I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
    U8 => u8, U16 => u16, U24 => cpal::U24, U32 => u32, U64 => u64
  )?;
  stream.play().context("start the input stream")?;
  Ok((stream, ring, format, signals))
}

fn build<T>(
  device: &cpal::Device,
  config: cpal::StreamConfig,
  mut producer: Producer<f32>,
  signals: &Arc<Signals>,
) -> Result<cpal::Stream>
where
  T: SizedSample,
  f32: FromSample<T>,
{
  let channels = config.channels as usize;
  let (data, errors) = (signals.clone(), signals.clone());
  let stream = device.build_input_stream(
    config,
    move |samples: &[T], _: &cpal::InputCallbackInfo| data.push(&mut producer, samples, channels),
    move |e: cpal::Error| match e.kind() {
      cpal::ErrorKind::Xrun => {
        errors.xruns.fetch_add(1, Ordering::Relaxed);
      }
      cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => debug!("input: {e}"),
      _ => errors.fail(e.to_string()),
    },
    None,
  )?;
  Ok(stream)
}

fn ring(format: Format) -> (Producer<f32>, Consumer<f32>) {
  RingBuffer::new(format.rate as usize * format.channels as usize * RING_SECONDS)
}

/// The fake microphone: a thread that feeds the file's samples at their real pace.
struct FileSource {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl Drop for FileSource {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

fn start_file(path: &Path) -> Result<(Capture, Consumer<f32>)> {
  let wav::Interleaved { samples, rate, channels } =
    wav::read_interleaved(path).with_context(|| FAKE_MIC_ENV.to_string())?;
  let format = Format { rate, channels };
  let (mut producer, ring) = ring(format);
  let stop = Arc::new(AtomicBool::new(false));
  let signals = Arc::new(Signals::default());
  let (stop_flag, feed_signals) = (stop.clone(), signals.clone());
  let channels = channels as usize;
  let mut feed = FileFeed::new(samples, rate, channels);
  let thread = thread::Builder::new().name("fake-mic".into()).spawn(move || {
    let started = Instant::now();
    while !stop_flag.load(Ordering::Acquire) {
      feed.play(started.elapsed(), |block| feed_signals.push(&mut producer, block, channels));
      thread::sleep(FAKE_MIC_BLOCK);
    }
  })?;

  let name =
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into());
  let capture = Capture {
    format,
    device: format!("fake mic ({name})"),
    warning: None,
    signals,
    _feed: Feed::File { _feeder: FileSource { stop, thread: Some(thread) } },
  };
  Ok((capture, ring))
}

/// The file's samples, then silence, as they fall due.
struct FileFeed {
  samples: Vec<f32>,
  /// Whole frames, so a block of silence never splits one.
  silence: Vec<f32>,
  rate: u32,
  channels: usize,
  /// Samples handed out, interleaved.
  sent: usize,
}

impl FileFeed {
  fn new(samples: Vec<f32>, rate: u32, channels: usize) -> Self {
    let silence = vec![0.0; (rate as usize / 10).max(1) * channels];
    Self { samples, silence, rate, channels, sent: 0 }
  }

  /// Hands `push` everything due `elapsed` after the start that it has not had yet.
  fn play(&mut self, elapsed: Duration, mut push: impl FnMut(&[f32])) {
    let due = (elapsed.as_secs_f64() * self.rate as f64) as usize * self.channels;
    while self.sent < due {
      let n = (due - self.sent).min(self.silence.len());
      let block = match self.samples.get(self.sent..) {
        Some(rest) if !rest.is_empty() => &rest[..n.min(rest.len())],
        _ => &self.silence[..n],
      };
      push(block);
      self.sent += block.len();
    }
  }
}

/// A capture of `format` whose samples the test pushes into the ring itself, at its own pace.
#[cfg(test)]
pub fn manual(format: Format) -> (Capture, Producer<f32>, Consumer<f32>) {
  let (producer, ring) = ring(format);
  let feeder = FileSource { stop: Arc::default(), thread: None };
  let capture = Capture {
    format,
    device: "manual".into(),
    warning: None,
    signals: Arc::default(),
    _feed: Feed::File { _feeder: feeder },
  };
  (capture, producer, ring)
}

#[cfg(test)]
mod tests {
  use std::f32::consts::TAU;

  use hound::{SampleFormat, WavSpec, WavWriter};

  use super::*;
  use crate::testing::TempDir;

  #[test]
  fn push_keeps_whole_frames_and_counts_drops() {
    let signals = Signals::default();
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(5);
    signals.push(&mut producer, &[0i16, i16::MAX, i16::MIN], 2);
    assert!(signals.first_data().is_some());

    // Of one and a half stereo frames only the whole one is kept.
    assert_eq!(consumer.slots(), 2);
    assert_eq!(signals.dropped(), 1);
    signals.push(&mut producer, &[0.5f32; 4], 2);
    assert_eq!(consumer.slots(), 4);
    assert_eq!(signals.dropped(), 3);

    let chunk = consumer.read_chunk(4).unwrap();
    let (a, b) = chunk.as_slices();
    let got: Vec<f32> = a.iter().chain(b).copied().collect();
    assert_eq!(got, [0.0, i16::MAX as f32 / 32_768.0, 0.5, 0.5]);
  }

  #[test]
  fn first_failure_wins() {
    let signals = Signals::default();
    assert!(!signals.failed());
    signals.fail("device unplugged".into());
    signals.fail("later".into());
    assert!(signals.failed());
    assert_eq!(signals.failure().as_deref(), Some("device unplugged"));
  }

  #[test]
  fn fake_mic_plays_at_the_files_pace_then_silence() {
    // 1 kHz stereo: 0.25 s of the file, a frame per millisecond.
    let file: Vec<f32> = (1..=500).map(|i| i as f32).collect();
    let mut feed = FileFeed::new(file.clone(), 1_000, 2);
    let mut got = Vec::new();
    let ms = Duration::from_millis;
    for (at, total) in
      [(0, 0), (10, 20), (10, 20), (15, 30), (240, 480), (400, 800), (1_000, 2_000)]
    {
      feed.play(ms(at), |block| {
        assert_eq!(block.len() % 2, 0, "whole frames");
        got.extend_from_slice(block);
      });
      assert_eq!(got.len(), total, "after {at} ms");
    }

    assert_eq!(got[..500], file[..], "the file first");
    assert!(got[500..].iter().all(|&s| s == 0.0), "then silence");
  }

  #[test]
  fn fake_mic_delivers_the_file() {
    let dir = TempDir::new("fake-mic");
    let path = dir.path().join("in.wav");
    let spec = WavSpec {
      channels: 2,
      sample_rate: 44_100,
      bits_per_sample: 32,
      sample_format: SampleFormat::Float,
    };
    let samples: Vec<f32> =
      (0..4_000).map(|i| (TAU * 440.0 * (i / 2) as f32 / 44_100.0).sin() * 0.5).collect();
    let mut w = WavWriter::create(&path, spec).unwrap();
    for &s in &samples {
      w.write_sample(s).unwrap();
    }

    w.finalize().unwrap();

    let (capture, mut ring) = start(&Source::File(path)).unwrap();
    assert_eq!(capture.format, Format { rate: 44_100, channels: 2 });
    assert!(capture.device.contains("in.wav"), "{}", capture.device);

    let deadline = Instant::now() + Duration::from_secs(10);
    while ring.slots() < 2_000 {
      assert!(Instant::now() < deadline, "only {} samples within 10 s", ring.slots());
      thread::sleep(Duration::from_millis(5));
    }

    assert!(capture.signals.first_data().is_some());
    let chunk = ring.read_chunk(2_000).unwrap();
    let (a, b) = chunk.as_slices();
    let got: Vec<f32> = a.iter().chain(b).copied().collect();
    assert_eq!(got, samples[..2_000]);
  }
}
