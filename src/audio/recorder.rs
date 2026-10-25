// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A recording: a capture source, and a processing thread that drains its ring every [`POLL`]
//! (woken at once on stop): downmix to mono, resampling to 16 kHz, the signal level and whether
//! there is sound at all, VAD scores and the WAV file. The microphone is open only between
//! [`Recorder::start`] and [`Recorder::stop`].
//!
//! A device error mid-recording (unplugged, sample rate changed), or one of the processing thread,
//! ends capture but not the recording: everything up to the error is kept and the error becomes a
//! warning.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, bounded};
use rtrb::Consumer;
use tracing::{debug, warn};

use super::SAMPLE_RATE;
use super::capture::{self, Capture, Signals, Source};
use super::resample::To16k;
use super::vad::{self, Clip, Vad};
use super::wav::WavSink;

/// How often the processing thread drains the ring (only while recording).
const POLL: Duration = Duration::from_millis(10);
/// The level is the RMS of windows this long: ~30 updates per second.
const LEVEL_WINDOW: usize = SAMPLE_RATE as usize / 30;
/// Exact zeros this long mean no signal at all, not a quiet room.
const DEAD_SILENCE: usize = SAMPLE_RATE as usize / 2;
/// While recording, exact zeros this long: digital silence, said at once.
const LIVE_DEAD: usize = SAMPLE_RATE as usize;
/// While recording, a level under [`FAINT`] this long: the softer case (a muted or far
/// microphone).
const LIVE_FAINT: usize = SAMPLE_RATE as usize * 4;
/// RMS (linear) under which a window is no sound to speak of: −80 dBFS.
const FAINT: f32 = 1e-4;
/// [`Recorder::quiet`] counts no further back than this.
const QUIET_MAX: usize = vad::frames(1000);

/// A recording in progress. Dropping it without [`stop`](Recorder::stop) still closes the
/// microphone and finalizes the WAV file.
pub struct Recorder {
  capture: Option<Capture>,
  device: String,
  warning: Option<String>,
  started: Instant,
  signals: Arc<Signals>,
  live: Arc<Live>,
  /// Of the device.
  rate: u32,
  stop: Arc<AtomicBool>,
  failures: Receiver<String>,
  thread: Option<JoinHandle<Processed>>,
}

/// What the processing thread tells of the recording as it goes, readable from any thread.
#[derive(Debug, Default)]
struct Live {
  /// [`Level::rms`] as bits.
  level: AtomicU32,
  /// [`Level::silent`].
  silent: AtomicBool,
  /// [`Vad::trailing_quiet`] in frames.
  quiet: AtomicU32,
  /// VAD frames scored; it releases `quiet`: whoever sees the count sees the quiet that goes with
  /// it.
  scored: AtomicU64,
}

/// [`Recorder::level`] and whether the microphone sends any sound, readable from any thread.
#[derive(Debug, Clone)]
pub struct Meter(Arc<Live>);

impl Meter {
  /// RMS of the last ~33 ms, linear, 0 to 1.
  pub fn level(&self) -> f32 {
    f32::from_bits(self.0.level.load(Ordering::Relaxed))
  }

  /// The microphone has sent no sound for a while (see [`Level::silent`]): it may be off, muted
  /// or the wrong one. Back to `false` as soon as sound comes.
  pub fn silent(&self) -> bool {
    self.0.silent.load(Ordering::Relaxed)
  }
}

/// The level of 16 kHz samples as they come, by [`LEVEL_WINDOW`], and whether they are sound at
/// all: exact zeros for [`LIVE_DEAD`] (digital silence), or a level under [`FAINT`] for
/// [`LIVE_FAINT`], is silent; a window with sound ends it.
#[derive(Debug, Default)]
struct Level {
  /// RMS of the last whole window.
  rms: f32,
  /// Exact zeros in a row.
  zeros: usize,
  /// Samples in a row in windows under [`FAINT`].
  faint: usize,
  sum: f64,
  n: usize,
}

impl Level {
  fn push(&mut self, samples: &[f32]) {
    for &s in samples {
      self.zeros = if s == 0.0 { self.zeros + 1 } else { 0 };
      self.sum += f64::from(s * s);
      self.n += 1;
      if self.n == LEVEL_WINDOW {
        self.rms = (self.sum / LEVEL_WINDOW as f64).sqrt() as f32;
        self.faint = if self.rms < FAINT { self.faint + LEVEL_WINDOW } else { 0 };
        (self.sum, self.n) = (0.0, 0);
      }
    }
  }

  fn silent(&self) -> bool {
    self.zeros >= LIVE_DEAD || self.faint >= LIVE_FAINT
  }
}

#[derive(Debug)]
pub struct Recording {
  /// The whole recording, 16 kHz mono, with its VAD scores.
  pub clip: Clip,
  // Read by the tests only, until the audio step trims the recorder.
  #[cfg_attr(not(test), allow(dead_code))]
  pub wav: PathBuf,
  /// The WAV file holds the whole recording: no write error.
  pub saved: bool,
  /// Device fallback, a device error that ended capture, lost samples, a WAV write error, no
  /// sound at all.
  pub warnings: Vec<String>,
  /// From [`Recorder::start`] to the first buffer of samples.
  #[cfg_attr(not(test), allow(dead_code))]
  pub open_latency: Option<Duration>,
}

impl Recorder {
  /// Opens `source` (a chosen microphone that is missing falls back to the system default with a
  /// warning) and records into `wav`, a new file; if it cannot, the file is gone again.
  pub fn start(source: &Source, wav: &Path) -> Result<Self> {
    let started = Instant::now();

    // The file first: a bad path never opens the microphone.
    let sink = WavSink::create(wav)?;
    let recorder =
      capture::start(source).and_then(|(capture, ring)| Self::run(capture, ring, sink, started));
    if recorder.is_err() {
      let _ = fs::remove_file(wav);
    }

    recorder
  }

  fn run(capture: Capture, ring: Consumer<f32>, sink: WavSink, started: Instant) -> Result<Self> {
    let signals = capture.signals.clone();
    let processor = Processor::new(capture.format.channels as usize, capture.format.rate, sink)?;
    let live = processor.live.clone();
    let rate = capture.format.rate;
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
      live,
      rate,
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
    self.meter().level()
  }

  /// Gets one message if the device or the processing fails mid-recording; capture has stopped by
  /// then, and [`stop`](Recorder::stop) returns what was recorded.
  pub fn failures(&self) -> &Receiver<String> {
    &self.failures
  }

  /// How long the audio recorded so far ends without speech (at most a second).
  pub fn quiet(&self) -> Duration {
    vad::duration(self.live.quiet.load(Ordering::Relaxed) as usize)
  }

  /// The audio the device has delivered so far.
  pub fn delivered(&self) -> Duration {
    Duration::from_secs_f64(self.signals.delivered() as f64 / f64::from(self.rate))
  }

  /// The audio the VAD has scored so far, behind [`delivered`](Self::delivered) by the
  /// resampler's block and a VAD frame (up to ~50 ms). [`quiet`](Self::quiet) read after it is at
  /// least as recent.
  pub fn scored(&self) -> Duration {
    vad::duration(self.live.scored.load(Ordering::Acquire) as usize)
  }

  /// The device failed: capture has ended.
  pub fn failed(&self) -> bool {
    self.signals.failed()
  }

  /// The level for another thread; it stops changing once the recording ends.
  pub fn meter(&self) -> Meter {
    Meter(self.live.clone())
  }

  fn open_latency(&self) -> Option<Duration> {
    self.signals.first_data().map(|t| t.duration_since(self.started))
  }

  /// Closes the microphone, drains what is left and finalizes the WAV file.
  pub fn stop(mut self) -> Result<Recording> {
    let processed = self.finish()?;
    let mut warnings: Vec<String> = self.warning.take().into_iter().collect();
    if let Some(failure) = self.signals.failure() {
      warnings.push(format!("the recording stopped: {failure}"));
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
      saved: processed.saved,
      warnings,
      open_latency: self.open_latency(),
    })
  }

  fn finish(&mut self) -> Result<Processed> {
    drop(self.capture.take()); // closes the microphone; the ring keeps what it delivered
    self.stop.store(true, Ordering::Release);
    let thread = self.thread.take().ok_or_else(|| anyhow!("the recording has already stopped"))?;
    thread.thread().unpark(); // no need to wait out the poll interval
    thread.join().map_err(|_| anyhow!("the audio thread panicked"))
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
) -> Processed {
  loop {
    // Read the flags before draining, so samples delivered before a stop are all processed.
    let stopping = stop.load(Ordering::Acquire);
    let mut failed = signals.failed();
    if let Err(e) = drain(&mut ring, &mut processor) {
      // As a device error: the recording ends with what was processed up to here.
      signals.fail(format!("the audio could not be processed: {e:#}"));
      failed = true;
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

/// Hands what the ring holds to the processor.
fn drain(ring: &mut Consumer<f32>, processor: &mut Processor) -> Result<()> {
  let n = ring.slots();
  if n == 0 {
    return Ok(());
  }

  let chunk = ring.read_chunk(n)?;
  let (a, b) = chunk.as_slices();
  processor.push(a)?;
  processor.push(b)?;
  chunk.commit_all();
  Ok(())
}

struct Processed {
  clip: Clip,
  wav: PathBuf,
  saved: bool,
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
  level: Level,
  live: Arc<Live>,
  warnings: Vec<String>,
  /// [`push`](Self::push) fails once audio has come through: a resampler error.
  #[cfg(test)]
  fail: bool,
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
      level: Level::default(),
      live: Arc::default(),
      warnings: Vec::new(),
      #[cfg(test)]
      fail: false,
    })
  }

  fn push(&mut self, mut interleaved: &[f32]) -> Result<()> {
    if interleaved.is_empty() {
      return Ok(());
    }

    #[cfg(test)]
    if self.fail && !self.pcm.is_empty() {
      anyhow::bail!("injected");
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

    // A NaN or infinity (a broken driver, a float WAV) would panic the resampler's FFT.
    for s in &mut self.mono {
      if !s.is_finite() {
        *s = 0.0;
      }
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

    self.level.push(&self.out);
    self.live.level.store(self.level.rms.to_bits(), Ordering::Relaxed);
    self.live.silent.store(self.level.silent(), Ordering::Relaxed);
    self.vad.push(&self.out);
    self.live.quiet.store(self.vad.trailing_quiet(QUIET_MAX) as u32, Ordering::Relaxed);
    self.live.scored.store(self.vad.scored() as u64, Ordering::Release);
    self.pcm.extend_from_slice(&self.out);
  }

  /// The recording, also after an error of [`push`](Self::push): the resampler's tail is lost
  /// then, with a warning.
  fn finish(mut self) -> Processed {
    self.out.clear();
    if let Err(e) = self.resampler.finish(&mut self.out) {
      self.warnings.push(format!("the end of the recording could not be processed: {e:#}"));
    }

    self.emit();
    let saved = match self.wav.take().map(WavSink::finalize) {
      Some(Ok(())) => true,
      Some(Err(e)) => {
        self.warnings.push(format!("the recording could not be saved: {e:#}"));
        false
      }
      None => false, // a write error, already a warning
    };

    let dead = self.pcm.len() >= DEAD_SILENCE && self.level.zeros == self.pcm.len();
    let clip = Clip { scores: self.vad.finish(), pcm: self.pcm };
    Processed { clip, wav: self.wav_path, saved, warnings: self.warnings, dead }
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
  use crate::testing::{TempDir, poll, tone};

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

    let level = p.level.rms;
    assert!((level - 0.5 / 2f32.sqrt()).abs() < 0.01, "level {level}");

    let processed = p.finish();
    assert!(processed.warnings.is_empty(), "{:?}", processed.warnings);
    assert!(processed.saved && !processed.dead);
    let pcm = &processed.clip.pcm;
    assert!(pcm.len().abs_diff(16_000) <= 1, "{} samples", pcm.len());
    assert_eq!(processed.clip.scores.len(), pcm.len().div_ceil(FRAME));
    let wav = WavReader::open(&processed.wav).unwrap();
    assert_eq!(wav.len() as usize, pcm.len());
  }

  #[test]
  fn non_finite_samples_are_silence() {
    let dir = TempDir::new("proc-nan");
    let sink = WavSink::create(&dir.path().join("n.wav")).unwrap();
    let mut p = Processor::new(2, 48_000, sink).unwrap();
    let mut input = sine(48_000, 2, 0.5);
    input[1_000] = f32::NAN;
    input[2_001] = f32::INFINITY;
    input[3_000] = f32::NEG_INFINITY;
    p.push(&input).unwrap();
    let processed = p.finish();
    assert!(processed.clip.pcm.len().abs_diff(8_000) <= 1, "{}", processed.clip.pcm.len());
    assert!(processed.clip.pcm.iter().all(|s| s.is_finite()));
  }

  #[test]
  fn digital_silence_is_a_warning() {
    let dir = TempDir::new("proc-zeros");
    let sink = WavSink::create(&dir.path().join("z.wav")).unwrap();
    let mut p = Processor::new(1, 48_000, sink).unwrap();
    p.push(&[0.0; 96_000]).unwrap();
    assert_eq!(p.live.quiet.load(Ordering::Relaxed) as usize, QUIET_MAX, "counted up to a second");
    assert!(p.live.silent.load(Ordering::Relaxed), "said while recording");
    assert!(p.finish().dead);
  }

  #[test]
  fn digital_silence_is_seen_within_a_second_and_sound_ends_it() {
    let mut w = Level::default();
    w.push(&tone(2.0, 0.1));
    assert!(!w.silent());
    w.push(&vec![0.0; SAMPLE_RATE as usize * 9 / 10]);
    assert!(!w.silent(), "0.9 s of zeros: not yet");
    w.push(&vec![0.0; SAMPLE_RATE as usize / 10]);
    assert!(w.silent(), "a second of zeros");
    w.push(&tone(0.1, 0.1));
    assert!(!w.silent(), "sound came back");
  }

  #[test]
  fn a_faint_signal_is_silent_only_after_seconds() {
    let mut w = Level::default();
    // Dither under −80 dBFS: not exact zeros, no sound either.
    w.push(&tone(3.5, 5e-5));
    assert!(!w.silent(), "3.5 s: a pause, not yet");
    w.push(&tone(0.6, 5e-5));
    assert!(w.silent(), "over 4 s");
    // A quiet room (−60 dBFS) is sound.
    let mut room = Level::default();
    room.push(&tone(10.0, 1.4e-3));
    assert!(!room.silent());
  }

  #[test]
  fn a_recording_without_its_file_is_not_saved() {
    let dir = TempDir::new("proc-unsaved");
    let sink = WavSink::create(&dir.path().join("u.wav")).unwrap();
    let mut p = Processor::new(1, SAMPLE_RATE, sink).unwrap();
    p.wav = None; // as after a write error
    p.push(&[0.5; 1_000]).unwrap();
    let processed = p.finish();
    assert!(!processed.saved);
    assert_eq!(processed.clip.pcm.len(), 1_000, "kept in memory");
  }

  /// 44.1 kHz stereo, as a microphone might deliver it.
  const FORMAT: Format = Format { rate: 44_100, channels: 2 };

  /// A recording into `out.wav` from a source the test feeds through the returned [`Feeder`].
  fn record_manual(dir: &TempDir) -> (Recorder, Feeder) {
    let (capture, producer, ring) = capture::manual(FORMAT);
    let signals = capture.signals.clone();
    let sink = WavSink::create(&dir.path().join("out.wav")).unwrap();
    let recorder = Recorder::run(capture, ring, sink, Instant::now()).unwrap();
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
    assert!(rec.saved);
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
    assert!(
      rec.warnings.iter().any(|w| w.contains("recording stopped: The requested")),
      "{:?}",
      rec.warnings
    );
    let wav = WavReader::open(&rec.wav).unwrap();
    assert_eq!(wav.len() as usize, pcm.len());
  }

  /// An error of the processing ends the recording as a device error does: it is said, and the
  /// audio up to it is kept.
  #[test]
  fn a_processing_error_ends_the_recording_and_keeps_the_audio() {
    let dir = TempDir::new("rec-process-error");
    let sink = WavSink::create(&dir.path().join("e.wav")).unwrap();
    let mut p = Processor::new(1, SAMPLE_RATE, sink).unwrap();
    p.fail = true;
    let (mut producer, ring) = rtrb::RingBuffer::new(SAMPLE_RATE as usize);
    let signals = Signals::default();
    let (stop, (failures_tx, failures)) = (AtomicBool::new(false), bounded(1));
    signals.push(&mut producer, &[0.5; 1_000], 1);
    thread::scope(|s| {
      let processing = s.spawn(|| process(ring, p, &stop, &signals, &failures_tx));
      poll("the first audio processed", Duration::from_secs(10), || {
        (producer.slots() == SAMPLE_RATE as usize).then_some(())
      });

      signals.push(&mut producer, &[0.5; 1_000], 1);
      let message = failures.recv_timeout(Duration::from_secs(10)).unwrap();
      assert_eq!(message, "the audio could not be processed: injected");
      let processed = processing.join().unwrap();
      assert_eq!(processed.clip.pcm.len(), 1_000, "what came before the error");
      assert!(processed.saved);
    });

    assert!(signals.failed());
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
