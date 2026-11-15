// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Audio and video files as the 16 kHz mono the models take, a part at a time, as the wavo CLI
//! reads them but streamed: a 4.5-hour lecture is never in memory whole (6 GB as f32 at 48 kHz
//! stereo). symphonia reads the container and decodes its first audio track, the channels are
//! averaged, [Resample] brings it to 16 kHz, and [Chunks] cuts it at pauses into what one engine
//! job takes.
//!
//! It reads WAV, AIFF, CAF, MP3, M4A and MP4/MOV (AAC, ALAC), FLAC, Ogg Vorbis and Matroska/WebM
//! of those codecs. Opus (Telegram's and WhatsApp's voice messages, most WebM) has no decoder in
//! symphonia: such a file gets a plain error.

use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};
use tracing::warn;

use crate::audio::{Resample, SAMPLE_RATE, downmix};

/// What the file picker offers: the extensions of the formats above, but those that are mostly
/// Opus (`.opus`, `.oga`, `.webm`), which a drop may still bring.
pub const EXTENSIONS: &[&str] = &[
  "wav", "wave", "aif", "aiff", "aifc", "caf", "mp3", "m4a", "m4b", "aac", "mp4", "mov", "flac",
  "ogg", "mka", "mkv",
];

/// Samples per ms at [SAMPLE_RATE].
const PER_MS: usize = SAMPLE_RATE as usize / 1000;

pub struct Decoder {
  format: Box<dyn FormatReader>,
  decoder: Box<dyn AudioDecoder>,
  track: u32,
  rate: u32,
  time_base: Option<TimeBase>,
  duration_ms: Option<i64>,
  resample: Resample,
  /// Frames at the file's rate still to drop: a seek lands before the time it was asked for.
  skip: u64,
  frame: Vec<f32>,
  ended: bool,
}

impl Decoder {
  /// The first audio track of the file at `path`, by its extension and content.
  pub fn open(path: &Path) -> Result<Self> {
    let file = File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
      hint.with_extension(ext);
    }

    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let options = (FormatOptions::default(), MetadataOptions::default());
    let format = symphonia::default::get_probe().probe(&hint, source, options.0, options.1);
    let format = format.map_err(|e| match e {
      Error::IoError(e) if e.kind() != ErrorKind::UnexpectedEof => anyhow!("can't read it: {e}"),
      _ => anyhow!("not an audio or video file Wavo can read"),
    })?;

    let track = format.default_track(TrackType::Audio).context("there is no sound in it")?;
    let params = track.codec_params.as_ref().and_then(|p| p.audio());
    let params = params.context("there is no sound in it")?.clone();
    let rate = bounded(&params)?;
    let time_base = track.time_base;
    let duration_ms = (track.duration.zip(time_base))
      .and_then(|(d, tb)| tb.calc_duration(d))
      .map(|t| t.as_millis() as i64)
      .or_else(|| track.num_frames.map(|n| (n * 1000 / u64::from(rate)) as i64));
    let track = track.id;
    let codecs = symphonia::default::get_codecs();
    let decoder = match codecs.make_audio_decoder(&params, &AudioDecoderOptions::default()) {
      Ok(decoder) => decoder,
      Err(_) if params.codec == CODEC_ID_OPUS => {
        bail!("its sound is Opus, which Wavo can't read yet")
      }
      Err(_) => bail!("its sound is in a format Wavo can't read"),
    };

    let resample = Resample::new(rate, SAMPLE_RATE)?;
    Ok(Self {
      format,
      decoder,
      track,
      rate,
      time_base,
      duration_ms,
      resample,
      skip: 0,
      frame: Vec::new(),
      ended: false,
    })
  }

  /// How long the audio is, if the file tells.
  pub fn duration_ms(&self) -> Option<i64> {
    self.duration_ms
  }

  /// Goes to `ms` into the audio, before the first [next](Self::next): where the format can't
  /// seek, the audio before it is decoded and dropped.
  pub fn seek(&mut self, ms: i64) -> Result<()> {
    let frames = |secs: f64| (secs * f64::from(self.rate)).round().max(0.0) as u64;
    let secs = ms.max(0) as f64 / 1000.0;
    let time = Time::try_from_secs_f64(secs).context("a time out of range")?;
    match self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track) }) {
      Ok(to) => {
        self.decoder.reset();
        let at = |ts| self.time_base.and_then(|tb| tb.calc_time(ts)).map(|t| t.as_secs_f64());
        let early = match (at(to.required_ts), at(to.actual_ts)) {
          (Some(asked), Some(landed)) => asked - landed,
          // In frames at the file's rate, then.
          _ => (to.required_ts.get() - to.actual_ts.get()) as f64 / f64::from(self.rate),
        };

        self.skip = frames(early);
      }
      Err(e) => {
        warn!("no seek ({e}): decoding the first {secs:.0} s to drop them");
        self.skip = frames(secs);
      }
    }

    Ok(())
  }

  /// The next part of the audio, at 16 kHz mono; `None` after the end. A packet that doesn't
  /// decode is skipped, as players do.
  pub fn next(&mut self) -> Result<Option<Vec<f32>>> {
    if self.ended {
      return Ok(None);
    }

    let mut out = Vec::new();
    loop {
      let packet = match self.format.next_packet() {
        Ok(Some(packet)) => packet,
        Ok(None) => break,
        Err(Error::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => break,
        Err(Error::ResetRequired) => {
          self.decoder.reset();
          continue;
        }
        Err(e) => return Err(e).context("can't read it"),
      };

      if packet.track_id != self.track {
        continue;
      }

      let audio = match self.decoder.decode(&packet) {
        Ok(audio) => audio,
        Err(Error::DecodeError(_)) => continue,
        Err(e) => return Err(e).context("can't decode it"),
      };

      let channels = audio.spec().channels().count().max(1);
      audio.copy_to_vec_interleaved(&mut self.frame);
      let mono: Vec<f32> = self.frame.chunks_exact(channels).map(downmix).collect();
      let skip = mono.len().min(self.skip as usize);
      self.skip -= skip as u64;
      self.resample.push(&mono[skip..], &mut out);
      if !out.is_empty() {
        return Ok(Some(out));
      }
    }

    self.resample.finish(&mut out);
    self.ended = true;
    Ok(Some(out))
  }
}

/// The sample rate of a track with `params`, checked against headers that would make the
/// resampler or the decoder allocate gigabytes (as the wavo CLI does): a rate of GHz, or packets
/// of billions of samples.
fn bounded(params: &AudioCodecParameters) -> Result<u32> {
  let rate = params.sample_rate.context("its sample rate is unknown")?;
  if !(1000..=384_000).contains(&rate) {
    bail!("a sample rate of {rate} Hz: Wavo reads 1 to 384 kHz");
  }

  // FLAC's largest, 65535 frames of 8 channels, is half of it.
  const MAX_PACKET: u64 = 1 << 20;
  let channels = params.channels.as_ref().map_or(1, |c| c.count()) as u64;
  let samples = params.max_frames_per_packet.unwrap_or(0).saturating_mul(channels);
  if samples > MAX_PACKET {
    bail!("packets of {samples} samples: Wavo reads up to {MAX_PACKET}");
  }

  Ok(rate)
}

// MARK: Chunks

/// One engine job's audio: whole pieces between pauses.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
  /// Where it starts in the audio.
  pub start_ms: i64,
  pub pcm: Vec<f32>,
}

impl Chunk {
  pub fn end_ms(&self) -> i64 {
    self.start_ms + (self.pcm.len() / PER_MS) as i64
  }
}

/// A file's audio from a time on, in chunks of up to `max_ms` cut at pauses ([wavo::split]): at
/// most two chunks of it are in memory.
pub struct Chunks {
  decoder: Decoder,
  max_ms: u32,
  buf: Vec<f32>,
  /// Where `buf` starts in the audio, in samples.
  at: usize,
}

impl Chunks {
  /// The audio of the file at `path` from `from_ms` on.
  pub fn open(path: &Path, from_ms: i64, max_ms: u32) -> Result<Self> {
    let mut decoder = Decoder::open(path)?;
    if from_ms > 0 {
      decoder.seek(from_ms)?;
    }

    let at = from_ms.max(0) as usize * PER_MS;
    Ok(Self { decoder, max_ms, buf: Vec::new(), at })
  }

  pub fn duration_ms(&self) -> Option<i64> {
    self.decoder.duration_ms()
  }

  /// The next chunk, `None` after the last.
  pub fn next(&mut self) -> Result<Option<Chunk>> {
    let max = self.max_ms as usize * PER_MS;
    let mut ended = false;
    while self.buf.len() < 2 * max {
      match self.decoder.next()? {
        Some(part) => self.buf.extend_from_slice(&part),
        None => {
          ended = true;
          break;
        }
      }
    }

    if self.buf.is_empty() {
      return Ok(None);
    }

    let cut = match ended && self.buf.len() <= max {
      true => self.buf.len(),
      false => wavo::split(&self.buf, self.max_ms)[0].end,
    };

    let pcm: Vec<f32> = self.buf.drain(..cut).collect();
    let start_ms = (self.at / PER_MS) as i64;
    self.at += cut;
    Ok(Some(Chunk { start_ms, pcm }))
  }
}

#[cfg(test)]
mod tests {
  use std::f32::consts::TAU;
  use std::fs;
  use std::path::PathBuf;

  use hound::{SampleFormat, WavSpec, WavWriter};

  use super::*;
  use crate::testing::TempDir;

  /// `seconds` of a `hz` sine at `rate` in `channels`, the right one at half the level, written
  /// as a WAV of `bits` in `dir`.
  fn wav(dir: &Path, name: &str, rate: u32, channels: u16, bits: u16, seconds: f64) -> PathBuf {
    let format = if bits == 32 { SampleFormat::Float } else { SampleFormat::Int };
    let spec =
      WavSpec { channels, sample_rate: rate, bits_per_sample: bits, sample_format: format };
    let path = dir.join(name);
    let mut w = WavWriter::create(&path, spec).unwrap();
    for i in 0..(seconds * f64::from(rate)) as usize {
      let x = 0.5 * (TAU * 440.0 * i as f32 / rate as f32).sin();
      for c in 0..channels {
        let x = if c == 1 { x / 2.0 } else { x };
        match format {
          SampleFormat::Float => w.write_sample(x).unwrap(),
          SampleFormat::Int => w.write_sample((x * ((1 << (bits - 1)) - 1) as f32) as i32).unwrap(),
        }
      }
    }

    w.finalize().unwrap();
    path
  }

  fn decode_all(d: &mut Decoder) -> Vec<f32> {
    let mut pcm = Vec::new();
    while let Some(part) = d.next().unwrap() {
      pcm.extend(part);
    }

    pcm
  }

  /// Sign changes per second: 880 for a 440 Hz sine.
  fn crossings(pcm: &[f32]) -> f64 {
    let n = pcm.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    n as f64 / (pcm.len() as f64 / 16_000.0)
  }

  fn rms(pcm: &[f32]) -> f32 {
    (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len() as f32).sqrt()
  }

  #[test]
  fn any_wav_becomes_16_khz_mono() {
    let dir = TempDir::new("decode-wav");
    for (rate, channels, bits) in
      [(44_100, 2, 16), (8_000, 1, 16), (48_000, 2, 24), (22_050, 1, 32)]
    {
      let path =
        wav(dir.path(), &format!("{rate}-{channels}-{bits}.wav"), rate, channels, bits, 3.0);
      let mut d = Decoder::open(&path).unwrap();
      assert_eq!(d.duration_ms(), Some(3_000), "{path:?}");
      let pcm = decode_all(&mut d);
      assert!(pcm.len().abs_diff(48_000) <= 16, "{path:?}: {}", pcm.len());
      let hz = crossings(&pcm[1_000..47_000]);
      assert!((hz - 880.0).abs() < 5.0, "{path:?}: {hz}");
      let want = if channels == 2 { 0.75 * 0.5 } else { 0.5 } / 2f32.sqrt();
      assert!((rms(&pcm[1_000..47_000]) - want).abs() < 0.01, "{path:?}: channels averaged");
      assert_eq!(d.next().unwrap(), None);
    }
  }

  /// A seek gives the audio from there on: at 16 kHz the very samples; resampled, the same tone.
  #[test]
  fn a_seek_goes_on_from_its_time() {
    let dir = TempDir::new("decode-seek");
    let plain = wav(dir.path(), "plain.wav", 16_000, 1, 16, 4.0);
    let whole = decode_all(&mut Decoder::open(&plain).unwrap());
    let mut d = Decoder::open(&plain).unwrap();
    d.seek(1_500).unwrap();
    assert_eq!(decode_all(&mut d), whole[24_000..]);

    let stereo = wav(dir.path(), "stereo.wav", 44_100, 2, 16, 4.0);
    let mut d = Decoder::open(&stereo).unwrap();
    d.seek(2_500).unwrap();
    let rest = decode_all(&mut d);
    assert!(rest.len().abs_diff(24_000) <= 160, "{}", rest.len());
    assert!((crossings(&rest[500..]) - 880.0).abs() < 5.0);
  }

  #[test]
  fn plain_errors() {
    let dir = TempDir::new("decode-errors");
    let err = |name: &str, bytes: &[u8]| {
      let path = dir.path().join(name);
      fs::write(&path, bytes).unwrap();
      format!("{:#}", Decoder::open(&path).err().unwrap())
    };

    assert_eq!(
      err("notes.txt", b"just some text, no audio at all"),
      "not an audio or video file Wavo can read"
    );
    assert_eq!(err("empty.mp3", b""), "not an audio or video file Wavo can read");
    assert_eq!(
      err("cut.wav", b"RIFF\x24\0\0\0WAVEfmt "),
      "not an audio or video file Wavo can read"
    );
    let gone = format!("{:#}", Decoder::open(&dir.path().join("gone.wav")).err().unwrap());
    assert!(gone.starts_with("can't open"), "{gone}");
  }

  /// The fixture in each format decodes as long as its WAV and as loud; Opus is refused.
  #[test]
  #[ignore = "needs make fixtures"]
  fn fixtures_in_every_format() {
    let files = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fixtures/files");
    let wav = crate::testing::fixture_pcm("ru-long");
    let names = ["mp3", "m4a", "alac.m4a", "flac", "ogg", "mkv", "aiff", "caf"];
    for path in names
      .iter()
      .map(|n| files.join(format!("ru-long.{n}")))
      .chain([files.join("ru-long-video.mp4")])
    {
      let pcm = decode_all(&mut Decoder::open(&path).unwrap());
      let video = path.ends_with("ru-long-video.mp4");
      let len = if video { pcm.len().min(wav.len()) } else { pcm.len() };
      assert!(len.abs_diff(wav.len()) < 16 * 150, "{path:?}: {} of {}", pcm.len(), wav.len());
      let (got, want) = (rms(&pcm), rms(&wav));
      assert!((got / want - 1.0).abs() < 0.1, "{path:?}: rms {got} of {want}");
    }

    let opus = format!("{:#}", Decoder::open(&files.join("ru-long.opus")).err().unwrap());
    assert_eq!(opus, "its sound is Opus, which Wavo can't read yet");
  }

  /// Chunks cut at pauses, each at most `max_ms`, cover the audio without a sample lost or twice,
  /// and no more than two chunks of it are held.
  #[test]
  fn chunks_cover_the_audio_at_pauses() {
    let dir = TempDir::new("decode-chunks");
    // 10 min of tone with a 400 ms pause every 7 s.
    let path = dir.path().join("long.wav");
    let spec = WavSpec {
      channels: 1,
      sample_rate: 16_000,
      bits_per_sample: 16,
      sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(&path, spec).unwrap();
    let pause = |i: usize| i % 112_000 >= 105_600;
    for i in 0..600 * 16_000 {
      let x = if pause(i) { 0.0 } else { 0.5 * (TAU * 440.0 * i as f32 / 16_000.0).sin() };
      w.write_sample((x * 32_767.0) as i16).unwrap();
    }
    w.finalize().unwrap();

    let mut chunks = Chunks::open(&path, 0, 60_000).unwrap();
    assert_eq!(chunks.duration_ms(), Some(600_000));
    let (mut at, mut n) = (0, 0);
    while let Some(c) = chunks.next().unwrap() {
      assert_eq!(c.start_ms, at, "contiguous");
      assert!(c.pcm.len() <= 60 * 16_000 && !c.pcm.is_empty());
      assert!(chunks.buf.capacity() <= 3 * 60 * 16_000, "{}", chunks.buf.capacity());
      let end = (c.start_ms as usize + c.pcm.len() / 16) * 16;
      if end < 600 * 16_000 {
        assert!(pause(end - 1) || pause(end), "cut in a pause at {end}");
      }

      at = c.end_ms();
      n += 1;
    }

    assert_eq!((at, n > 10), (600_000, true));

    // From a time on, as a resume does.
    let mut rest = Chunks::open(&path, 590_000, 60_000).unwrap();
    let c = rest.next().unwrap().unwrap();
    assert_eq!((c.start_ms, c.end_ms()), (590_000, 600_000));
    assert_eq!(rest.next().unwrap(), None);
  }
}
