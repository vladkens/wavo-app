// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A transcript's text with times: segments, each a sentence short enough to be a subtitle, made
//! of a model's tokens, and what is made of them: paragraphs to read, TXT and SRT.

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

/// A segment ends after one of these, as a sentence does.
const SENTENCE_ENDS: [char; 7] = ['.', '?', '!', '…', '。', '？', '！'];
/// A segment ends before it would grow past this many characters: two subtitle lines of 42.
const SEGMENT_CHARS: usize = 84;
/// A segment ends before a word this long after its start.
const SEGMENT_MS: i64 = 10_000;
/// A segment lasts at most this long after its last time for each word at that time: a word-timed
/// model's last word, or a Whisper segment's words, which all start at the segment's time.
const LINGER_MS: i64 = 1_500;

/// The segments of a piece of audio a model transcribed whole, `start_ms..end_ms`, from its tokens:
/// their text (`▁` or a space starts a word) and start times.
///
/// A segment ends after a sentence, or before it would pass [SEGMENT_CHARS] or [SEGMENT_MS]. Words
/// that start at the same time stay in one segment, as times can't part them: a frame of GigaAM's
/// or Parakeet's, a segment of Whisper's, which times its tokens by their segment, and the whole
/// piece of Moonshine's, which has no times. A segment ends where the next one starts, the last
/// one where the piece ends, but [LINGER_MS] after its last time for each word at it, so that a
/// pause after it is a pause.
pub fn of_tokens<'a>(
  start_ms: i64,
  end_ms: i64,
  tokens: impl IntoIterator<Item = (&'a str, i64)>,
) -> Vec<Segment> {
  // Each word's start and text, the space before it included.
  let mut words: Vec<(i64, String)> = Vec::new();
  for (piece, at) in tokens {
    let piece = piece.replace('▁', " ");
    match words.last_mut() {
      Some(word) if !piece.starts_with(' ') => word.1 += &piece,
      _ => words.push((at.clamp(start_ms, end_ms), piece)),
    }
  }

  // Each segment, its last time and how many of its words start at it.
  let mut out: Vec<(Segment, i64, i64)> = Vec::new();
  for (at, text) in words {
    let text = text.trim();
    match out.last_mut() {
      _ if text.is_empty() => {}
      Some((s, last, n)) if at == *last => {
        (s.text, *n) = (format!("{} {text}", s.text), *n + 1);
      }
      Some((s, last, n))
        if !s.text.ends_with(SENTENCE_ENDS)
          && s.text.chars().count() + 1 + text.chars().count() <= SEGMENT_CHARS
          && at - s.start_ms < SEGMENT_MS =>
      {
        (s.text, *last, *n) = (format!("{} {text}", s.text), at, 1);
      }
      _ => {
        out.push((Segment { start_ms: at, end_ms: at, speaker: None, text: text.into() }, at, 1))
      }
    }
  }

  let nexts: Vec<i64> = out.iter().skip(1).map(|o| o.0.start_ms).chain([end_ms]).collect();
  let ends = out.into_iter().zip(nexts);
  let end = |s: &Segment, last, n, next: i64| next.min(last + n * LINGER_MS).max(s.start_ms);
  ends.map(|((s, last, n), next)| Segment { end_ms: end(&s, last, n, next), ..s }).collect()
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

  /// Tokens from `(piece, start_ms)`.
  fn cut(start_ms: i64, end_ms: i64, tokens: &[(&str, i64)]) -> Vec<Segment> {
    of_tokens(start_ms, end_ms, tokens.iter().copied())
  }

  /// The words of the segments' texts joined: those of the piece's text.
  fn words(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
  }

  #[test]
  fn segments_end_at_sentences_and_pauses() {
    let tokens = [
      ("▁При", 1000),
      ("вет", 1040),
      (".", 1300),
      ("▁Как", 2000),
      ("▁дела", 2400),
      ("?", 2600),
      ("▁Хорошо", 9000),
    ];
    let s = cut(1000, 12_000, &tokens);
    assert_eq!(
      s,
      [seg(1000, 2000, "Привет."), seg(2000, 3900, "Как дела?"), seg(9000, 10_500, "Хорошо")]
    );
    assert_eq!(words(&s), "Привет. Как дела? Хорошо");
    assert_eq!(cut(0, 1000, &[("。", 0)])[0].text, "。");
    assert!(cut(0, 1000, &[]).is_empty(), "an empty piece");
    assert!(cut(0, 1000, &[("▁", 0), (" ", 500)]).is_empty(), "only spaces");
  }

  #[test]
  fn a_long_sentence_is_cut_at_a_word() {
    let long: Vec<(String, i64)> = (0..30).map(|i| (format!("▁word{i:02}"), i * 300)).collect();
    let s = of_tokens(0, 9000, long.iter().map(|(p, at)| (p.as_str(), *at)));
    assert!(s.iter().all(|s| s.text.chars().count() <= SEGMENT_CHARS), "{s:?}");
    assert_eq!(s.len(), 3);
    assert_eq!((s[0].end_ms, s[1].start_ms), (s[1].start_ms, 12 * 300));
    assert_eq!(words(&s).split(' ').count(), 30);

    let slow: Vec<(String, i64)> = (0..6).map(|i| (format!("▁w{i}"), i * 4000)).collect();
    let s = of_tokens(0, 24_000, slow.iter().map(|(p, at)| (p.as_str(), *at)));
    assert_eq!(s.iter().map(|s| s.start_ms).collect::<Vec<_>>(), [0, 12_000], "at 10 s");
  }

  #[test]
  fn shared_times_keep_their_words_together() {
    // Whisper: each token carries its segment's start.
    let whisper =
      [(" One", 0), (" two.", 0), (" Three", 0), (" four", 0), (" five.", 0), (" Six.", 4000)];
    let s = cut(0, 30_000, &whisper);
    assert_eq!(s, [seg(0, 4000, "One two. Three four five."), seg(4000, 5500, "Six.")]);

    // Moonshine: no times, each token at its piece's start.
    let moonshine = [("▁A", 25_000), ("▁long", 25_000), ("▁piece.", 25_000), ("▁Of", 25_000)];
    assert_eq!(cut(25_000, 30_000, &moonshine), [seg(25_000, 30_000, "A long piece. Of")]);
  }

  #[test]
  fn character_vocabularies_join_with_their_spaces() {
    // GigaAM ctc: a token per character, and a piece after the first starts with a space.
    let chars = [(" ", 0), ("д", 40), ("а", 80), (" ", 120), ("н", 160), ("е", 200), ("т", 240)];
    assert_eq!(cut(0, 5000, &chars), [seg(0, 1620, "да нет")]);
    let later = [("т", 6000), ("а", 6040), ("к", 6080)];
    assert_eq!(cut(5000, 7000, &later), [seg(6000, 7000, "так")], "a piece starts a word");
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
