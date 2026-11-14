// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The live preview: the text of a dictation while it records, for the HUD. Every [STEP] the
//! engine transcribes the recording since the last piece it fixed (30–110 ms for up to 15 s of
//! audio on an M5, ~0.5 s with Whisper large); at a pause ([Clip::piece_end]) the piece before
//! it is transcribed once more and its text fixed, so the audio decoded again stays short. A word
//! shown stays put once two jobs in a row agree on it (as LocalAgreement-2 of whisper_streaming
//! does), so only the last words, the ones still being said, may change. The text pasted is still
//! the whole recording's, transcribed after the stop; a preview never reaches the app the person
//! dictates in.

use std::time::Duration;

use crate::audio::Clip;
use crate::engine::EngineError;

/// How often the preview asks the engine; one job at a time, so a slower model updates it less
/// often.
pub const STEP: Duration = Duration::from_millis(500);

/// What a dictation said so far: the words that stay, then those the next step may change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
  pub fixed: String,
  pub tail: String,
}

/// A dictation's preview as it records.
#[derive(Debug, Default)]
pub struct Preview {
  pub text: Text,
  /// The text of the fixed pieces, and of the audio after them as the last job heard it.
  pieces: String,
  heard: String,
  /// The VAD frame the audio after the fixed pieces starts at.
  from: usize,
  /// The job running: its id, and the frame its piece ends at when it fixes one.
  job: Option<(u64, Option<usize>)>,
}

impl Preview {
  /// The frame [next](Preview::next) wants the recording from.
  pub fn from(&self) -> usize {
    self.from
  }

  /// The job running, to cancel.
  pub fn job(&self) -> Option<u64> {
    self.job.map(|(id, _)| id)
  }

  /// The audio for job `id` from `after`, the recording since [from](Preview::from): the piece to
  /// fix when it has one, else all of it. `None` while a job runs, or without speech in it (a
  /// silent piece is fixed at once, without text).
  pub fn next(&mut self, id: u64, after: Clip) -> Option<Vec<f32>> {
    if self.job.is_some() {
      return None;
    }

    let (clip, end) = match after.piece_end() {
      Some(end) => (after.split_at(end).0, Some(self.from + end)),
      None => (after, None),
    };

    let Some(input) = clip.engine_input() else {
      match end {
        Some(end) => self.from = end,
        None => {
          self.heard.clear();
          self.show(false);
        }
      }

      return None;
    };

    self.job = Some((id, end));
    Some(input.pcm)
  }

  /// Job `id`'s text: `false` for another job, and for one cancelled, which the next step asks
  /// again. A failed one counts as no text. A fixed piece keeps the words heard after its own
  /// until the next job hears them again.
  pub fn done(&mut self, id: u64, result: Result<String, EngineError>) -> bool {
    let Some((_, end)) = self.job.filter(|(job, _)| *job == id) else { return false };
    self.job = None;
    let text = match result {
      Err(EngineError::Cancelled) => return false,
      Ok(text) => text.trim().to_owned(),
      Err(_) => String::new(),
    };

    let Some(end) = end else {
      self.heard = text;
      self.show(true);
      return true;
    };

    self.from = end;
    let words = text.split_whitespace().count();
    self.heard = self.heard.split_whitespace().skip(words).collect::<Vec<_>>().join(" ");
    self.pieces = [self.pieces.as_str(), &text].join(" ").trim().to_owned();
    self.show(false);
    true
  }

  /// The text heard into the text shown: the words that stay are kept as they are, and what was
  /// heard after them follows; of that, the words of fixed pieces stay too, and, after a job that
  /// heard the audio anew (`agreed`), those it says as the one before did, but for the last one,
  /// which may still be half said or end with the model's period. A word that stays as agreed
  /// keeps the case and punctuation it showed with, unless it was the last.
  fn show(&mut self, agreed: bool) {
    let pieces = self.pieces.split_whitespace().count();
    let heard: Vec<&str> =
      self.pieces.split_whitespace().chain(self.heard.split_whitespace()).collect();
    let mut words: Vec<&str> = self.text.fixed.split_whitespace().collect();
    let at = aligned(&words, &heard);
    let after = &heard[at..];
    let before: Vec<&str> = self.text.tail.split_whitespace().collect();
    let fixed = pieces.saturating_sub(at);
    let same = after.iter().zip(&before).take_while(|(a, b)| key(a) == key(b)).count();
    let stay = if agreed { fixed.max(same.min(after.len().saturating_sub(1))) } else { fixed };
    for (i, &word) in after[..stay].iter().enumerate() {
      words.push(if i >= fixed && i + 1 < before.len() { before[i] } else { word });
    }

    self.text.fixed = words.join(" ");
    self.text.tail = after[stay..].join(" ");
  }

  /// A piece was fixed: the next step should not wait.
  pub fn fixed_since(&self, before: usize) -> bool {
    self.from != before
  }
}

/// A word as compared: without case and punctuation.
fn key(word: &str) -> String {
  word.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Where the words `kept` end in `heard`: the end of the beginning of `heard` with the fewest edits
/// from them, of those one at their last word, the nearest to their count; a job may join, split,
/// drop or add a word there.
fn aligned(kept: &[&str], heard: &[&str]) -> usize {
  let keys = |words: &[&str]| words.iter().map(|w| key(w)).collect::<Vec<_>>();
  let (kept, heard) = (keys(kept), keys(heard));
  let mut row: Vec<usize> = (0..=heard.len()).collect();
  for (i, k) in kept.iter().enumerate() {
    let mut next = vec![i + 1; heard.len() + 1];
    for (j, h) in heard.iter().enumerate() {
      next[j + 1] = (row[j] + usize::from(k != h)).min(row[j + 1] + 1).min(next[j] + 1);
    }

    row = next;
  }

  let last = |j: usize| j > 0 && kept.last() == heard.get(j - 1);
  (0..=heard.len()).min_by_key(|&j| (row[j], !last(j), j.abs_diff(kept.len()))).unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Instant;

  use crossbeam_channel::unbounded;

  use super::*;
  use crate::engine::{Engine, Event, Wavo};
  use crate::testing::{fixture, model_ru, wer};

  const SPEECH: f32 = 0.9;
  const PAUSE: f32 = 0.1;

  /// A clip of `parts`, each (score, ms), in 16 ms frames, its samples silent.
  fn clip(parts: &[(f32, usize)]) -> Clip {
    let scores: Vec<f32> = parts.iter().flat_map(|&(s, ms)| vec![s; ms / 16]).collect();
    Clip { pcm: vec![0.0; scores.len() * 256], scores }
  }

  fn frame(ms: usize) -> usize {
    ms / 16
  }

  #[test]
  fn a_piece_ends_in_a_pause_after_two_seconds_or_in_the_longest_gap_past_ten() {
    assert_eq!(clip(&[(SPEECH, 5000)]).piece_end(), None, "no pause");
    assert_eq!(
      clip(&[(SPEECH, 1000), (PAUSE, 480), (SPEECH, 2000)]).piece_end(),
      None,
      "too early"
    );
    assert_eq!(
      clip(&[(SPEECH, 3000), (PAUSE, 160), (SPEECH, 2000)]).piece_end(),
      None,
      "too short"
    );
    assert_eq!(clip(&[(SPEECH, 3000), (PAUSE, 480)]).piece_end(), None, "no speech after it yet");

    let two = clip(&[(SPEECH, 2400), (PAUSE, 320), (SPEECH, 2400), (PAUSE, 480), (SPEECH, 400)]);
    assert_eq!(two.piece_end(), Some(frame(2400 + 320 + 2400) + frame(480) / 2), "the last");

    let gaps = [(SPEECH, 4000), (0.4, 64), (SPEECH, 3000), (0.45, 160), (SPEECH, 3200)];
    assert_eq!(clip(&gaps).piece_end(), Some(frame(4000 + 64 + 3000) + frame(160) / 2));
    assert_eq!(clip(&[(SPEECH, 10_400)]).piece_end(), Some(frame(10_000)), "no gap at all");
  }

  #[test]
  fn the_tail_changes_and_a_fixed_piece_stays() {
    let mut p = Preview::default();
    let speech = clip(&[(PAUSE, 160), (SPEECH, 1600)]);
    assert!(p.next(1, speech.clone()).is_some());
    assert_eq!(p.next(2, speech.clone()), None, "one job at a time");
    assert!(!p.done(2, Ok("hm".into())), "not the job running");
    assert!(p.done(1, Ok(" Hello wor ".into())));
    assert_eq!(p.text, Text { fixed: "".into(), tail: "Hello wor".into() });

    // A cancelled job is asked again: nothing moved.
    assert!(p.next(3, speech).is_some());
    assert!(!p.done(3, Err(EngineError::Cancelled)));
    assert_eq!((p.job(), p.from()), (None, 0));

    let paused = clip(&[(PAUSE, 160), (SPEECH, 2400), (PAUSE, 480), (SPEECH, 800)]);
    let end = frame(160 + 2400) + frame(480) / 2;
    let piece = p.next(4, paused).unwrap();
    assert!(piece.len() <= end * 256, "only the piece");
    assert_eq!(p.next(5, clip(&[(SPEECH, 1600)])), None, "while it runs");
    assert!(p.done(4, Ok("Hello world.".into())));
    assert!(p.fixed_since(0));
    assert_eq!(p.from(), end);
    assert_eq!(p.text, Text { fixed: "Hello world.".into(), tail: "".into() });
  }

  #[test]
  fn a_fixed_piece_keeps_the_tails_words_after_it() {
    let mut p = Preview {
      text: Text { fixed: "One.".into(), tail: "Two three four".into() },
      pieces: "One.".into(),
      heard: "Two three four".into(),
      ..Preview::default()
    };
    let paused = clip(&[(SPEECH, 2400), (PAUSE, 480), (SPEECH, 800)]);
    assert!(p.next(1, paused).is_some());
    assert!(p.done(1, Ok("Two three.".into())));
    assert_eq!(p.text, Text { fixed: "One. Two three.".into(), tail: "four".into() });
  }

  /// Job `id` heard `text` in a recording with no pause.
  fn heard(p: &mut Preview, id: u64, text: &str) -> Text {
    assert!(p.next(id, clip(&[(SPEECH, 1600)])).is_some());
    assert!(p.done(id, Ok(text.into())));
    p.text.clone()
  }

  fn text(fixed: &str, tail: &str) -> Text {
    Text { fixed: fixed.into(), tail: tail.into() }
  }

  /// What two jobs in a row say alike stays as it showed, but for the last word; what a later job
  /// says otherwise changes only the words after it.
  #[test]
  fn the_words_two_jobs_agree_on_stay() {
    let mut p = Preview::default();
    assert_eq!(heard(&mut p, 1, "Hello there, how."), text("", "Hello there, how."));
    let shown = "as shown, the last one without its period as heard";
    assert_eq!(
      heard(&mut p, 2, "Hello. There, how are"),
      text("Hello there, how", "are"),
      "{shown}"
    );
    assert_eq!(
      heard(&mut p, 3, "Hello. There. How are you doing"),
      text("Hello there, how are", "you doing")
    );
    assert_eq!(
      heard(&mut p, 4, "Hello. How are you doing."),
      text("Hello there, how are you", "doing.")
    );
    assert_eq!(
      heard(&mut p, 5, "Hello there how are you doing today"),
      text("Hello there, how are you doing", "today")
    );
    assert_eq!(heard(&mut p, 6, ""), text("Hello there, how are you doing", ""), "nothing heard");
  }

  /// The words shown to stay are found in what a job heard again even when it joined, split,
  /// dropped or added a word in them.
  #[test]
  fn the_words_that_stay_are_found_in_what_is_heard_again() {
    let cases: [(&str, &str, usize); 6] = [
      ("", "a b", 0),
      ("a b c", "a b c d e", 3),
      ("a b c", "A, b. c d", 3),
      ("one to three", "one two three four", 3),
      ("some thing new", "something new here", 2),
      ("a b c", "a b x c d", 4),
    ];
    for (kept, heard, end) in cases {
      let (k, h): (Vec<_>, Vec<_>) =
        (kept.split_whitespace().collect(), heard.split_whitespace().collect());
      assert_eq!(aligned(&k, &h), end, "{kept:?} in {heard:?}");
    }
  }

  /// The recording `pcm` played to the preview a [STEP] at a time, each job's text waited for;
  /// the text after each step, and the time the jobs took.
  fn played(model: PathBuf, pcm: Vec<f32>, language: &str) -> (Vec<Text>, Duration) {
    let (tx, events) = unbounded();
    let engine = Engine::start(Arc::new(Wavo), model, false, None, tx);
    let mut busy = Duration::ZERO;
    let all = Clip::analyze(pcm);
    let (mut p, mut id, mut steps) = (Preview::default(), 0, Vec::new());
    let step = frame(STEP.as_millis() as usize);
    for end in (step..=all.scores.len()).step_by(step) {
      // A fixed piece asks again at once.
      loop {
        let from = p.from().min(end);
        let after = Clip {
          pcm: all.pcm[from * 256..end * 256].to_vec(),
          scores: all.scores[from..end].to_vec(),
        };
        id += 1;
        if let Some(pcm) = p.next(id, after) {
          let started = Instant::now();
          engine.preview(id, pcm, Some(language.into()));
          let result = loop {
            if let Event::Done(job, result, _) = events.recv().unwrap()
              && job == id
            {
              break result;
            }
          };
          // The first loads the model.
          busy += if id > 1 { started.elapsed() } else { Duration::ZERO };
          p.done(id, result);
        }

        if !p.fixed_since(from) {
          break;
        }
      }

      steps.push(p.text.clone());
    }

    (steps, busy)
  }

  /// A paragraph read: the words that stay never change, and only the last ones may, two at most
  /// on average; what it shows in the end is what was said.
  #[test]
  #[ignore = "needs models: make test-models"]
  fn on_a_real_model_only_the_last_words_change() {
    let f = fixture("ru-long");
    let seconds = f.pcm.len() as f32 / 16_000.0;
    let (steps, busy) = played(model_ru(), f.pcm, "ru");
    for (before, after) in steps.iter().zip(&steps[1..]) {
      assert!(after.fixed.starts_with(&before.fixed), "{before:?} then {after:?}");
    }

    let tail: usize = steps.iter().map(|t| t.tail.split_whitespace().count()).sum();
    let last = steps.last().unwrap();
    let average = tail as f32 / steps.len() as f32;
    eprintln!("the engine busy {:.1}% of {seconds:.0} s", busy.as_secs_f32() / seconds * 100.0);
    eprintln!(
      "{} words, {average:.1} changing on average\n  {} [{}]",
      last.fixed.split_whitespace().count(),
      last.fixed,
      last.tail
    );
    assert!(average <= 2.5, "{average:.1}");
    assert!(wer(&f.text, &format!("{} {}", last.fixed, last.tail)) < 0.15);
  }

  #[test]
  fn silence_is_fixed_without_a_job_and_clears_the_tail() {
    let text = Text { fixed: "".into(), tail: "noise".into() };
    let mut p = Preview { text, heard: "noise".into(), ..Preview::default() };
    assert_eq!(p.next(1, clip(&[(PAUSE, 1600)])), None);
    assert_eq!((p.job(), p.text.tail.as_str()), (None, ""));

    // The pause past the first two seconds ends the piece; no speech in it.
    assert_eq!(p.next(2, clip(&[(PAUSE, 2400), (SPEECH, 800)])), None);
    let (a, b) = (frame(2000), frame(2400));
    assert_eq!(p.from(), a + (b - a) / 2, "fixed as it is, without text");
  }
}
