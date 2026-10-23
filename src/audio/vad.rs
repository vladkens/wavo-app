// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Voice activity detection (earshot) on 16 kHz mono, scored per 16 ms frame. A [`Clip`] is audio
//! with its scores.

use earshot::Detector;

use super::SAMPLE_RATE;

/// Earshot's frame: 256 samples, 16 ms at 16 kHz.
pub const FRAME: usize = 256;
/// A frame scoring at least this is speech. Digital silence scores 0.1–0.3, quiet noise up to 0.4,
/// speech mostly above 0.8.
pub const VOICED: f32 = 0.5;

/// Streaming detector: feed 16 kHz samples in any chunk sizes, get one score per frame.
pub struct Vad {
  detector: Box<Detector>,
  pending: Vec<f32>,
  scores: Vec<f32>,
}

impl Default for Vad {
  fn default() -> Self {
    Self {
      detector: Detector::default_boxed(),
      pending: Vec::with_capacity(FRAME),
      scores: Vec::new(),
    }
  }
}

impl Vad {
  pub fn push(&mut self, mut pcm: &[f32]) {
    while !pcm.is_empty() {
      let take = (FRAME - self.pending.len()).min(pcm.len());
      self.pending.extend_from_slice(&pcm[..take]);
      pcm = &pcm[take..];
      if self.pending.len() == FRAME {
        self.score_pending();
      }
    }
  }

  /// All scores; a trailing partial frame is scored padded with silence, so frame `i` always
  /// covers samples `i * FRAME ..` of the audio.
  pub fn finish(mut self) -> Vec<f32> {
    if !self.pending.is_empty() {
      self.pending.resize(FRAME, 0.0);
      self.score_pending();
    }

    self.scores
  }

  fn score_pending(&mut self) {
    // Earshot wants [-1, 1]; resampling can overshoot a full-scale signal a little.
    for s in &mut self.pending {
      *s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
    }

    self.scores.push(self.detector.predict_f32(&self.pending));
    self.pending.clear();
  }
}

/// 16 kHz mono audio with its VAD scores, one per [`FRAME`] (the last frame may be partial).
#[derive(Debug)]
pub struct Clip {
  pub pcm: Vec<f32>,
  pub scores: Vec<f32>,
}

impl Clip {
  pub fn seconds(&self) -> f64 {
    self.pcm.len() as f64 / SAMPLE_RATE as f64
  }

  pub fn voiced_frames(&self) -> usize {
    self.scores.iter().filter(|&&s| s >= VOICED).count()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn scores(pcm: &[f32]) -> Vec<f32> {
    let mut vad = Vad::default();
    vad.push(pcm);
    vad.finish()
  }

  #[test]
  fn streaming_matches_one_shot_and_pads_the_last_frame() {
    let pcm: Vec<f32> = (0..FRAME * 5 + 100).map(|i| ((i as f32) * 0.05).sin() * 0.3).collect();
    let whole = scores(&pcm);
    assert_eq!(whole.len(), 6);

    let mut vad = Vad::default();
    for chunk in pcm.chunks(77) {
      vad.push(chunk);
    }

    assert_eq!(vad.scores.len(), 5);
    assert_eq!(vad.finish(), whole);
  }

  #[test]
  fn silence_is_not_speech() {
    let pcm = vec![0.0; SAMPLE_RATE as usize];
    let clip = Clip { scores: scores(&pcm), pcm };
    assert_eq!(clip.voiced_frames(), 0);
    assert_eq!(clip.seconds(), 1.0);
  }

  #[test]
  fn tolerates_overshoot_and_nan() {
    let mut pcm = vec![1.5; FRAME];
    pcm[3] = f32::NAN;
    assert!(scores(&pcm)[0].is_finite());
  }
}
