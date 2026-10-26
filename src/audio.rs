// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Audio: input devices, capture (cpal, or a WAV file standing in for the microphone) into an
//! `rtrb` ring, the recorder's thread (downmix, resampling to 16 kHz, level, VAD, the WAV file),
//! WAV files and playback.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender, bounded};
use earshot::Detector;
use hound::{WavReader, WavSpec, WavWriter};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// The engine's sample rate: every model takes 16 kHz mono.
pub const SAMPLE_RATE: u32 = 16_000;

// MARK: Devices

/// A chosen microphone. The ID (CoreAudio's device UID) is matched first: it survives a rename and
/// tells two devices of the same name apart; the name matches when the ID does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
  pub id: Option<String>,
  pub name: String,
}

/// An input device as the settings list it.
#[derive(Debug, Clone)]
pub struct Device {
  pub id: Option<String>,
  pub name: String,
  pub default: bool,
}

struct Input {
  id: Option<String>,
  name: String,
  device: cpal::Device,
}

impl Input {
  fn new(device: cpal::Device) -> Option<Self> {
    let name = device.description().ok()?.name().to_owned(); // `None`: unplugged while listed
    Some(Self { id: device.id().ok().map(|id| id.to_string()), name, device })
  }

  fn is(&self, choice: &Choice) -> bool {
    match &choice.id {
      Some(id) => self.id.as_ref() == Some(id),
      None => self.name == choice.name,
    }
  }
}

fn inputs(host: &cpal::Host) -> Vec<Input> {
  host.input_devices().map(|ds| ds.filter_map(Input::new).collect()).unwrap_or_default()
}

/// Input devices in system order, the system default marked.
pub fn devices() -> Vec<Device> {
  let host = cpal::default_host();
  let default = host.default_input_device();
  let inputs = inputs(&host).into_iter();
  inputs
    .map(|x| Device { default: default.as_ref() == Some(&x.device), id: x.id, name: x.name })
    .collect()
}

/// Opens `choice` (`None`: the system default) with `open`: its name, what `open` made, and a
/// warning when the chosen device is missing or fails and the default is used. The choice stays:
/// once the device is back, the next recording uses it.
fn open_device<T>(
  choice: Option<&Choice>,
  open: impl Fn(&cpal::Device) -> Result<T>,
) -> Result<(String, T, Option<String>)> {
  let host = cpal::default_host();
  let default = host.default_input_device().and_then(Input::new);
  let mut warning = None;
  if let Some(choice) = choice {
    // The default first: usually it is the one, and listing every device is slower.
    let found = match &default {
      Some(d) if d.is(choice) => Some((d.device.clone(), d.name.clone())),
      _ => {
        let all = inputs(&host);
        let by_id = all.iter().position(|x| x.id.is_some() && x.id == choice.id);
        let at = by_id.or_else(|| all.iter().position(|x| x.name == choice.name));
        at.map(|i| (all[i].device.clone(), all[i].name.clone()))
      }
    };

    match found {
      Some((device, name)) => match open(&device) {
        Ok(value) => return Ok((name, value, None)),
        Err(e) => warning = Some(format!("could not open the microphone {:?}: {e:#}", choice.name)),
      },
      None => warning = Some(format!("the microphone {:?} is not connected", choice.name)),
    }
  }

  let Some(Input { name, device, .. }) = default else {
    match warning {
      Some(w) => bail!("{w}, and there is no default microphone"),
      None => bail!("no microphone found"),
    }
  };

  let value = open(&device).map_err(|e| match &warning {
    Some(w) => anyhow!("{w}, and the default microphone {name:?} failed too: {e:#}"),
    None => anyhow!("could not open the microphone {name:?}: {e:#}"),
  })?;
  let warning = warning.map(|w| format!("{w}; using {name:?}"));
  Ok((name, value, warning))
}

/// The first device query in a process costs CoreAudio 40–50 ms: done ahead of the first
/// recording, off the caller's thread.
pub fn warm_up() {
  let warm = || {
    let name = cpal::default_host().default_input_device().and_then(Input::new).map(|x| x.name);
    debug!("audio warmed up (default input {name:?})");
  };

  if let Err(e) = thread::Builder::new().name("audio-warm-up".into()).spawn(warm) {
    debug!("audio warm-up: {e}");
  }
}

// MARK: Capture

/// `$f::<T>(…)` with `T` the sample type of `$format`: devices default to any of them (32-bit
/// integers on Intel HDA and many USB microphones). DSD is no PCM.
macro_rules! by_format {
  ($format:expr, $f:ident($($arg:expr),*)) => {
    match $format {
      SampleFormat::I8 => $f::<i8>($($arg),*),
      SampleFormat::I16 => $f::<i16>($($arg),*),
      SampleFormat::I24 => $f::<cpal::I24>($($arg),*),
      SampleFormat::I32 => $f::<i32>($($arg),*),
      SampleFormat::I64 => $f::<i64>($($arg),*),
      SampleFormat::U8 => $f::<u8>($($arg),*),
      SampleFormat::U16 => $f::<u16>($($arg),*),
      SampleFormat::U24 => $f::<cpal::U24>($($arg),*),
      SampleFormat::U32 => $f::<u32>($($arg),*),
      SampleFormat::U64 => $f::<u64>($($arg),*),
      SampleFormat::F32 => $f::<f32>($($arg),*),
      SampleFormat::F64 => $f::<f64>($($arg),*),
      other => Err(anyhow!("unsupported sample format {other}")),
    }
  };
}

/// The ring holds this much audio; the recorder drains it every [POLL].
const RING_SECONDS: usize = 2;

/// What a recording captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
  /// The chosen microphone, the system default for `None`.
  Mic(Option<Choice>),
  /// A WAV file played in real time, then silence.
  File(PathBuf),
}

impl Source {
  /// The fake microphone ([fake_mic]) if there is one, else the microphone `choice`.
  pub fn new(fake: Option<PathBuf>, choice: Option<Choice>) -> Self {
    fake.map_or(Self::Mic(choice), Self::File)
  }
}

/// `WAVO_FAKE_MIC=<wav>`: the file replaces the microphone.
pub fn fake_mic() -> Option<PathBuf> {
  std::env::var_os("WAVO_FAKE_MIC").filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// A running capture, its samples in the ring that comes with it; dropping it closes the
/// microphone.
struct Capture {
  rate: u32,
  channels: usize,
  device: String,
  warning: Option<String>,
  /// The source's first fatal error: capture has stopped.
  failure: Arc<OnceLock<String>>,
  _feed: Feed,
}

enum Feed {
  Mic { _stream: cpal::Stream },
  File { _mic: FakeMic },
}

fn capture(source: &Source) -> Result<(Capture, Consumer<f32>)> {
  let choice = match source {
    Source::File(path) => return fake_capture(path),
    Source::Mic(choice) => choice.as_ref(),
  };

  let (device, (mut capture, ring), warning) = open_device(choice, open_stream)?;
  (capture.device, capture.warning) = (device, warning);
  Ok((capture, ring))
}

fn open_stream(device: &cpal::Device) -> Result<(Capture, Consumer<f32>)> {
  let supported = device.default_input_config().context("input format")?;
  let config = supported.config();
  let (rate, channels) = (config.sample_rate, config.channels as usize);
  // A multiple of the channels: with whole frames written, a frame never wraps around the end.
  let (producer, ring) = RingBuffer::new(rate as usize * channels * RING_SECONDS);
  let failure = Arc::new(OnceLock::new());
  let format = supported.sample_format();
  let stream = by_format!(format, build(device, config, producer, failure.clone()))?;
  stream.play().context("start the input stream")?;
  let feed = Feed::Mic { _stream: stream };
  let capture =
    Capture { rate, channels, device: String::new(), warning: None, failure, _feed: feed };
  Ok((capture, ring))
}

/// The callback runs on the audio thread: it only converts samples into the ring, no locks and no
/// allocation.
fn build<T: SizedSample>(
  device: &cpal::Device,
  config: cpal::StreamConfig,
  mut ring: Producer<f32>,
  failure: Arc<OnceLock<String>>,
) -> Result<cpal::Stream>
where
  f32: FromSample<T>,
{
  let channels = config.channels as usize;
  let stream = device.build_input_stream(
    config,
    move |samples: &[T], _: &cpal::InputCallbackInfo| push(&mut ring, samples, channels),
    move |e: cpal::Error| match e.kind() {
      // A glitch, a refused real-time priority, a route cpal follows: capture goes on.
      cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {
        debug!("input: {e}")
      }
      _ => _ = failure.set(e.to_string()),
    },
    None,
  )?;
  Ok(stream)
}

/// Pushes the whole frames of `samples` that fit; the rest is lost.
fn push<T: Sample>(ring: &mut Producer<f32>, samples: &[T], channels: usize)
where
  f32: FromSample<T>,
{
  let n = ring.slots().min(samples.len()) / channels * channels;
  if let Ok(chunk) = ring.write_chunk_uninit(n) {
    chunk.fill_from_iter(samples.iter().map(|s| s.to_sample::<f32>()));
  }
}

/// A thread that feeds the file's samples (16 kHz mono) at their real pace, then silence.
struct FakeMic {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

impl Drop for FakeMic {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    let _ = self.thread.take().unwrap().join();
  }
}

fn fake_capture(path: &Path) -> Result<(Capture, Consumer<f32>)> {
  let pcm = read_wav(path).context("WAVO_FAKE_MIC")?;
  let (mut producer, ring) = RingBuffer::new(SAMPLE_RATE as usize * RING_SECONDS);
  let stop = Arc::new(AtomicBool::new(false));
  let flag = stop.clone();
  let thread = thread::Builder::new().name("fake-mic".into()).spawn(move || {
    let (started, mut sent) = (Instant::now(), 0);
    while !flag.load(Ordering::Relaxed) {
      let due = (started.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE)) as usize;
      let block: Vec<f32> = (sent..due).map(|i| pcm.get(i).copied().unwrap_or(0.0)).collect();
      push(&mut producer, &block, 1);
      sent = due;
      thread::sleep(POLL);
    }
  })?;

  let name = path.file_name().unwrap_or_default().to_string_lossy();
  let capture = Capture {
    rate: SAMPLE_RATE,
    channels: 1,
    device: format!("fake mic ({name})"),
    warning: None,
    failure: Arc::default(),
    _feed: Feed::File { _mic: FakeMic { stop, thread: Some(thread) } },
  };
  Ok((capture, ring))
}

// MARK: Recorder

/// How often the processing thread drains the ring.
const POLL: Duration = Duration::from_millis(10);
/// The level is the RMS of windows this long: ~30 a second.
const LEVEL_WINDOW: usize = SAMPLE_RATE as usize / 30;
/// RMS (linear) under which a window is no sound to speak of: −80 dBFS.
const FAINT: f32 = 1e-4;
/// Faint audio this long: the microphone sends no sound (off, muted, the wrong one).
const NO_SOUND: usize = SAMPLE_RATE as usize * 2;

/// A recording in progress: the microphone is open until [stop](Recorder::stop). A device error
/// mid-recording ends capture but not the recording: the audio up to it is kept.
pub struct Recorder {
  capture: Option<Capture>,
  failure: Arc<OnceLock<String>>,
  live: Arc<Live>,
  stop: Arc<AtomicBool>,
  failures: Receiver<String>,
  thread: Option<JoinHandle<Recording>>,
}

#[derive(Debug, Default)]
struct Live {
  /// RMS of the last window as `f32` bits.
  level: AtomicU32,
  silent: AtomicBool,
}

/// The recording's level and whether the microphone sends any sound, for another thread.
#[derive(Debug, Clone)]
pub struct Meter(Arc<Live>);

impl Meter {
  /// RMS of the last ~33 ms, linear, 0 to 1.
  pub fn level(&self) -> f32 {
    f32::from_bits(self.0.level.load(Ordering::Relaxed))
  }

  /// No sound for a while; `false` again as soon as sound comes.
  pub fn silent(&self) -> bool {
    self.0.silent.load(Ordering::Relaxed)
  }
}

#[derive(Debug)]
pub struct Recording {
  /// The whole recording, 16 kHz mono, with its VAD scores.
  pub clip: Clip,
  /// The WAV file holds the whole recording.
  pub saved: bool,
  pub warnings: Vec<String>,
}

impl Recorder {
  /// Opens `source` and records into `wav`, a new file.
  pub fn start(source: &Source, wav: &Path) -> Result<Self> {
    // The file first: a bad path never opens the microphone.
    let sink = WavSink::create(wav)?;
    let (capture, ring) = capture(source)?;
    let processor = Processor::new(capture.channels, capture.rate, sink)?;
    let live = processor.live.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, failures) = bounded(1);
    let (flag, failure) = (stop.clone(), capture.failure.clone());
    let thread = thread::Builder::new()
      .name("audio".into())
      .spawn(move || process(ring, processor, &flag, &failure, &tx))?;

    let failure = capture.failure.clone();
    Ok(Self { capture: Some(capture), failure, live, stop, failures, thread: Some(thread) })
  }

  /// The device actually recording.
  pub fn device(&self) -> &str {
    &self.capture.as_ref().unwrap().device
  }

  /// Why the device is not the one asked for, if it is not.
  pub fn warning(&self) -> Option<&str> {
    self.capture.as_ref().unwrap().warning.as_deref()
  }

  pub fn meter(&self) -> Meter {
    Meter(self.live.clone())
  }

  /// One message when the device fails mid-recording; capture has ended then.
  pub fn failures(&self) -> &Receiver<String> {
    &self.failures
  }

  pub fn failed(&self) -> bool {
    self.failure.get().is_some()
  }

  /// Closes the microphone, processes what is left and finalizes the WAV file.
  pub fn stop(mut self) -> Result<Recording> {
    let capture = self.capture.take().unwrap();
    let mut warnings: Vec<String> = capture.warning.clone().into_iter().collect();
    let device = capture.device.clone();
    drop(capture); // the ring keeps what it delivered

    let mut rec = self.join()?;
    if let Some(failure) = self.failure.get() {
      warnings.push(format!("the recording stopped: {failure}"));
    }

    warnings.append(&mut rec.warnings);
    let pcm = &rec.clip.pcm;
    if pcm.len() >= SAMPLE_RATE as usize / 2 && pcm.iter().all(|&s| s == 0.0) {
      // A closed laptop lid, privacy settings, or the wrong microphone.
      warnings.push(format!("No sound from {device}: check the microphone"));
    }

    rec.warnings = warnings;
    Ok(rec)
  }

  fn join(&mut self) -> Result<Recording> {
    self.stop.store(true, Ordering::Release);
    let thread = self.thread.take().unwrap();
    thread.thread().unpark(); // no need to wait out the poll
    thread.join().map_err(|_| anyhow!("the audio thread panicked"))
  }
}

impl Drop for Recorder {
  fn drop(&mut self) {
    if self.thread.is_some() {
      let _ = self.join();
    }
  }
}

fn process(
  mut ring: Consumer<f32>,
  mut processor: Processor,
  stop: &AtomicBool,
  failure: &OnceLock<String>,
  failures: &Sender<String>,
) -> Recording {
  loop {
    // The flags before draining: every sample delivered before a stop is processed.
    let stopping = stop.load(Ordering::Acquire);
    let failed = failure.get();
    let chunk = ring.read_chunk(ring.slots()).unwrap();
    let (a, b) = chunk.as_slices();
    processor.push(a);
    processor.push(b);
    chunk.commit_all();
    if let Some(failure) = failed {
      let _ = failures.try_send(failure.clone());
    }

    if stopping || failed.is_some() {
      return processor.finish();
    }

    thread::park_timeout(POLL);
  }
}

/// Interleaved device samples in, 16 kHz mono out to the WAV file, the meter, the VAD and memory.
struct Processor {
  channels: usize,
  mono: Vec<f32>,
  out: Vec<f32>,
  resampler: Resample,
  vad: Vad,
  pcm: Vec<f32>,
  /// `None` after a write error.
  wav: Option<WavSink>,
  warnings: Vec<String>,
  level: Level,
  live: Arc<Live>,
}

impl Processor {
  fn new(channels: usize, rate: u32, wav: WavSink) -> Result<Self> {
    Ok(Self {
      channels,
      mono: Vec::new(),
      out: Vec::new(),
      resampler: Resample::new(rate, SAMPLE_RATE)?,
      vad: Vad::default(),
      pcm: Vec::new(),
      wav: Some(wav),
      warnings: Vec::new(),
      level: Level::default(),
      live: Arc::default(),
    })
  }

  fn push(&mut self, interleaved: &[f32]) {
    self.mono.clear();
    self.mono.extend(interleaved.chunks_exact(self.channels).map(downmix));
    self.out.clear();
    self.resampler.push(&self.mono, &mut self.out);
    self.emit();
  }

  fn emit(&mut self) {
    if let Some(wav) = &mut self.wav
      && let Err(e) = wav.write(&self.out)
    {
      self.warnings.push(format!("the recording could not be saved: {e:#}"));
      self.wav = None;
    }

    self.level.push(&self.out);
    self.live.level.store(self.level.rms.to_bits(), Ordering::Relaxed);
    self.live.silent.store(self.level.faint >= NO_SOUND, Ordering::Relaxed);
    self.vad.push(&self.out);
    self.pcm.extend_from_slice(&self.out);
  }

  fn finish(mut self) -> Recording {
    self.out.clear();
    self.resampler.finish(&mut self.out);
    self.emit();
    let saved = match self.wav.take().map(WavSink::finalize) {
      Some(Err(e)) => {
        self.warnings.push(format!("the recording could not be saved: {e:#}"));
        false
      }
      done => done.is_some(),
    };

    let clip = Clip { scores: self.vad.finish(), pcm: self.pcm };
    Recording { clip, saved, warnings: self.warnings }
  }
}

/// A frame of interleaved samples as one; a NaN or infinity (a broken driver) would panic the
/// resampler's FFT, so it is silence.
fn downmix(frame: &[f32]) -> f32 {
  let s = frame.iter().sum::<f32>() / frame.len() as f32;
  if s.is_finite() { s } else { 0.0 }
}

/// The RMS of the last whole [LEVEL_WINDOW], and how long the samples have been faint.
#[derive(Debug, Default)]
struct Level {
  rms: f32,
  faint: usize,
  sum: f64,
  n: usize,
}

impl Level {
  fn push(&mut self, samples: &[f32]) {
    for &s in samples {
      self.sum += f64::from(s * s);
      self.n += 1;
      if self.n == LEVEL_WINDOW {
        self.rms = (self.sum / LEVEL_WINDOW as f64).sqrt() as f32;
        self.faint = if self.rms < FAINT { self.faint + LEVEL_WINDOW } else { 0 };
        (self.sum, self.n) = (0.0, 0);
      }
    }
  }
}

// MARK: Resample

/// Chunk size hint for the FFT resampler; the real input chunk is rounded to fit the ratio.
const CHUNK: usize = 1024;

/// Streaming mono resampler (rubato's FFT one, startup delay trimmed): input in any chunk sizes,
/// and on [finish](Resample::finish) the tail, so the output is as long as the input.
struct Resample {
  /// `None` at the same rate: samples pass through.
  fft: Option<Fft<f32>>,
  ratio: f64,
  pending: Vec<f32>,
  scratch: Vec<f32>,
  /// Output frames of startup delay still to drop.
  delay: usize,
  fed: usize,
  produced: usize,
}

impl Resample {
  fn new(from: u32, to: u32) -> Result<Self> {
    let fft = match from == to {
      true => None,
      false => Some(
        Fft::<f32>::new(from as usize, to as usize, CHUNK, 1, FixedSync::Both)
          .map_err(|e| anyhow!("resampler {from} Hz → {to} Hz: {e}"))?,
      ),
    };

    let delay = fft.as_ref().map_or(0, |f| f.output_delay());
    let scratch = vec![0.0; fft.as_ref().map_or(0, |f| f.output_frames_max())];
    let ratio = f64::from(to) / f64::from(from);
    Ok(Self { fft, ratio, pending: Vec::new(), scratch, delay, fed: 0, produced: 0 })
  }

  /// Appends the resampled samples that are ready to `out`.
  fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
    let Some(fft) = &mut self.fft else { return out.extend_from_slice(input) };
    self.pending.extend_from_slice(input);
    self.fed += input.len();
    let need = fft.input_frames_next();
    let mut offset = 0;
    while self.pending.len() - offset >= need {
      let n = process_fft(fft, &self.pending[offset..offset + need], &mut self.scratch, None);
      offset += need;
      emit(&self.scratch[..n], &mut self.delay, &mut self.produced, out);
    }

    self.pending.drain(..offset);
  }

  /// The input still buffered and the filter's tail.
  fn finish(&mut self, out: &mut Vec<f32>) {
    let Some(fft) = &mut self.fft else { return };
    let expected = (self.fed as f64 * self.ratio).ceil() as usize;
    let mut partial = self.pending.len();
    self.pending.resize(fft.input_frames_next(), 0.0);

    // The last partial chunk, then silence until the delayed output has caught up.
    while self.produced < expected {
      let indexing = Indexing::new().partial_len(partial);
      let n = process_fft(fft, &self.pending, &mut self.scratch, Some(&indexing));
      emit(&self.scratch[..n], &mut self.delay, &mut self.produced, out);
      partial = 0;
    }

    out.truncate(out.len() - (self.produced - expected));
  }
}

fn process_fft(fft: &mut Fft<f32>, input: &[f32], out: &mut [f32], at: Option<&Indexing>) -> usize {
  let frames = out.len();
  let input = InterleavedSlice::new(input, 1, input.len()).unwrap();
  let mut out = InterleavedSlice::new_mut(out, 1, frames).unwrap();
  fft.process_into_buffer(&input, &mut out, at).expect("buffers sized by the resampler").1
}

fn emit(chunk: &[f32], delay: &mut usize, produced: &mut usize, out: &mut Vec<f32>) {
  let skip = (*delay).min(chunk.len());
  *delay -= skip;
  out.extend_from_slice(&chunk[skip..]);
  *produced += chunk.len() - skip;
}

// MARK: VAD

/// Earshot's frame: 256 samples, 16 ms at 16 kHz.
const FRAME: usize = 256;
/// A frame scoring at least this is speech.
const VOICED: f32 = 0.5;
/// A frame scoring below this is a pause; between the two it is neither: never speech, never cut
/// out. Digital silence scores 0.1–0.3, quiet noise up to 0.4, speech mostly above 0.8.
const PAUSE: f32 = 0.35;
/// Speech needs at least this much voiced audio; with less the engine is not called.
const MIN_SPEECH_MS: usize = 150;
/// Silence kept before the first and after the last speech in the engine's input.
const EDGE_MS: usize = 300;
/// Pauses longer than this are shortened in the engine's input to [KEPT_PAUSE_MS], half of it
/// kept on each side.
const LONG_PAUSE_MS: usize = 1000;
const KEPT_PAUSE_MS: usize = 300;

/// Frames in `ms`, rounded down.
const fn frames(ms: usize) -> usize {
  ms * SAMPLE_RATE as usize / 1000 / FRAME
}

/// Streaming detector: 16 kHz samples in any chunk sizes, one score per frame.
struct Vad {
  detector: Box<Detector>,
  pending: Vec<f32>,
  scores: Vec<f32>,
}

impl Default for Vad {
  fn default() -> Self {
    Self { detector: Detector::default_boxed(), pending: Vec::new(), scores: Vec::new() }
  }
}

impl Vad {
  fn push(&mut self, mut pcm: &[f32]) {
    while !pcm.is_empty() {
      let take = (FRAME - self.pending.len()).min(pcm.len());
      self.pending.extend_from_slice(&pcm[..take]);
      pcm = &pcm[take..];
      if self.pending.len() == FRAME {
        self.score();
      }
    }
  }

  /// All scores; a trailing partial frame is padded with silence, so frame `i` always covers
  /// samples `i * FRAME..`.
  fn finish(mut self) -> Vec<f32> {
    if !self.pending.is_empty() {
      self.pending.resize(FRAME, 0.0);
      self.score();
    }

    self.scores
  }

  fn score(&mut self) {
    // Earshot wants [-1, 1]; resampling can overshoot a full-scale signal a little.
    self.pending.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
    self.scores.push(self.detector.predict_f32(&self.pending));
    self.pending.clear();
  }
}

/// 16 kHz mono audio with its VAD scores, one per [FRAME] (the last frame may be partial).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Clip {
  pub pcm: Vec<f32>,
  pub scores: Vec<f32>,
}

impl Clip {
  pub fn analyze(pcm: Vec<f32>) -> Self {
    let mut vad = Vad::default();
    vad.push(&pcm);
    Self { scores: vad.finish(), pcm }
  }

  pub fn seconds(&self) -> f64 {
    self.pcm.len() as f64 / f64::from(SAMPLE_RATE)
  }

  pub fn duration_ms(&self) -> i64 {
    self.pcm.len() as i64 * 1000 / i64::from(SAMPLE_RATE)
  }

  fn voiced_frames(&self) -> usize {
    self.scores.iter().filter(|&&s| s >= VOICED).count()
  }

  pub fn has_speech(&self) -> bool {
    self.voiced_frames() >= frames(MIN_SPEECH_MS)
  }

  /// What the engine gets: `None` without speech, else the clip with silent edges trimmed to
  /// [EDGE_MS] and long pauses shortened. Only whole pause frames are cut, so the scores stay in
  /// step with the audio; the recording on disk keeps everything.
  pub fn engine_input(&self) -> Option<Clip> {
    if !self.has_speech() {
      return None;
    }

    let n = self.scores.len();
    let (edge, long, kept) = (frames(EDGE_MS), frames(LONG_PAUSE_MS), frames(KEPT_PAUSE_MS));
    let mut cut = Vec::new();
    let mut start = None;
    for i in 0..=n {
      let pause = i < n && self.scores[i] < PAUSE;
      match (pause, start) {
        (true, None) => start = Some(i),
        (false, Some(a)) => {
          start = None;
          let len = i - a;
          if a == 0 && len > edge {
            cut.push(0..i - edge);
          } else if i == n && len > edge {
            cut.push(a + edge..n);
          } else if a > 0 && i < n && len > long {
            cut.push(a + kept / 2..i - (kept - kept / 2));
          }
        }
        _ => {}
      }
    }

    let sample = |frame: usize| (frame * FRAME).min(self.pcm.len());
    let mut out = Clip::default();
    let mut from = 0;
    for range in cut.into_iter().chain(std::iter::once(n..n)) {
      out.pcm.extend_from_slice(&self.pcm[sample(from)..sample(range.start)]);
      out.scores.extend_from_slice(&self.scores[from..range.start]);
      from = range.end;
    }

    Some(out)
  }
}

// MARK: WAV

/// Any PCM WAV (integer or float, any channels and rate) as 16 kHz mono in [-1, 1].
pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
  let mut reader = WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
  let spec = reader.spec();
  let samples: Vec<f32> = match spec.sample_format {
    hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    hound::SampleFormat::Int => {
      let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
      reader.samples::<i32>().map(|s| s.map(|s| s as f32 * scale)).collect::<Result<_, _>>()?
    }
  };

  let mono: Vec<f32> = samples.chunks_exact(spec.channels.max(1) as usize).map(downmix).collect();
  let mut resampler = Resample::new(spec.sample_rate, SAMPLE_RATE)?;
  let mut pcm = Vec::new();
  resampler.push(&mono, &mut pcm);
  resampler.finish(&mut pcm);
  Ok(pcm)
}

/// The streaming writer of a recording: 16 kHz mono 16-bit. The header is rewritten with every
/// write, so a killed process leaves a readable file.
pub struct WavSink(WavWriter<BufWriter<File>>);

impl WavSink {
  /// A new file, never over an existing one, and its folder.
  pub fn create(path: &Path) -> Result<Self> {
    let spec = WavSpec {
      channels: 1,
      sample_rate: SAMPLE_RATE,
      bits_per_sample: 16,
      sample_format: hound::SampleFormat::Int,
    };

    fs::create_dir_all(path.parent().unwrap())?;
    let file = File::create_new(path).with_context(|| format!("create {}", path.display()))?;
    Ok(Self(WavWriter::new(BufWriter::new(file), spec)?))
  }

  pub fn write(&mut self, pcm: &[f32]) -> Result<()> {
    for &s in pcm {
      // `as` saturates, and makes a NaN 0.
      self.0.write_sample((s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)?;
    }

    self.0.flush()?;
    Ok(())
  }

  pub fn finalize(self) -> Result<()> {
    Ok(self.0.finalize()?)
  }
}

// MARK: Playback

/// A recording played on the default output through cpal, resampled to the output's rate (the
/// same path on every system); dropping it stops it.
pub struct Playback {
  play: Arc<Play>,
  _stream: cpal::Stream,
}

struct Play {
  done: AtomicBool,
  error: OnceLock<String>,
  ended: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Play {
  /// Played to its end, or failed: `ended` runs once, on the audio thread.
  fn end(&self, error: Option<String>) {
    if self.done.load(Ordering::Acquire) {
      return;
    }

    if let Some(e) = error {
      warn!("playback: {e}");
      let _ = self.error.set(e);
    }

    if !self.done.swap(true, Ordering::AcqRel)
      && let Some(ended) = self.ended.lock().unwrap().take()
    {
      ended();
    }
  }
}

impl Playback {
  /// `ended` runs when it has played to its end or could not play ([error](Playback::error)),
  /// not when it is stopped.
  pub fn start(path: &Path, ended: impl FnOnce() + Send + 'static) -> Result<Self> {
    let pcm = read_wav(path)?;
    let device = cpal::default_host().default_output_device().context("no speakers")?;
    let supported = device.default_output_config().context("output format")?;
    let config = supported.config();
    let mut resampler = Resample::new(SAMPLE_RATE, config.sample_rate)?;
    let mut out = Vec::new();
    resampler.push(&pcm, &mut out);
    resampler.finish(&mut out);

    let ended: Box<dyn FnOnce() + Send> = Box::new(ended);
    let play = Arc::new(Play {
      done: AtomicBool::new(false),
      error: OnceLock::new(),
      ended: Mutex::new(Some(ended)),
    });
    let format = supported.sample_format();
    let stream = by_format!(format, output(&device, config, out, play.clone()))?;
    stream.play().context("start the output stream")?;
    Ok(Self { play, _stream: stream })
  }

  /// Not done yet.
  pub fn playing(&self) -> bool {
    !self.play.done.load(Ordering::Acquire)
  }

  /// Why it could not play, once it is done.
  pub fn error(&self) -> Option<String> {
    self.play.error.get().cloned()
  }
}

/// The callback runs on the audio thread: each sample to every channel, then silence.
fn output<T: SizedSample + FromSample<f32>>(
  device: &cpal::Device,
  config: cpal::StreamConfig,
  pcm: Vec<f32>,
  play: Arc<Play>,
) -> Result<cpal::Stream> {
  let channels = config.channels as usize;
  let failed = play.clone();
  let mut at = 0;
  let stream = device.build_output_stream(
    config,
    move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
      for frame in out.chunks_mut(channels) {
        frame.fill(T::from_sample(pcm.get(at).copied().unwrap_or(0.0)));
        at += 1;
      }

      if at >= pcm.len() {
        play.end(None);
      }
    },
    move |e: cpal::Error| match e.kind() {
      cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {
        debug!("output: {e}")
      }
      _ => failed.end(Some(e.to_string())),
    },
    None,
  )?;
  Ok(stream)
}

#[cfg(test)]
mod tests {
  use std::f32::consts::TAU;

  use super::*;
  use crate::testing::{TempDir, fixture_pcm, tone};

  fn sine(rate: u32, seconds: f32) -> Vec<f32> {
    let n = (rate as f32 * seconds) as usize;
    (0..n).map(|i| (TAU * 440.0 * i as f32 / rate as f32).sin() * 0.5).collect()
  }

  #[test]
  fn resampling_keeps_duration_and_pitch() {
    // Down to 16 kHz for recordings, up to the output's rate for playback.
    let pairs = [16_000, 22_050, 44_100, 48_000].map(|r| (r, SAMPLE_RATE));
    for (from, to) in pairs.into_iter().chain([(SAMPLE_RATE, 44_100), (SAMPLE_RATE, 48_000)]) {
      let mut r = Resample::new(from, to).unwrap();
      let mut out = Vec::new();
      for chunk in sine(from, 2.0).chunks(441) {
        r.push(chunk, &mut out);
      }

      r.finish(&mut out);
      let at = |secs: f32| (secs * to as f32) as usize;
      assert!(out.len().abs_diff(at(2.0)) <= 1, "{from} → {to} Hz: {} samples", out.len());
      // 440 Hz crosses zero 880 times a second; the edges, where the filter rings, are left out.
      let mid = &out[at(0.1)..at(1.9)];
      let crossings = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
      assert!(crossings.abs_diff(1_584) <= 4, "{from} → {to} Hz: {crossings} crossings");
      let peak = mid.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      assert!((peak - 0.5).abs() < 0.02, "{from} → {to} Hz: peak {peak}");
    }
  }

  #[test]
  fn every_pcm_format_has_its_sample_type() {
    fn format<T: SizedSample>() -> Result<SampleFormat> {
      Ok(T::FORMAT)
    }

    use SampleFormat::*;
    for f in [I8, I16, I24, I32, I64, U8, U16, U24, U32, U64, F32, F64] {
      assert_eq!(by_format!(f, format()).unwrap(), f);
    }

    assert!(by_format!(DsdU8, format()).is_err());
    let (mut ring, mut got) = RingBuffer::new(4);
    push(&mut ring, &[i32::MIN, 0, 1 << 30, 0], 2);
    let got: Vec<f32> = (0..4).map(|_| got.pop().unwrap()).collect();
    assert_eq!(got, [-1.0, 0.0, 0.5, 0.0]);
  }

  #[test]
  fn processor_downmixes_resamples_and_saves() {
    let dir = TempDir::new("processor");
    let path = dir.path().join("p.wav");
    let mut p = Processor::new(2, 48_000, WavSink::create(&path).unwrap()).unwrap();
    let mut stereo: Vec<f32> = sine(48_000, 1.0).iter().flat_map(|&s| [s, s]).collect();
    stereo[1_000] = f32::NAN;
    for chunk in stereo.chunks(1_000) {
      p.push(chunk);
    }

    assert!((p.level.rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "level {}", p.level.rms);
    let rec = p.finish();
    assert!(rec.saved && rec.warnings.is_empty(), "{:?}", rec.warnings);
    assert!(rec.clip.pcm.len().abs_diff(16_000) <= 1, "{} samples", rec.clip.pcm.len());
    assert_eq!(rec.clip.scores.len(), rec.clip.pcm.len().div_ceil(FRAME));
    assert_eq!(read_wav(&path).unwrap().len(), rec.clip.pcm.len());
  }

  #[test]
  fn a_faint_signal_is_silent_after_seconds_and_sound_ends_it() {
    let mut level = Level::default();
    level.push(&tone(1.5, 5e-5)); // dither under −80 dBFS
    assert!(level.faint < NO_SOUND, "a pause");
    level.push(&vec![0.0; SAMPLE_RATE as usize]);
    assert!(level.faint >= NO_SOUND);
    level.push(&tone(0.1, 1.4e-3)); // a quiet room, −60 dBFS
    assert_eq!(level.faint, 0);
  }

  #[test]
  fn a_device_error_ends_the_recording_and_keeps_the_audio() {
    let dir = TempDir::new("recorder");
    let mic = dir.path().join("mic.wav");
    let spec = WavSpec {
      channels: 2,
      sample_rate: 44_100,
      bits_per_sample: 16,
      sample_format: hound::SampleFormat::Int,
    };
    let mut w = WavWriter::create(&mic, spec).unwrap();
    sine(44_100, 5.0).iter().for_each(|&s| w.write_sample((s * 32_767.0) as i16).unwrap());
    w.finalize().unwrap();

    let path = dir.path().join("day/rec.wav");
    let recorder = Recorder::start(&Source::File(mic), &path).unwrap();
    assert!(recorder.device().contains("mic.wav"), "{}", recorder.device());
    let meter = recorder.meter();
    while meter.level() < 0.3 {
      thread::sleep(Duration::from_millis(5));
    }

    let _ = recorder.failure.set("The requested device is not available".into());
    let failure = recorder.failures().recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(failure.contains("not available"));
    let rec = recorder.stop().unwrap();
    assert!(rec.saved && rec.clip.pcm.iter().any(|s| s.abs() > 0.4));
    assert_eq!(rec.warnings, ["the recording stopped: The requested device is not available"]);
    assert_eq!(read_wav(&path).unwrap().len(), rec.clip.pcm.len());
  }

  /// A clip from `(score, ms)` runs: a tone where the score is voiced, silence elsewhere.
  fn synthetic(runs: &[(f32, usize)]) -> Clip {
    let mut clip = Clip::default();
    for &(score, ms) in runs {
      let n = frames(ms);
      let a = if score >= VOICED { 0.3 } else { 0.0 };
      let start = clip.pcm.len();
      clip.pcm.extend((0..n * FRAME).map(|i| ((start + i) as f32 * 0.172).sin() * a));
      clip.scores.extend(std::iter::repeat_n(score, n));
    }

    clip
  }

  #[test]
  fn speech_needs_enough_voiced_frames() {
    assert_eq!(synthetic(&[(0.1, 5_000)]).engine_input(), None);
    assert!(!synthetic(&[(0.1, 1_000), (0.9, 128), (0.45, 2_000)]).has_speech());
    assert!(synthetic(&[(0.1, 1_000), (0.9, 160), (0.1, 1_000)]).has_speech());
  }

  #[test]
  fn engine_input_trims_edges_and_shortens_long_pauses() {
    let runs = [(0.1, 2_000), (0.9, 1_008), (0.1, 2_496), (0.9, 496), (0.1, 800), (0.9, 496)];
    let clip = synthetic(&[&runs[..], &[(0.1, 1_504)]].concat());
    let input = clip.engine_input().unwrap();
    let (edge, kept) = (frames(EDGE_MS), frames(KEPT_PAUSE_MS));
    let lens = [edge, frames(1_008), kept, frames(496), frames(800), frames(496), edge];
    assert_eq!(input.scores.len(), lens.iter().sum::<usize>());
    assert_eq!(input.pcm.len(), input.scores.len() * FRAME);
    assert_eq!(input.voiced_frames(), clip.voiced_frames(), "no speech is removed");

    // Neither speech nor pause (a soft onset, breathing) is never removed.
    let clip = synthetic(&[(0.45, 2_000), (0.9, 480), (0.45, 3_000), (0.9, 480), (0.4, 1_200)]);
    assert_eq!(clip.engine_input().unwrap(), clip);
  }

  #[test]
  #[ignore = "needs the fixtures: make test-models"]
  fn fixtures_with_and_without_speech() {
    for name in ["silence", "noise"] {
      assert_eq!(Clip::analyze(fixture_pcm(name)).engine_input(), None, "{name}");
    }

    for name in ["ru-short", "ru-typical", "ru-long", "en-short", "en-paragraph", "en-short-44k"] {
      let clip = Clip::analyze(fixture_pcm(name));
      let input = clip.engine_input().unwrap_or_else(|| panic!("{name}: no speech"));
      assert_eq!(input.voiced_frames(), clip.voiced_frames(), "{name}: speech removed");
    }
  }

  #[test]
  #[cfg(target_os = "macos")] // afplay
  fn playback_of_a_missing_file_fails_at_once() {
    let Err(e) = Playback::start(Path::new("/nonexistent/take.wav"), || {}) else { panic!() };
    assert!(format!("{e:#}").contains("take.wav"), "{e:#}");
  }
}
