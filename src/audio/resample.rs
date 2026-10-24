// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Sample rate conversion of capture streams to the engine's 16 kHz ([`To16k`]), and of a whole
//! clip to any rate ([`convert`]: a recording for the output device), with rubato's FFT
//! resampler: fixed ratio, full quality, startup delay trimmed.

use anyhow::{Result, anyhow};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

use super::SAMPLE_RATE;

/// Chunk size hint for the FFT resampler; the real input chunk is rounded to fit the ratio.
const CHUNK: usize = 1024;

fn fft(from: u32, to: u32) -> Result<Fft<f32>> {
  Fft::<f32>::new(from as usize, to as usize, CHUNK, 1, FixedSync::Both)
    .map_err(|e| anyhow!("resampler {from} Hz → {to} Hz: {e}"))
}

/// A whole mono clip from rate `from` to `to`.
pub fn convert(mono: Vec<f32>, from: u32, to: u32) -> Result<Vec<f32>> {
  if from == to || mono.is_empty() {
    return Ok(mono);
  }

  let mut fft = fft(from, to)?;
  let input = InterleavedSlice::new(&mono, 1, mono.len()).map_err(|e| anyhow!("{e}"))?;
  let output = fft.process_all(&input, mono.len(), None)?;
  Ok(output.take_data())
}

/// Streaming mono resampler to 16 kHz: takes input in any chunk sizes, emits what is ready, and
/// on [`finish`](To16k::finish) the tail, so the output is exactly as long as the whole clip
/// resampled at once.
pub struct To16k {
  /// `None` at 16 kHz: samples pass through.
  fft: Option<Fft<f32>>,
  ratio: f64,
  pending: Vec<f32>,
  scratch: Vec<f32>,
  /// Output frames of startup delay still to drop.
  delay: usize,
  fed: usize,
  produced: usize,
}

impl To16k {
  pub fn new(rate: u32) -> Result<Self> {
    let fft = if rate == SAMPLE_RATE { None } else { Some(fft(rate, SAMPLE_RATE)?) };
    let (delay, scratch) = fft
      .as_ref()
      .map_or((0, Vec::new()), |f| (f.output_delay(), vec![0.0; f.output_frames_max()]));

    Ok(Self {
      ratio: SAMPLE_RATE as f64 / rate as f64,
      pending: Vec::with_capacity(fft.as_ref().map_or(0, |f| 2 * f.input_frames_max())),
      fft,
      scratch,
      delay,
      fed: 0,
      produced: 0,
    })
  }

  /// Resamples `input`, appending the 16 kHz samples that are ready to `out`.
  pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
    let Some(fft) = &mut self.fft else {
      out.extend_from_slice(input);
      return Ok(());
    };

    self.pending.extend_from_slice(input);
    self.fed += input.len();
    let need = fft.input_frames_next();
    let mut offset = 0;
    while self.pending.len() - offset >= need {
      let chunk = &self.pending[offset..offset + need];
      let n = process(fft, chunk, &mut self.scratch, None)?;
      offset += need;
      emit(&self.scratch[..n], &mut self.delay, &mut self.produced, out);
    }

    self.pending.drain(..offset);
    Ok(())
  }

  /// Flushes the input still buffered and the filter tail.
  pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
    let Some(fft) = &mut self.fft else { return Ok(()) };
    let expected = (self.fed as f64 * self.ratio).ceil() as usize;
    let need = fft.input_frames_next();
    let mut partial = self.pending.len();
    self.pending.resize(need, 0.0);

    // The last partial chunk, then silence until the delayed output has caught up.
    while self.produced < expected {
      let indexing = Indexing::new().partial_len(partial);
      let n = process(fft, &self.pending, &mut self.scratch, Some(&indexing))?;
      emit(&self.scratch[..n], &mut self.delay, &mut self.produced, out);
      partial = 0;
    }

    out.truncate(out.len() - (self.produced - expected));
    self.produced = expected;
    self.pending.clear();
    Ok(())
  }
}

fn process(
  fft: &mut Fft<f32>,
  input: &[f32],
  output: &mut [f32],
  indexing: Option<&Indexing>,
) -> Result<usize> {
  let frames_out = output.len();
  let input = InterleavedSlice::new(input, 1, input.len()).map_err(|e| anyhow!("{e}"))?;
  let mut output = InterleavedSlice::new_mut(output, 1, frames_out).map_err(|e| anyhow!("{e}"))?;
  let (_, n) = fft.process_into_buffer(&input, &mut output, indexing)?;
  Ok(n)
}

fn emit(chunk: &[f32], delay: &mut usize, produced: &mut usize, out: &mut Vec<f32>) {
  let skip = (*delay).min(chunk.len());
  *delay -= skip;
  out.extend_from_slice(&chunk[skip..]);
  *produced += chunk.len() - skip;
}

#[cfg(test)]
mod tests {
  use std::f32::consts::TAU;

  use super::*;

  fn sine(freq: f32, rate: u32, seconds: f32) -> Vec<f32> {
    let n = (rate as f32 * seconds) as usize;
    (0..n).map(|i| (TAU * freq * i as f32 / rate as f32).sin() * 0.5).collect()
  }

  fn zero_crossings(x: &[f32]) -> usize {
    x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
  }

  /// The whole clip at once, by rubato itself: what streaming must match.
  fn whole(mono: &[f32], rate: u32) -> Vec<f32> {
    convert(mono.to_vec(), rate, SAMPLE_RATE).unwrap()
  }

  /// `input` pushed in chunks of the sizes in `chunks`, repeated.
  fn stream(input: &[f32], rate: u32, chunks: &[usize]) -> Vec<f32> {
    let mut r = To16k::new(rate).unwrap();
    let mut out = Vec::new();
    let mut rest = input;
    for &n in chunks.iter().cycle() {
      if rest.is_empty() {
        break;
      }

      let (head, tail) = rest.split_at(n.min(rest.len()));
      r.push(head, &mut out).unwrap();
      rest = tail;
    }

    r.finish(&mut out).unwrap();
    out
  }

  #[test]
  fn keeps_duration_and_pitch() {
    for rate in [8_000, 22_050, 44_100, 48_000] {
      let out = stream(&sine(440.0, rate, 2.0), rate, &[4_096]);
      assert!(out.len().abs_diff(32_000) <= 1, "{rate} Hz: {} samples", out.len());

      // 440 Hz for 2 s crosses zero ~1760 times; skip the edges where the filter rings.
      let crossings = zero_crossings(&out[1_600..30_400]);
      assert!(crossings.abs_diff(1_584) <= 4, "{rate} Hz: {crossings} crossings");
      let peak = out[1_600..30_400].iter().fold(0.0f32, |m, s| m.max(s.abs()));
      assert!((peak - 0.5).abs() < 0.02, "{rate} Hz: peak {peak}");
    }
  }

  #[test]
  fn native_rate_and_empty_pass_through() {
    let x = sine(300.0, SAMPLE_RATE, 0.1);
    assert_eq!(stream(&x, SAMPLE_RATE, &[441]), x);
    assert!(stream(&[], 44_100, &[441]).is_empty());
  }

  #[test]
  fn streaming_matches_whole_clip() {
    for rate in [16_000, 22_050, 44_100, 48_000] {
      for seconds in [0.01, 0.3, 1.7] {
        let x = sine(523.0, rate, seconds);
        let whole = whole(&x, rate);
        for chunks in [&[1][..], &[441, 7, 1_000], &[10_000]] {
          if chunks == [1] && x.len() > 20_000 {
            continue; // one sample at a time is slow and covered by the shorter clips
          }

          let streamed = stream(&x, rate, chunks);
          assert_eq!(streamed.len(), whole.len(), "{rate} Hz, {seconds} s, {chunks:?}");
          let diff = streamed.iter().zip(&whole).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
          assert!(diff < 1e-5, "{rate} Hz, {seconds} s, {chunks:?}: max diff {diff}");
        }
      }
    }
  }
}
