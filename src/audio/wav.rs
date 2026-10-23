// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! WAV files: reading any PCM WAV as it is stored, and [`WavSink`], the streaming writer of
//! recordings (16 kHz mono 16-bit).

use std::fs::{self, File};
use std::io::{self, BufWriter, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

use super::SAMPLE_RATE;

/// Interleaved samples of a WAV file as they are stored, scaled to `[-1, 1]`.
pub struct Interleaved {
  pub samples: Vec<f32>,
  pub rate: u32,
  pub channels: u16,
}

/// Any PCM WAV (8/16/24/32-bit integer or 32-bit float, any channel count and rate), without
/// conversion of rate or channels.
pub fn read_interleaved(path: &Path) -> Result<Interleaved> {
  let mut reader = WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
  let spec = reader.spec();
  let samples: Vec<f32> = match spec.sample_format {
    SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    SampleFormat::Int => {
      let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
      reader.samples::<i32>().map(|s| s.map(|s| s as f32 * scale)).collect::<Result<_, _>>()?
    }
  };

  if spec.channels == 0 {
    bail!("{}: no channels", path.display());
  }

  Ok(Interleaved { samples, rate: spec.sample_rate, channels: spec.channels })
}

/// Creates `path`, never over an existing file, and its folder if it is missing: again if the
/// folder is removed before the file is in it.
fn create_file(path: &Path) -> io::Result<File> {
  let mut tries = 0;
  loop {
    match File::create_new(path) {
      Err(e) if e.kind() == ErrorKind::NotFound && tries < 3 => {
        tries += 1;
        if let Some(dir) = path.parent() {
          fs::create_dir_all(dir)?;
        }
      }
      result => return result,
    }
  }
}

/// Streaming writer of a recording: 16 kHz mono 16-bit PCM. The header is rewritten every second
/// of audio, so the file stays readable up to the last second even if the process is killed.
pub struct WavSink {
  writer: WavWriter<BufWriter<File>>,
  path: PathBuf,
  unflushed: usize,
}

impl WavSink {
  /// Samples between header updates: one second.
  pub const FLUSH_EVERY: usize = SAMPLE_RATE as usize;

  /// Creates the file (an existing one is an error), and its folder if it is missing.
  pub fn create(path: &Path) -> Result<Self> {
    let spec = WavSpec {
      channels: 1,
      sample_rate: SAMPLE_RATE,
      bits_per_sample: 16,
      sample_format: SampleFormat::Int,
    };
    let file = create_file(path).with_context(|| format!("create {}", path.display()))?;

    // A valid, empty file from the start, or none (a full disk).
    let writer = WavWriter::new(BufWriter::new(file), spec)
      .map_err(anyhow::Error::from)
      .and_then(|mut w| w.flush().map(|()| w).map_err(anyhow::Error::from));
    match writer {
      Ok(writer) => Ok(Self { writer, path: path.to_owned(), unflushed: 0 }),
      Err(e) => {
        let _ = fs::remove_file(path);
        Err(e).with_context(|| format!("write {}", path.display()))
      }
    }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn write(&mut self, pcm: &[f32]) -> Result<()> {
    let mut rest = pcm;
    while !rest.is_empty() {
      let take = (Self::FLUSH_EVERY - self.unflushed).min(rest.len());
      let mut w = self.writer.get_i16_writer(take as u32);
      for &s in &rest[..take] {
        w.write_sample(to_i16(s));
      }

      w.flush().with_context(|| format!("write {}", self.path.display()))?;
      rest = &rest[take..];
      self.unflushed += take;
      if self.unflushed == Self::FLUSH_EVERY {
        self.writer.flush().with_context(|| format!("write {}", self.path.display()))?;
        self.unflushed = 0;
      }
    }

    Ok(())
  }

  pub fn finalize(self) -> Result<()> {
    let path = self.path;
    self.writer.finalize().with_context(|| format!("finalize {}", path.display()))
  }
}

fn to_i16(s: f32) -> i16 {
  let s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
  (s * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::TempDir;

  fn write<S: hound::Sample + Copy>(path: &Path, spec: WavSpec, samples: &[S]) {
    let mut w = WavWriter::create(path, spec).unwrap();
    for &s in samples {
      w.write_sample(s).unwrap();
    }

    w.finalize().unwrap();
  }

  #[test]
  fn reads_stereo_int_as_stored() {
    let dir = TempDir::new("wav-stereo");
    let path = dir.path().join("x.wav");
    let spec = WavSpec {
      channels: 2,
      sample_rate: 48_000,
      bits_per_sample: 16,
      sample_format: SampleFormat::Int,
    };
    write(&path, spec, &[i16::MAX / 2, 0, i16::MIN, -1]);

    let wav = read_interleaved(&path).unwrap();
    assert_eq!((wav.rate, wav.channels), (48_000, 2));
    assert_eq!(wav.samples, [16_383.0 / 32_768.0, 0.0, -1.0, -1.0 / 32_768.0]);
  }

  #[test]
  fn reads_mono_float_unchanged() {
    let dir = TempDir::new("wav-float");
    let path = dir.path().join("x.wav");
    let spec = WavSpec {
      channels: 1,
      sample_rate: 16_000,
      bits_per_sample: 32,
      sample_format: SampleFormat::Float,
    };
    let samples = [0.0f32, 0.25, -0.5, 1.0];
    write(&path, spec, &samples);
    assert_eq!(read_interleaved(&path).unwrap().samples, samples);
  }

  #[test]
  fn missing_file_names_the_path() {
    let Err(err) = read_interleaved(Path::new("/nonexistent/wavo.wav")) else { panic!() };
    assert!(format!("{err:#}").contains("/nonexistent/wavo.wav"));
  }

  fn header_samples(path: &Path) -> u32 {
    WavReader::open(path).unwrap().len()
  }

  #[test]
  fn sink_creates_its_folder() {
    let dir = TempDir::new("wav-sink-folder");
    let path = dir.path().join("2026-10-12").join("rec.wav");
    WavSink::create(&path).unwrap().finalize().unwrap();
    assert_eq!(header_samples(&path), 0);
  }

  #[test]
  fn sink_keeps_the_file_valid_every_second() {
    let dir = TempDir::new("wav-sink");
    let path = dir.path().join("rec.wav");
    let mut sink = WavSink::create(&path).unwrap();
    assert_eq!(header_samples(&path), 0, "valid while still empty");

    let pcm: Vec<f32> = (0..40_000).map(|i| ((i % 100) as f32 / 50.0) - 1.0).collect();
    // Odd chunk sizes cross the flush points.
    for chunk in pcm[..20_000].chunks(333) {
      sink.write(chunk).unwrap();
    }

    // Without finalize (a killed process), the header covers every whole second written.
    assert_eq!(header_samples(&path), 16_000);
    for chunk in pcm[20_000..].chunks(4_096) {
      sink.write(chunk).unwrap();
    }

    assert_eq!(header_samples(&path), 32_000);
    sink.finalize().unwrap();

    let back = read_interleaved(&path).unwrap();
    assert_eq!((back.rate, back.channels), (SAMPLE_RATE, 1));
    assert_eq!(back.samples.len(), pcm.len());
    let diff = back.samples.iter().zip(&pcm).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(diff < 1.0 / 16_000.0, "16-bit roundtrip error {diff}");
  }

  #[test]
  fn sink_clamps_out_of_range_and_nan() {
    assert_eq!(to_i16(2.0), i16::MAX);
    assert_eq!(to_i16(-2.0), -i16::MAX);
    assert_eq!(to_i16(f32::NAN), 0);
    assert_eq!(to_i16(0.5), 16_384);
  }
}
