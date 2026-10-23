// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Voice activity detection (earshot) on 16 kHz mono, scored per 16 ms frame. A [`Clip`] is audio
//! with its scores: it says whether there is speech at all and gives the engine's input (silent
//! edges trimmed, long pauses shortened).

use std::ops::Range;
use std::time::Duration;

use earshot::Detector;

use super::SAMPLE_RATE;

/// Earshot's frame: 256 samples, 16 ms at 16 kHz.
pub const FRAME: usize = 256;
/// A frame scoring at least this is speech.
pub const VOICED: f32 = 0.5;
/// A frame scoring below this is a pause. Between the two thresholds a frame is neither: it never
/// counts as speech, and it is never cut out. Digital silence scores 0.1–0.3, quiet noise up to
/// 0.4, speech mostly above 0.8.
pub const SILENT: f32 = 0.35;

/// Speech needs at least this much voiced audio; with less the engine is not called.
pub const MIN_SPEECH_MS: usize = 150;
/// Silence kept before the first and after the last speech in the engine's input.
pub const EDGE_MS: usize = 300;
/// Pauses longer than this are shortened in the engine's input…
pub const LONG_PAUSE_MS: usize = 1000;
/// …to this: half of it kept after the speech before, half before the speech after.
pub const KEPT_PAUSE_MS: usize = 300;

/// Frames in `ms` milliseconds (16 ms each), rounded down.
pub const fn frames(ms: usize) -> usize {
  ms * SAMPLE_RATE as usize / 1000 / FRAME
}

/// The length of `frames` frames.
pub fn duration(frames: usize) -> Duration {
  Duration::from_secs_f64((frames * FRAME) as f64 / f64::from(SAMPLE_RATE))
}

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

  /// Frames scored so far.
  pub fn scored(&self) -> usize {
    self.scores.len()
  }

  /// Frames at the end of the audio scored so far that are not speech, at most `max`.
  pub fn trailing_quiet(&self, max: usize) -> usize {
    self.scores.iter().rev().take(max).take_while(|&&s| s < VOICED).count()
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
    self.pcm.len() as f64 / SAMPLE_RATE as f64
  }

  pub fn voiced_frames(&self) -> usize {
    self.scores.iter().filter(|&&s| s >= VOICED).count()
  }

  pub fn has_speech(&self) -> bool {
    self.voiced_frames() >= frames(MIN_SPEECH_MS).max(1)
  }

  /// Samples of frames `range`.
  pub fn samples(&self, range: Range<usize>) -> Range<usize> {
    (range.start * FRAME).min(self.pcm.len())..(range.end * FRAME).min(self.pcm.len())
  }

  /// What the engine gets: `None` without speech, else the clip with silent edges trimmed to
  /// [`EDGE_MS`] and pauses longer than [`LONG_PAUSE_MS`] shortened to [`KEPT_PAUSE_MS`]. Only
  /// pause frames are removed, whole frames at a time, so the scores stay in step with the audio.
  /// Recordings keep the original audio; this is only the engine's copy.
  pub fn engine_input(&self) -> Option<Clip> {
    if !self.has_speech() {
      return None;
    }

    let n = self.scores.len();
    let (edge, long, kept) = (frames(EDGE_MS), frames(LONG_PAUSE_MS), frames(KEPT_PAUSE_MS));
    let mut cut = Vec::new();
    for pause in self.pauses() {
      let len = pause.len();
      if pause.start == 0 && len > edge {
        cut.push(0..pause.end - edge);
      } else if pause.end == n && len > edge {
        cut.push(pause.start + edge..n);
      } else if pause.start > 0 && pause.end < n && len > long {
        cut.push(pause.start + kept / 2..pause.end - (kept - kept / 2));
      }
    }

    let mut out = Clip::default();
    let mut from = 0;
    for range in cut.into_iter().chain(std::iter::once(n..n)) {
      out.pcm.extend_from_slice(&self.pcm[self.samples(from..range.start)]);
      out.scores.extend_from_slice(&self.scores[from..range.start]);
      from = range.end;
    }

    Some(out)
  }

  /// Maximal runs of pause frames, in frames.
  pub fn pauses(&self) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &s) in self.scores.iter().enumerate() {
      match (s < SILENT, start) {
        (true, None) => start = Some(i),
        (false, Some(a)) => {
          runs.push(a..i);
          start = None;
        }
        _ => {}
      }
    }

    if let Some(a) = start {
      runs.push(a..self.scores.len());
    }

    runs
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::fixture_pcm;

  #[test]
  fn frames_rounds_down() {
    assert_eq!(frames(16), 1);
    assert_eq!(frames(1000), 62);
    assert_eq!(frames(300), 18);
  }

  #[test]
  fn streaming_matches_one_shot_and_pads_the_last_frame() {
    let pcm: Vec<f32> = (0..FRAME * 5 + 100).map(|i| ((i as f32) * 0.05).sin() * 0.3).collect();
    let whole = Clip::analyze(pcm.clone()).scores;
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
    let clip = Clip::analyze(vec![0.0; SAMPLE_RATE as usize]);
    assert_eq!(clip.voiced_frames(), 0);
    assert_eq!(clip.seconds(), 1.0);
    let all = 0..clip.scores.len();
    assert_eq!(clip.pauses(), [all]);
  }

  /// A clip from `(score, ms)` runs: a 440 Hz tone where the score is voiced, silence elsewhere.
  fn synthetic(runs: &[(f32, usize)]) -> Clip {
    let mut clip = Clip::default();
    for &(score, ms) in runs {
      let n = frames(ms);
      let amplitude = if score >= VOICED { 0.3 } else { 0.0 };
      let start = clip.pcm.len();
      clip.pcm.extend((0..n * FRAME).map(|i| ((start + i) as f32 * 0.172).sin() * amplitude));
      clip.scores.extend(std::iter::repeat_n(score, n));
    }

    clip
  }

  fn ms(frames: usize) -> usize {
    frames * 16
  }

  #[test]
  fn speech_needs_enough_voiced_frames() {
    assert!(!synthetic(&[(0.1, 5_000)]).has_speech());
    assert!(!synthetic(&[(0.1, 1_000), (0.9, 128), (0.45, 2_000)]).has_speech());
    assert!(synthetic(&[(0.1, 1_000), (0.9, 160), (0.1, 1_000)]).has_speech());
    assert_eq!(synthetic(&[(0.1, 5_000)]).engine_input(), None);
  }

  #[test]
  fn engine_input_trims_edges_and_shortens_long_pauses() {
    let clip = synthetic(&[
      (0.1, 2_000),
      (0.9, 1_008),
      (0.1, 2_496), // long pause
      (0.9, 496),
      (0.1, 800), // short pause: kept
      (0.9, 496),
      (0.1, 1_504),
    ]);
    let input = clip.engine_input().unwrap();
    let edge = frames(EDGE_MS);
    let runs =
      [edge, frames(1_008), frames(KEPT_PAUSE_MS), frames(496), frames(800), frames(496), edge];
    assert_eq!(input.scores.len(), runs.iter().sum::<usize>());
    assert_eq!(input.pcm.len(), input.scores.len() * FRAME);
    assert_eq!(input.voiced_frames(), clip.voiced_frames(), "no speech is removed");

    // The speech comes through unchanged and in order.
    let voiced = |c: &Clip| -> Vec<f32> {
      (0..c.scores.len())
        .filter(|&i| c.scores[i] >= VOICED)
        .flat_map(|i| c.pcm[c.samples(i..i + 1)].to_vec())
        .collect()
    };
    assert_eq!(voiced(&input), voiced(&clip));
    let pauses: Vec<usize> = input.pauses().iter().map(|p| ms(p.len())).collect();
    assert_eq!(pauses, [ms(edge), ms(frames(KEPT_PAUSE_MS)), 800, ms(edge)]);
  }

  #[test]
  fn uncertain_frames_are_never_removed() {
    // Neither speech nor pause (between SILENT and VOICED), e.g. a soft onset or breathing.
    let clip = synthetic(&[(0.45, 2_000), (0.9, 480), (0.45, 3_000), (0.9, 480), (0.4, 1_200)]);
    assert_eq!(clip.engine_input().unwrap(), clip);
  }

  #[test]
  fn engine_input_keeps_a_partial_last_frame() {
    let mut clip = synthetic(&[(0.1, 160), (0.9, 480)]);
    clip.pcm.truncate(clip.pcm.len() - 100);
    assert_eq!(clip.engine_input().unwrap(), clip, "nothing to cut");

    let mut clip = synthetic(&[(0.9, 480), (0.1, 2_000)]);
    clip.pcm.truncate(clip.pcm.len() - 100);
    let input = clip.engine_input().unwrap();
    assert_eq!(input.scores.len(), frames(480) + frames(EDGE_MS));
    assert_eq!(input.pcm.len(), input.scores.len() * FRAME);
  }

  #[test]
  #[ignore = "needs the fixtures: make test-models"]
  fn fixtures_with_and_without_speech() {
    for name in ["silence", "noise"] {
      let clip = Clip::analyze(fixture_pcm(name));
      eprintln!("{name}: {} voiced frames", clip.voiced_frames());
      assert_eq!(clip.engine_input(), None, "{name}");
    }

    for name in ["ru-short", "ru-typical", "ru-long", "en-short", "en-paragraph", "en-short-44k"] {
      let clip = Clip::analyze(fixture_pcm(name));
      let input = clip.engine_input().unwrap_or_else(|| panic!("{name}: no speech"));
      eprintln!("{name}: {:.2} s → {:.2} s to the engine", clip.seconds(), input.seconds());
      assert_eq!(input.voiced_frames(), clip.voiced_frames(), "{name}: speech removed");
    }
  }

  #[test]
  fn tolerates_overshoot_and_nan() {
    let mut pcm = vec![1.5; FRAME];
    pcm[3] = f32::NAN;
    assert!(Clip::analyze(pcm).scores[0].is_finite());
  }
}
