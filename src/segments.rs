// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A transcript's text with times: segments, each a sentence short enough to be a subtitle, and
//! what is made of them: paragraphs to read, TXT and SRT.

use std::fmt::Write;
use std::ops::Range;

/// A stretch of a transcript: its words and where they are in the audio, in ms from its start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
  pub start_ms: i64,
  /// After `start_ms`: where the next segment starts, or where its piece of audio ends.
  pub end_ms: i64,
  /// Who speaks, from 0; `None` until a transcript tells speakers apart.
  pub speaker: Option<i64>,
  pub text: String,
}

/// A pause this long between segments starts a new paragraph.
const PARAGRAPH_PAUSE_MS: i64 = 2_000;
/// A paragraph holds at most this many segments.
const PARAGRAPH_MAX: usize = 6;

/// The segments cut into paragraphs, as ranges of them: a new one after a pause of
/// [PARAGRAPH_PAUSE_MS], after [PARAGRAPH_MAX] segments, or when the speaker changes.
pub fn paragraphs(segments: &[Segment]) -> Vec<Range<usize>> {
  let mut out: Vec<Range<usize>> = Vec::new();
  for (i, s) in segments.iter().enumerate() {
    match out.last_mut() {
      Some(p)
        if p.len() < PARAGRAPH_MAX
          && s.speaker == segments[i - 1].speaker
          && s.start_ms - segments[i - 1].end_ms < PARAGRAPH_PAUSE_MS =>
      {
        p.end = i + 1
      }
      _ => out.push(i..i + 1),
    }
  }

  out
}

/// A paragraph's text: its segments' joined by spaces.
pub fn paragraph_text(segments: &[Segment]) -> String {
  let words = segments.iter().map(|s| s.text.trim()).filter(|t| !t.is_empty());
  words.collect::<Vec<_>>().join(" ")
}

/// The text to read or copy: its [paragraphs] with a blank line between them, no times.
pub fn txt(segments: &[Segment]) -> String {
  let paras = paragraphs(segments).into_iter().map(|p| paragraph_text(&segments[p]));
  let mut out = paras.filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n");
  if !out.is_empty() {
    out.push('\n');
  }

  out
}

/// SubRip subtitles: a cue per segment with text, numbered from 1.
pub fn srt(segments: &[Segment]) -> String {
  let mut out = String::new();
  let cues = segments.iter().filter(|s| !s.text.trim().is_empty());
  for (n, s) in (1..).zip(cues) {
    let end = s.end_ms.max(s.start_ms + 1);
    let (from, to) = (srt_time(s.start_ms), srt_time(end));
    let _ = write!(out, "{n}\n{from} --> {to}\n{}\n\n", s.text.trim());
  }

  out
}

/// `01:02:03,456`.
fn srt_time(ms: i64) -> String {
  let ms = ms.max(0);
  let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
  format!("{h:02}:{m:02}:{s:02},{:03}", ms % 1000)
}

/// A time in the audio as a clock: `2:05` under an hour, `1:02:05` from it.
pub fn clock(ms: i64) -> String {
  let s = ms.max(0) / 1000;
  match s / 3600 {
    0 => format!("{}:{:02}", s / 60, s % 60),
    h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn seg(start_ms: i64, end_ms: i64, text: &str) -> Segment {
    Segment { start_ms, end_ms, speaker: None, text: text.into() }
  }

  #[test]
  fn paragraphs_at_pauses_speakers_and_length() {
    let mut s: Vec<Segment> = (0..8).map(|i| seg(i * 1000, i * 1000 + 900, "a.")).collect();
    s.push(seg(10_000, 11_000, "b."));
    s.push(Segment { speaker: Some(1), ..seg(11_000, 12_000, "c.") });
    assert_eq!(paragraphs(&s), [0..6, 6..8, 8..9, 9..10]);
    assert!(paragraphs(&[]).is_empty());
  }

  #[test]
  fn txt_has_paragraphs_without_times() {
    let s = [
      seg(0, 1000, " One."),
      seg(1000, 2000, "Two. "),
      seg(2000, 2500, ""),
      seg(9000, 9500, "Three."),
    ];
    assert_eq!(txt(&s), "One. Two.\n\nThree.\n");
    assert_eq!(txt(&[]), "");
  }

  #[test]
  fn srt_cues_numbered_with_times() {
    let s = [seg(0, 1500, "One."), seg(1500, 1500, " "), seg(36_061_005, 36_062_000, "Late.")];
    let want =
      "1\n00:00:00,000 --> 00:00:01,500\nOne.\n\n2\n10:01:01,005 --> 10:01:02,000\nLate.\n\n";
    assert_eq!(srt(&s), want);
    assert_eq!(srt(&[seg(5, 5, "x")]), "1\n00:00:00,005 --> 00:00:00,006\nx\n\n", "never empty");
  }

  #[test]
  fn clocks() {
    assert_eq!(
      [clock(0), clock(65_999), clock(3_600_000), clock(16_205_000)],
      ["0:00", "1:05", "1:00:00", "4:30:05"]
    );
  }
}
