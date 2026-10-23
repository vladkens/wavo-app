// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A recording: a capture source, and a processing thread that drains its ring every [`POLL`]
//! (woken at once on stop): downmix to mono, resampling to 16 kHz, the signal level, VAD scores
//! and the WAV file. The microphone is open only between [`Recorder::start`] and
//! [`Recorder::stop`].
//!
//! A device error mid-recording (unplugged, sample rate changed) ends capture but not the
//! recording: everything up to the error is kept and the error becomes a warning.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, bounded};
use rtrb::Consumer;
use tracing::{debug, warn};

use super::SAMPLE_RATE;
use super::capture::{self, Capture, Signals, Source};
use super::resample::To16k;
use super::vad::{Clip, Vad};
use super::wav::WavSink;

/// How often the processing thread drains the ring (only while recording).
const POLL: Duration = Duration::from_millis(10);
/// The level is the RMS of windows this long: ~30 updates per second.
const LEVEL_WINDOW: usize = SAMPLE_RATE as usize / 30;
/// Exact zeros this long mean no signal at all, not a quiet room.
const DEAD_SILENCE: usize = SAMPLE_RATE as usize / 2;

/// A recording in progress. Dropping it without [`stop`](Recorder::stop) still closes the
/// microphone and finalizes the WAV file.
pub struct Recorder {
  capture: Option<Capture>,
  device: String,
  warning: Option<String>,
  started: Instant,
  signals: Arc<Signals>,
  level: Arc<AtomicU32>,
  stop: Arc<AtomicBool>,
  failures: Receiver<String>,
  thread: Option<JoinHandle<Result<Processed>>>,
}

#[derive(Debug)]
pub struct Recording {
  /// The whole recording, 16 kHz mono, with its VAD scores.
  pub clip: Clip,
  pub wav: PathBuf,
  pub device: String,
  /// Device fallback, a device error that ended capture, lost samples, a WAV write error, no
  /// sound at all.
  pub warnings: Vec<String>,
  /// From [`Recorder::start`] to the first buffer of samples.
  pub open_latency: Option<Duration>,
}

impl Recorder {
  /// Opens `source` (a chosen microphone that is missing falls back to the system default with a
  /// warning) and records into `wav`, a new file; if it cannot, the file is gone again.
  pub fn start(source: &Source, wav: &Path) -> Result<Self> {
    let started = Instant::now();

    // The file first: a bad path never opens the microphone.
    let sink = WavSink::create(wav)?;
    let signals = Arc::new(Signals::default());
    let recorder = capture::start(source, &signals)
      .and_then(|(capture, ring)| Self::run(capture, ring, sink, signals, started));
    if recorder.is_err() {
      let _ = fs::remove_file(wav);
    }

    recorder
  }

  fn run(
    capture: Capture,
    ring: Consumer<f32>,
    sink: WavSink,
    signals: Arc<Signals>,
    started: Instant,
  ) -> Result<Self> {
    let processor = Processor::new(capture.format.channels as usize, capture.format.rate, sink)?;
    let level = processor.level.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (failures_tx, failures) = bounded(1);
    let (thread_stop, thread_signals) = (stop.clone(), signals.clone());
    let thread = thread::Builder::new()
      .name("audio".into())
      .spawn(move || process(ring, processor, &thread_stop, &thread_signals, &failures_tx))
      .context("spawn the audio thread")?;

    Ok(Self {
      device: capture.device.clone(),
      warning: capture.warning.clone(),
      capture: Some(capture),
      started,
      signals,
      level,
      stop,
      failures,
      thread: Some(thread),
    })
  }

  /// Name of the device actually recording.
  pub fn device(&self) -> &str {
    &self.device
  }

  /// Why the device is not the one asked for, if it is not.
  pub fn warning(&self) -> Option<&str> {
    self.warning.as_deref()
  }

  /// RMS of the last ~33 ms, linear, 0 to 1.
  pub fn level(&self) -> f32 {
    f32::from_bits(self.level.load(Ordering::Relaxed))
  }

  /// Gets one message if the device fails mid-recording; capture has stopped by then, and
  /// [`stop`](Recorder::stop) returns what was recorded.
  pub fn failures(&self) -> &Receiver<String> {
    &self.failures
  }

  fn open_latency(&self) -> Option<Duration> {
    self.signals.first_data().map(|t| t.duration_since(self.started))
  }

  /// Closes the microphone, drains what is left and finalizes the WAV file.
  pub fn stop(mut self) -> Result<Recording> {
    let processed = self.finish()?;
    let mut warnings: Vec<String> = self.warning.take().into_iter().collect();
    if let Some(failure) = self.signals.failure() {
      warnings.push(format!("the microphone stopped: {failure}"));
    }

    let dropped = self.signals.dropped();
    if dropped > 0 {
      warnings.push(format!("{dropped} samples were lost (the audio thread fell behind)"));
    }

    if self.signals.xruns() > 0 {
      debug!("{} input overruns", self.signals.xruns());
    }

    warnings.extend(processed.warnings);
    if processed.dead {
      // A microphone that is off (a closed laptop lid), blocked by privacy settings, or the
      // wrong one.
      warnings.push(format!("No sound from {}: check the microphone", self.device));
    }

    Ok(Recording {
      clip: processed.clip,
      wav: processed.wav,
      device: std::mem::take(&mut self.device),
      warnings,
      open_latency: self.open_latency(),
    })
  }

  fn finish(&mut self) -> Result<Processed> {
    drop(self.capture.take()); // closes the microphone; the ring keeps what it delivered
    self.stop.store(true, Ordering::Release);
    let thread = self.thread.take().ok_or_else(|| anyhow!("the recording has already stopped"))?;
    thread.thread().unpark(); // no need to wait out the poll interval
    thread.join().map_err(|_| anyhow!("the audio thread panicked"))?
  }
}

impl Drop for Recorder {
  fn drop(&mut self) {
    if self.thread.is_some()
      && let Err(e) = self.finish()
    {
      warn!("recording: {e:#}");
    }
  }
}

fn process(
  mut ring: Consumer<f32>,
  mut processor: Processor,
  stop: &AtomicBool,
  signals: &Signals,
  failures: &Sender<String>,
) -> Result<Processed> {
  loop {
    // Read the flags before draining, so samples delivered before a stop are all processed.
    let stopping = stop.load(Ordering::Acquire);
    let failed = signals.failed();
    let n = ring.slots();
    if n > 0 {
      let chunk = ring.read_chunk(n)?;
      let (a, b) = chunk.as_slices();
      processor.push(a)?;
      processor.push(b)?;
      chunk.commit_all();
    }

    if failed {
      let _ = failures.try_send(signals.failure().unwrap_or_default());
    }

    if stopping || failed {
      return processor.finish();
    }

    thread::park_timeout(POLL);
  }
}

struct Processed {
  clip: Clip,
  wav: PathBuf,
  warnings: Vec<String>,
  /// Only exact zeros came, at least [`DEAD_SILENCE`] of them.
  dead: bool,
}

/// Interleaved device samples in, 16 kHz mono out to the WAV file, the VAD and memory.
struct Processor {
  channels: usize,
  /// Samples of an interleaved frame split across two pushes.
  carry: Vec<f32>,
  mono: Vec<f32>,
  resampler: To16k,
  out: Vec<f32>,
  pcm: Vec<f32>,
  vad: Vad,
  wav_path: PathBuf,
  /// `None` after a write error: recording goes on in memory.
  wav: Option<WavSink>,
  level: Arc<AtomicU32>,
  level_sum: f64,
  level_n: usize,
  warnings: Vec<String>,
}

impl Processor {
  fn new(channels: usize, rate: u32, wav: WavSink) -> Result<Self> {
    Ok(Self {
      channels: channels.max(1),
      carry: Vec::with_capacity(channels),
      mono: Vec::new(),
      resampler: To16k::new(rate)?,
      out: Vec::new(),
      pcm: Vec::new(),
      vad: Vad::default(),
      wav_path: wav.path().to_owned(),
      wav: Some(wav),
      level: Arc::new(AtomicU32::new(0)),
      level_sum: 0.0,
      level_n: 0,
      warnings: Vec::new(),
    })
  }

  fn push(&mut self, mut interleaved: &[f32]) -> Result<()> {
    if interleaved.is_empty() {
      return Ok(());
    }

    self.mono.clear();
    let ch = self.channels;
    if ch == 1 {
      self.mono.extend_from_slice(interleaved);
    } else {
      if !self.carry.is_empty() {
        let take = (ch - self.carry.len()).min(interleaved.len());
        self.carry.extend_from_slice(&interleaved[..take]);
        interleaved = &interleaved[take..];
        if self.carry.len() == ch {
          self.mono.push(self.carry.iter().sum::<f32>() / ch as f32);
          self.carry.clear();
        }
      }

      let whole = interleaved.len() / ch * ch;
      let frames = interleaved[..whole].chunks_exact(ch);
      self.mono.extend(frames.map(|f| f.iter().sum::<f32>() / ch as f32));
      self.carry.extend_from_slice(&interleaved[whole..]);
    }

    self.out.clear();
    self.resampler.push(&self.mono, &mut self.out)?;
    self.emit();
    Ok(())
  }

  /// Hands the 16 kHz samples in `out` to the WAV file, the level meter, the VAD and memory.
  fn emit(&mut self) {
    if let Some(wav) = &mut self.wav
      && let Err(e) = wav.write(&self.out)
    {
      warn!("{e:#}; recording on in memory only");
      self.warnings.push(format!("the recording could not be saved: {e:#}"));
      self.wav = None;
    }

    for &s in &self.out {
      self.level_sum += (s * s) as f64;
      self.level_n += 1;
      if self.level_n == LEVEL_WINDOW {
        let rms = (self.level_sum / LEVEL_WINDOW as f64).sqrt() as f32;
        self.level.store(rms.to_bits(), Ordering::Relaxed);
        (self.level_sum, self.level_n) = (0.0, 0);
      }
    }

    self.vad.push(&self.out);
    self.pcm.extend_from_slice(&self.out);
  }

  fn finish(mut self) -> Result<Processed> {
    self.out.clear();
    self.resampler.finish(&mut self.out)?;
    self.emit();
    if let Some(wav) = self.wav.take()
      && let Err(e) = wav.finalize()
    {
      self.warnings.push(format!("the recording could not be saved: {e:#}"));
    }

    let dead = self.pcm.len() >= DEAD_SILENCE && self.pcm.iter().all(|&s| s == 0.0);
    let clip = Clip { scores: self.vad.finish(), pcm: self.pcm };
    Ok(Processed { clip, wav: self.wav_path, warnings: self.warnings, dead })
  }
}

#[cfg(test)]
mod tests {
  use std::f32::consts::TAU;

  use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
  use rtrb::Producer;

  use super::*;
  use crate::audio::capture::Format;
  use crate::audio::vad::FRAME;
  use crate::testing::TempDir;

  fn sine(rate: u32, channels: u16, seconds: f32) -> Vec<f32> {
    let frames = (rate as f32 * seconds) as usize;
    let mono = (0..frames).map(|i| (TAU * 440.0 * i as f32 / rate as f32).sin() * 0.5);
    mono.flat_map(|s| std::iter::repeat_n(s, channels as usize)).collect()
  }

  #[test]
  fn processor_downmixes_resamples_and_meters_across_split_frames() {
    let dir = TempDir::new("proc");
    let sink = WavSink::create(&dir.path().join("p.wav")).unwrap();
    let mut p = Processor::new(2, 48_000, sink).unwrap();
    let input = sine(48_000, 2, 1.0);
    for chunk in input.chunks(1_001) {
      p.push(chunk).unwrap(); // odd sizes split stereo frames
    }

    let level = f32::from_bits(p.level.load(Ordering::Relaxed));
    assert!((level - 0.5 / 2f32.sqrt()).abs() < 0.01, "level {level}");

    let processed = p.finish().unwrap();
    assert!(processed.warnings.is_empty(), "{:?}", processed.warnings);
    assert!(!processed.dead);
    let pcm = &processed.clip.pcm;
    assert!(pcm.len().abs_diff(16_000) <= 1, "{} samples", pcm.len());
    assert_eq!(processed.clip.scores.len(), pcm.len().div_ceil(FRAME));
    let wav = WavReader::open(&processed.wav).unwrap();
    assert_eq!(wav.len() as usize, pcm.len());
  }

  #[test]
  fn digital_silence_is_a_warning() {
    let dir = TempDir::new("proc-zeros");
    let sink = WavSink::create(&dir.path().join("z.wav")).unwrap();
    let mut p = Processor::new(1, 48_000, sink).unwrap();
    p.push(&[0.0; 96_000]).unwrap();
    assert!(p.finish().unwrap().dead);
  }

  /// 44.1 kHz stereo, as a microphone might deliver it.
  const FORMAT: Format = Format { rate: 44_100, channels: 2 };

  /// A recording into `out.wav` from a source the test feeds through the returned [`Feeder`].
  fn record_manual(dir: &TempDir) -> (Recorder, Feeder) {
    let signals = Arc::new(Signals::default());
    let (capture, producer, ring) = capture::manual(FORMAT);
    let sink = WavSink::create(&dir.path().join("out.wav")).unwrap();
    let recorder = Recorder::run(capture, ring, sink, signals.clone(), Instant::now()).unwrap();
    (recorder, Feeder { producer, signals })
  }

  struct Feeder {
    producer: Producer<f32>,
    signals: Arc<Signals>,
  }

  impl Feeder {
    fn push(&mut self, samples: &[f32]) {
      self.signals.push(&mut self.producer, samples, FORMAT.channels as usize);
    }
  }

  #[test]
  fn a_failed_start_removes_only_its_own_file() {
    let dir = TempDir::new("rec-failed");
    let missing = Source::File(dir.path().join("missing.wav"));
    let wav = dir.path().join("rec.wav");
    assert!(Recorder::start(&missing, &wav).is_err());
    assert!(!wav.exists(), "the file it created");

    fs::write(&wav, b"someone else's").unwrap();
    assert!(Recorder::start(&missing, &wav).is_err());
    assert_eq!(fs::read(&wav).unwrap(), b"someone else's", "a file that was there");
  }

  #[test]
  fn records_from_the_fake_mic() {
    let dir = TempDir::new("rec-fake-mic");
    let input = dir.path().join("in.wav");
    let mut w = WavWriter::create(&input, FAKE_MIC_SPEC).unwrap();
    for s in sine(44_100, 2, 5.0) {
      w.write_sample(s).unwrap();
    }

    w.finalize().unwrap();

    let recorder = Recorder::start(&Source::File(input), &dir.path().join("rec.wav")).unwrap();
    assert!(recorder.device().contains("in.wav"), "{}", recorder.device());
    let deadline = Instant::now() + Duration::from_secs(10);
    while recorder.level() < 0.3 {
      assert!(Instant::now() < deadline, "no sound within 10 s");
      thread::sleep(Duration::from_millis(5));
    }

    let rec = recorder.stop().unwrap();
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
    assert!(rec.clip.pcm.iter().any(|s| s.abs() > 0.4));
    let wav = WavReader::open(&rec.wav).unwrap();
    assert_eq!(wav.len() as usize, rec.clip.pcm.len());
  }

  const FAKE_MIC_SPEC: WavSpec = WavSpec {
    channels: 2,
    sample_rate: 44_100,
    bits_per_sample: 32,
    sample_format: SampleFormat::Float,
  };

  #[test]
  fn records_what_the_source_delivered() {
    let dir = TempDir::new("rec-manual");
    let (recorder, mut feeder) = record_manual(&dir);
    feeder.push(&sine(44_100, 2, 0.3));
    feeder.push(&[0.0; 44_100 / 5 * 2]);
    assert!(recorder.open_latency().is_some());

    let rec = recorder.stop().unwrap();
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
    assert!(rec.open_latency.is_some());
    let pcm = &rec.clip.pcm;
    assert!(pcm.len().abs_diff(8_000) <= 1, "{} samples", pcm.len());
    assert!(pcm[..4_000].iter().any(|s| s.abs() > 0.4));
    assert!(pcm[pcm.len() - 1_000..].iter().all(|s| s.abs() < 1e-3));
    let wav = WavReader::open(&rec.wav).unwrap();
    assert_eq!(wav.len() as usize, pcm.len());
  }

  #[test]
  fn device_error_ends_capture_and_keeps_the_audio() {
    let dir = TempDir::new("rec-fail");
    let (recorder, mut feeder) = record_manual(&dir);
    let audio = sine(44_100, 2, 0.3);
    feeder.push(&audio);
    recorder.signals.fail("The requested audio device is not available".into());
    let message = recorder.failures().recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(message.contains("not available"));

    feeder.push(&audio); // capture has ended: this is not recorded
    let rec = recorder.stop().unwrap();
    let pcm = &rec.clip.pcm;
    assert!(pcm.len().abs_diff(4_800) <= 1, "{} samples kept", pcm.len());
    assert!(rec.warnings.iter().any(|w| w.contains("microphone stopped")), "{:?}", rec.warnings);
    let wav = WavReader::open(&rec.wav).unwrap();
    assert_eq!(wav.len() as usize, pcm.len());
  }

  #[test]
  fn dropping_a_recorder_finalizes_the_file() {
    let dir = TempDir::new("rec-drop");
    let (recorder, mut feeder) = record_manual(&dir);
    feeder.push(&sine(44_100, 2, 0.2));
    drop(recorder);
    let wav = WavReader::open(dir.path().join("out.wav")).unwrap();
    assert!(wav.len().abs_diff(3_200) <= 1, "{} samples", wav.len());
  }
}
