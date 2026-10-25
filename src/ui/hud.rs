// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD (the recording panel): its geometry, what it shows after the hotkey is let go, and the
//! timing of its transitions; the [overlay](super::overlay) draws it.
//!
//! One pill, 176 × 36, for every state. Recording: a red dot, the level as 16 bars, the time.
//! Transcribing: the bars become a travelling wave, the time freezes (or `n/m` for a recording in
//! pieces); no words. Then, when the text has gone where it goes, the pill collapses to a circle
//! and fades: no "done" state, also when nothing was said. Only an error shows: a glyph and a short
//! lowercase reason for a few seconds, the pill widening to fit it, since a failure must never be
//! silent. While recording, a warning takes the level's place when the microphone sends no sound
//! or no model can transcribe ([during]: `no model · open models`); the recording goes on.
//!
//! Times are the overlay's clock: a [Duration] since it started.

use std::time::Duration;

use crate::session::{NoModel, Outcome as Ended, Status};

// Geometry, in points.
pub const WIDTH: f32 = 176.0;
pub const HEIGHT: f32 = 36.0;
pub const RADIUS: f32 = HEIGHT / 2.0;
pub const PAD: f32 = 14.0;
pub const DOT: f32 = 10.0;
const BARS: usize = 16;
pub const BAR_WIDTH: f32 = 3.0;
const BAR_MIN: f32 = 3.0;
const BAR_GAP: f32 = 2.0;
const BAR_MAX: f32 = 20.0;
const BARS_X: f32 = PAD + DOT + 10.0;
pub const TIME_X: f32 = BARS_X + BARS as f32 * (BAR_WIDTH + BAR_GAP) + 8.0;
/// A warning's glyph's box, and its reason after it.
pub const GLYPH: f32 = 20.0;
pub const REASON_X: f32 = PAD + GLYPH + 8.0;
/// The widest the pill grows for a reason.
pub const MAX_WIDTH: f32 = 320.0;
/// Around the pill in its window: room for its shadow.
pub const MARGIN: f32 = 12.0;

// Timing.
const APPEAR: Duration = Duration::from_millis(100);
/// Recording to transcribing: the bars settle, the dot fades.
const SETTLE: Duration = Duration::from_millis(180);
const COLLAPSE: Duration = Duration::from_millis(260);
const RESIZE: Duration = Duration::from_millis(200);
const ERROR_HOLD: Duration = Duration::from_millis(4000);
/// The meter takes a level this often: the bars move one place.
const METER_TICK: Duration = Duration::from_millis(33);

/// The secondary and tertiary text as the text colour's alpha.
const SECONDARY: f32 = 0.6;
const TERTIARY: f32 = 0.4;

// The wave: each bar goes from its low to its high and back, later than the one before.
const WAVE_LOW: f32 = 3.0;
const WAVE_HIGH: f32 = 9.0;
const WAVE_HALF: Duration = Duration::from_millis(600);
const WAVE_STAGGER: Duration = Duration::from_millis(75);

/// What the HUD does once the session is idle again.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
  /// The text went where it goes, or there was none (nothing said, cancelled): it collapses and
  /// goes.
  Collapse,
  /// A short lowercase reason for [`ERROR_HOLD`], then it collapses.
  Error(String),
}

/// What came of a dictation that ended since the status `before`, or a new error of a recording
/// that did not start; `None`: no news (it collapses if it shows something). With `auto_paste`, a
/// refused paste is an error.
pub fn after(status: &Status, before: Option<&Status>, auto_paste: bool) -> Option<Outcome> {
  let take = |s: &Status| s.last.as_ref().map(|l| l.take);
  let Some(last) = status.last.as_ref().filter(|l| Some(l.take) != before.and_then(take)) else {
    // A recording that did not start leaves no dictation, only the error.
    let shown = before.and_then(|s| s.error.as_deref());
    let error = status.error.as_deref().filter(|e| shown != Some(*e))?;
    return Some(Outcome::Error(short(error)));
  };

  Some(match last.outcome {
    Ended::Done if last.pasted => Outcome::Collapse,
    Ended::Done => match last.not_pasted.as_deref() {
      Some(why) if auto_paste => Outcome::Error(format!("not pasted: {}", short(why))),
      // Without auto-paste the text is on the clipboard: only a copy that failed is news.
      Some(why) if why.contains("copying failed") => Outcome::Error("copy failed".into()),
      _ => Outcome::Collapse,
    },
    Ended::NoSpeech | Ended::Cancelled => Outcome::Collapse,
    Ended::NoText => Outcome::Error("nothing recognized".into()),
    Ended::NoModel => {
      let load = last.error.as_deref().is_some_and(|e| e.contains("could not be loaded"));
      Outcome::Error(no_model(load))
    }
    Ended::Failed => Outcome::Error(short(last.error.as_deref().unwrap_or("failed"))),
  })
}

/// What the pill says while recording when no model can transcribe ([Status::no_model]): the
/// recording goes on, kept for later.
pub fn during(status: &Status) -> Option<String> {
  let why = status.no_model.as_ref()?;
  Some(no_model(matches!(why, NoModel::LoadFailed { .. })))
}

/// No model could transcribe: `no model · open models`, or `model failed to load · open models`
/// for one that is there (`load`).
fn no_model(load: bool) -> String {
  let what = if load { "model failed to load" } else { "no model" };
  format!("{what} · open models")
}

/// An error as the HUD says it: the known causes in a few words, else the first clause,
/// lowercase.
fn short(error: &str) -> String {
  let lower = error.to_lowercase();
  let known = [
    (&["no speech model", "no model"][..], "no model"),
    (&["microphone access", "not authorized", "permission"][..], "no microphone access"),
    (&["password", "secure"][..], "password field"),
    (&["focus moved"][..], "focus moved"),
    (&["crash", "died", "panicked"][..], "engine crashed"),
    (&["timed out", "took longer"][..], "engine timed out"),
    (&["engine is stuck"][..], "engine stuck"),
  ];
  if let Some((_, short)) = known.iter().find(|(keys, _)| keys.iter().any(|k| lower.contains(k))) {
    return (*short).to_owned();
  }

  let clause = lower.split([':', ';', '(', '\n']).next().unwrap_or("").trim();
  let clause = clause.trim_end_matches(['.', ',']);
  match clause.chars().count() {
    0 => "failed".into(),
    n if n > 40 => clause.chars().take(39).collect::<String>() + "…",
    _ => clause.to_owned(),
  }
}

/// `n/m` for the piece being transcribed of a recording the engine takes in pieces.
fn pieces(progress: Option<(usize, usize)>) -> Option<String> {
  match progress {
    Some((done, total)) if total > 1 => Some(format!("{}/{total}", (done + 1).min(total))),
    _ => None,
  }
}

/// A level (linear RMS) as 0 to 1 on a dB scale: −60 dBFS and below is 0, −12 dBFS and above 1.
fn loudness(level: f32) -> f32 {
  let db = 20.0 * level.max(1e-6).log10();
  ((db + 60.0) / 48.0).clamp(0.0, 1.0)
}

/// A bar's height for a level.
fn bar_height(level: f32) -> f32 {
  BAR_MIN + (BAR_MAX - BAR_MIN) * loudness(level)
}

/// Bar `i`'s x in the pill.
fn bar_x(i: usize) -> f32 {
  BARS_X + i as f32 * (BAR_WIDTH + BAR_GAP)
}

/// The pill's width for a reason `text` wide.
fn reason_width(text: f32) -> f32 {
  (REASON_X + text + PAD).clamp(WIDTH, MAX_WIDTH)
}

/// Bar `i`'s height `elapsed` into the wave: low to high and back, ease-in-out, each bar
/// [`WAVE_STAGGER`] after the one before.
fn wave(i: usize, elapsed: Duration) -> f32 {
  let Some(t) = elapsed.checked_sub(WAVE_STAGGER * i as u32) else { return WAVE_LOW };
  let half = WAVE_HALF.as_secs_f32();
  let phase = t.as_secs_f32() % (2.0 * half) / half;
  let up = if phase <= 1.0 { phase } else { 2.0 - phase };
  WAVE_LOW + (WAVE_HIGH - WAVE_LOW) * ease_in_out(up)
}

/// How far `t` is into `span`, through `ease`.
fn eased(t: Duration, span: Duration, ease: fn(f32) -> f32) -> f32 {
  ease((t.as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0))
}

fn ease_in(t: f32) -> f32 {
  bezier(0.4, 0.0, 1.0, 1.0, t)
}

fn ease_out(t: f32) -> f32 {
  bezier(0.0, 0.0, 0.2, 1.0, t)
}

fn ease_in_out(t: f32) -> f32 {
  bezier(0.42, 0.0, 0.58, 1.0, t)
}

/// The CSS timing curve with control points (x1, y1) and (x2, y2) at time `t`: x(s) = t solved for
/// s by bisection, then y(s).
fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
  if t <= 0.0 || t >= 1.0 {
    return t.clamp(0.0, 1.0);
  }

  let curve = |a: f32, b: f32, s: f32| {
    let u = 1.0 - s;
    3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
  };
  let (mut lo, mut hi) = (0.0, 1.0);
  for _ in 0..30 {
    let mid = (lo + hi) / 2.0;
    if curve(x1, x2, mid) < t {
      lo = mid;
    } else {
      hi = mid;
    }
  }

  curve(y1, y2, (lo + hi) / 2.0)
}

/// What the HUD is showing, as `wavo-app ctl status` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
  Hidden,
  Recording,
  Transcribing,
  /// An error's reason, for a while.
  Error,
  /// Collapsing to a circle and fading.
  Closing,
}

/// What to draw at `alpha`, placed from the pill's left edge (its contents' when it is narrower
/// than [WIDTH]).
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
  /// The red dot, [DOT] wide at [PAD].
  Dot { alpha: f32 },
  /// A bar of the level or the wave at `x`, [BAR_WIDTH] wide.
  Bar { x: f32, height: f32, alpha: f32 },
  /// The time, or `n/m`, at [TIME_X].
  Time { text: String, alpha: f32 },
  /// A warning's glyph, [GLYPH] wide at [PAD], and its reason after it at [REASON_X].
  Warning { text: String, alpha: f32 },
}

/// A frame of the HUD: the pill's width and opacity, and what is in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
  pub width: f32,
  pub alpha: f32,
  pub shapes: Vec<Shape>,
}

/// The HUD's state and timeline: the mode is always the target; animations are only how a frame
/// looks on the way to it.
#[derive(Debug, Clone)]
pub struct State {
  mode: Mode,
  /// What a collapse fades out.
  from: Mode,
  /// When the shown mode began, and when the pill began to appear.
  since: Duration,
  appearing: Option<Duration>,
  /// When the recording started, its last levels (oldest first) and when the last came, its
  /// length once it ended.
  started: Duration,
  levels: [f32; BARS],
  ticked: Duration,
  frozen: u64,
  /// The bars' heights when transcribing began: where they settle from.
  settled_from: [f32; BARS],
  progress: Option<(usize, usize)>,
  /// An error's reason, or the warning a recording shows in place of the level.
  text: Option<String>,
  /// The pill's width: from, to, and since when it goes between them.
  width: (f32, f32, Duration),
}

impl Default for State {
  fn default() -> Self {
    Self {
      mode: Mode::Hidden,
      from: Mode::Hidden,
      since: Duration::ZERO,
      appearing: None,
      started: Duration::ZERO,
      levels: [0.0; BARS],
      ticked: Duration::ZERO,
      frozen: 0,
      settled_from: [BAR_MIN; BARS],
      progress: None,
      text: None,
      width: (WIDTH, WIDTH, Duration::ZERO),
    }
  }
}

impl State {
  pub fn mode(&self) -> Mode {
    self.mode
  }

  /// The text it shows, for the tests: `warning: <reason>`, `n/m`.
  #[cfg(test)]
  pub fn text(&self) -> Option<String> {
    match self.mode {
      Mode::Error | Mode::Recording => self.text.as_ref().map(|t| format!("warning: {t}")),
      Mode::Transcribing => pieces(self.progress),
      Mode::Hidden | Mode::Closing => None,
    }
  }

  fn enter(&mut self, mode: Mode, now: Duration) {
    self.appearing = match self.mode {
      Mode::Hidden => Some(now),
      // Back at once: no fade-in for a pill still on screen.
      Mode::Closing => None,
      _ => self.appearing,
    };

    self.mode = mode;
    self.since = now;
  }

  fn width_at(&self, now: Duration) -> f32 {
    let (from, to, since) = self.width;
    from + (to - from) * eased(now.saturating_sub(since), RESIZE, ease_in_out)
  }

  /// Recording since `started`: whatever ran is dropped, the full pill shows at once.
  pub fn recording(&mut self, started: Duration, now: Duration) {
    if self.mode == Mode::Recording {
      return;
    }

    self.enter(Mode::Recording, now);
    (self.started, self.levels, self.ticked) = (started, [0.0; BARS], now);
    self.text = None;
    self.width = (WIDTH, WIDTH, now);
  }

  /// While recording: a warning (`reason`, `text_width` wide as drawn) in place of the level,
  /// the pill widening to it; `None` the level again.
  pub fn warning(&mut self, reason: Option<(&str, f32)>, now: Duration) {
    if self.mode != Mode::Recording || self.text.as_deref() == reason.map(|r| r.0) {
      return;
    }

    let to = reason.map_or(WIDTH, |(_, text_width)| reason_width(text_width));
    self.width = (self.width_at(now), to, now);
    self.text = reason.map(|r| r.0.to_owned());
  }

  /// Transcribing: the bars settle and become the wave; `n/m` for a recording in pieces. Not of a
  /// recording shown (one the menu or `wavo-app ctl` started while hidden): nothing to do.
  pub fn transcribing(&mut self, progress: Option<(usize, usize)>, now: Duration) {
    self.progress = progress;
    if self.mode != Mode::Recording {
      return;
    }

    self.settled_from = self.levels.map(bar_height);
    self.frozen = now.saturating_sub(self.started).as_secs();
    self.warning(None, now);
    self.enter(Mode::Transcribing, now);
  }

  /// An error's reason, `text_width` wide as drawn, for [`ERROR_HOLD`]; the pill widens to it.
  pub fn error(&mut self, reason: &str, text_width: f32, now: Duration) {
    let from =
      if self.mode == Mode::Hidden { reason_width(text_width) } else { self.width_at(now) };
    self.enter(Mode::Error, now);
    self.text = Some(reason.to_owned());
    self.width = (from, reason_width(text_width), now);
  }

  /// The text went where it goes: the pill collapses and goes. Nothing shown: nothing to do.
  pub fn collapse(&mut self, now: Duration) {
    if matches!(self.mode, Mode::Hidden | Mode::Closing) {
      return;
    }

    let from = self.width_at(now);
    self.from = self.mode;
    self.enter(Mode::Closing, now);
    self.width = (from, from, now);
  }

  /// The timer, every frame: a level for the meter while recording, every [METER_TICK]; at the
  /// end of an error's hold it collapses, at the end of the collapse it hides.
  pub fn tick(&mut self, level: f32, now: Duration) {
    let t = now.saturating_sub(self.since);
    match self.mode {
      Mode::Recording if now >= self.ticked + METER_TICK => {
        self.levels.rotate_left(1);
        self.levels[BARS - 1] = level;
        self.ticked = now;
      }
      Mode::Error if t >= ERROR_HOLD => self.collapse(now),
      Mode::Closing if t >= COLLAPSE => self.mode = Mode::Hidden,
      _ => {}
    }
  }

  /// The frame at `now`.
  pub fn frame(&self, now: Duration) -> Frame {
    let t = now.saturating_sub(self.since);
    let appear = self.appearing.map_or(1.0, |a| eased(now.saturating_sub(a), APPEAR, ease_out));
    // The collapse: the contents fade over 0–100 ms (linear), the width narrows to a circle over
    // 0–200 ms (ease-in), the pill fades over 120–260 ms (ease-in).
    let ms = Duration::from_millis;
    let (width, alpha, a) = match self.mode {
      Mode::Closing => (
        self.width.0 + (HEIGHT - self.width.0) * eased(t, ms(200), ease_in),
        1.0 - eased(t.saturating_sub(ms(120)), ms(140), ease_in),
        1.0 - eased(t, ms(100), |x| x),
      ),
      _ => (self.width_at(now), appear, 1.0),
    };

    let mut shapes = Vec::new();
    let time = |secs: u64| format!("{}:{:02}", secs / 60, secs % 60);
    let warning = || Shape::Warning { text: self.text.clone().unwrap_or_default(), alpha: a };
    // Closing fades out what was shown before it, as it was.
    let shown = if self.mode == Mode::Closing { self.from } else { self.mode };
    match shown {
      Mode::Hidden | Mode::Closing => {}
      Mode::Recording if self.text.is_some() => shapes.push(warning()),
      Mode::Recording => {
        shapes.push(Shape::Dot { alpha: a });
        for (i, level) in self.levels.iter().enumerate() {
          shapes.push(Shape::Bar { x: bar_x(i), height: bar_height(*level), alpha: a });
        }

        let end = if self.mode == Mode::Closing { self.since } else { now };
        let secs = end.saturating_sub(self.started).as_secs();
        shapes.push(Shape::Time { text: time(secs), alpha: a });
      }
      Mode::Transcribing => {
        let settle = match self.mode {
          Mode::Closing => 1.0,
          _ => eased(t, SETTLE, ease_out),
        };

        if settle < 1.0 {
          shapes.push(Shape::Dot { alpha: 1.0 - settle });
        }

        for (i, from) in self.settled_from.iter().enumerate() {
          let height = match self.mode {
            _ if settle < 1.0 => from + (WAVE_LOW - from) * settle,
            Mode::Closing => WAVE_LOW,
            _ => wave(i, t.saturating_sub(SETTLE)),
          };

          shapes.push(Shape::Bar { x: bar_x(i), height, alpha: SECONDARY * a });
        }

        shapes.push(match pieces(self.progress) {
          Some(pieces) => Shape::Time { text: pieces, alpha: SECONDARY * a },
          None => Shape::Time { text: time(self.frozen), alpha: TERTIARY * a },
        });
      }
      Mode::Error => shapes.push(warning()),
    }

    Frame { width, alpha, shapes }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::Phase as EnginePhase;
  use crate::session::{EngineStatus, Last, NotPasted, Phase};

  const MS: fn(u64) -> Duration = Duration::from_millis;

  #[test]
  fn the_pill_is_the_recordings_size() {
    assert_eq!(WIDTH - PAD - TIME_X, 40.0, "room for the time");
    assert_eq!(reason_width(10.0), WIDTH, "never narrower");
    assert_eq!(reason_width(1000.0), MAX_WIDTH, "nor wider");
    assert_eq!(reason_width(150.0), PAD + GLYPH + 8.0 + 150.0 + PAD);
  }

  #[test]
  fn pieces_show_as_n_of_m() {
    assert_eq!(pieces(None), None);
    assert_eq!(pieces(Some((0, 1))), None);
    assert_eq!(pieces(Some((0, 2))).as_deref(), Some("1/2"));
    assert_eq!(pieces(Some((2, 2))).as_deref(), Some("2/2"));
  }

  #[test]
  fn loudness_is_a_clamped_db_scale() {
    assert_eq!(loudness(0.0), 0.0);
    assert!(loudness(0.001) < 1e-5); // −60 dBFS
    assert!((loudness(0.01585) - 0.5).abs() < 0.01); // −36 dBFS
    assert_eq!(loudness(0.5), 1.0);
    assert_eq!((bar_height(0.0), bar_height(0.5)), (BAR_MIN, BAR_MAX));
  }

  #[test]
  fn the_curves_are_the_css_ones() {
    for ease in [ease_in, ease_out, ease_in_out] {
      assert!(ease(0.0).abs() < 1e-6 && (ease(1.0) - 1.0).abs() < 1e-6);
      let mut last = 0.0;
      for k in 1..=20 {
        let v = ease(k as f32 / 20.0);
        assert!(v >= last, "monotonic");
        last = v;
      }
    }

    assert!(ease_in(0.5) < 0.5 && ease_out(0.5) > 0.5);
    assert!((ease_in_out(0.5) - 0.5).abs() < 1e-3);
  }

  #[test]
  fn the_wave_travels_across_the_bars() {
    assert_eq!(wave(0, MS(0)), WAVE_LOW);
    assert!((wave(0, MS(600)) - WAVE_HIGH).abs() < 1e-4, "high after half a period");
    assert!((wave(0, MS(1200)) - WAVE_LOW).abs() < 1e-4, "low again");
    assert_eq!(wave(4, MS(200)), WAVE_LOW, "not started: 4 × 75 ms");
    assert!((wave(4, MS(300 + 600)) - WAVE_HIGH).abs() < 1e-4);
    for i in 0..BARS {
      for t in (0..3000).step_by(37) {
        assert!((WAVE_LOW..=WAVE_HIGH).contains(&wave(i, MS(t))));
      }
    }
  }

  fn status(last: Option<Last>, error: Option<&str>) -> Status {
    Status {
      state: Phase::Idle,
      recording: None,
      engine: EngineStatus { phase: EnginePhase::Stopped, ..EngineStatus::default() },
      last,
      retranscribing: Vec::new(),
      error: error.map(str::to_owned),
      warnings: Vec::new(),
      no_model: None,
    }
  }

  fn last(outcome: Ended, pasted: bool, error: Option<&str>, why: Option<&str>) -> Last {
    Last {
      take: 1,
      entry: None,
      outcome,
      text: "Hi".into(),
      error: error.map(str::to_owned),
      pasted,
      not_pasted: why.map(str::to_owned),
    }
  }

  #[test]
  fn a_dictation_that_went_well_just_goes() {
    let outcome = |l: Last, auto_paste| after(&status(Some(l), None), None, auto_paste);
    assert_eq!(outcome(last(Ended::Done, true, None, None), true), Some(Outcome::Collapse));
    let copied = last(Ended::Done, false, None, Some("auto-paste is off; the text is on it"));
    assert_eq!(outcome(copied, false), Some(Outcome::Collapse), "no extra feedback");
    let cancelled = last(Ended::Cancelled, false, None, None);
    assert_eq!(outcome(cancelled, true), Some(Outcome::Collapse));
    let silent = last(Ended::NoSpeech, false, None, None);
    assert_eq!(outcome(silent, true), Some(Outcome::Collapse), "nothing said: it just goes");
    let done = status(Some(last(Ended::Done, true, None, None)), None);
    assert_eq!(after(&done, Some(&done), true), None, "told already");
  }

  #[test]
  fn errors_say_why_in_a_few_lowercase_words() {
    let outcome = |l: Last, auto_paste| after(&status(Some(l), None), None, auto_paste);
    let error = |s: &str| Some(Outcome::Error(s.into()));
    let refused = |why: NotPasted| last(Ended::Done, false, None, Some(&why.to_string()));
    assert_eq!(outcome(refused(NotPasted::FocusChanged), true), error("not pasted: focus moved"));
    let unknown = refused(NotPasted::Failed("the app in focus is unknown".into()));
    assert_eq!(outcome(unknown, true), error("not pasted: the app in focus is unknown"));
    let field = refused(NotPasted::SecureField);
    assert_eq!(outcome(field, true), error("not pasted: password field"));
    let copy = last(Ended::Done, false, None, Some("auto-paste is off, and copying failed: x"));
    assert_eq!(outcome(copy, false), error("copy failed"));
    let died = Some("the engine panicked during a run");
    assert_eq!(outcome(last(Ended::Failed, false, died, None), true), error("engine crashed"));
    let mut nothing = last(Ended::NoText, false, None, None);
    nothing.text.clear();
    assert_eq!(outcome(nothing, false), error("nothing recognized"), "without auto-paste too");

    // Kept without a model: said, and where to get one.
    let gone = Some("parakeet-tdt-0.6b-v3 is not on disk");
    let gone = last(Ended::NoModel, false, gone, None);
    assert_eq!(outcome(gone, true), error("no model · open models"));
    let load = Some("parakeet-tdt-0.6b-v3 could not be loaded: not a GGUF speech model");
    let load = last(Ended::NoModel, false, load, None);
    assert_eq!(outcome(load, false), error("model failed to load · open models"));
    let mut recording = status(None, None);
    recording.state = Phase::Recording;
    assert_eq!(during(&recording), None);
    recording.no_model = Some(NoModel::NoneDownloaded);
    assert_eq!(during(&recording).as_deref(), Some("no model · open models"));
    recording.no_model = Some(NoModel::LoadFailed { model: "x".into(), error: "y".into() });
    assert_eq!(during(&recording).as_deref(), Some("model failed to load · open models"));

    // A recording that did not start: its error, once.
    let mic = "could not start recording: no input device";
    let failed = status(None, Some(mic));
    assert_eq!(after(&failed, None, true), error("could not start recording"));
    assert_eq!(after(&failed, Some(&failed), true), None);
    assert_eq!(short(&"x".repeat(80)).chars().count(), 40);
    assert_eq!(short(""), "failed");
  }

  fn has(f: &Frame, test: impl Fn(&Shape) -> bool) -> bool {
    f.shapes.iter().any(test)
  }

  #[test]
  fn the_state_goes_from_recording_through_the_wave_to_the_collapse() {
    let mut hud = State::default();
    assert_eq!(hud.frame(MS(0)).shapes, []);
    hud.recording(MS(0), MS(0));
    let first = hud.frame(MS(0));
    assert_eq!((first.width, first.alpha), (WIDTH, 0.0), "fading in");
    assert_eq!(hud.frame(APPEAR).alpha, 1.0);
    hud.tick(0.5, MS(100));
    let f = hud.frame(MS(3_200));
    assert!(f.shapes.contains(&Shape::Dot { alpha: 1.0 }));
    assert!(has(&f, |s| matches!(s, Shape::Time { text, .. } if text == "0:03")));
    assert!(has(&f, |s| matches!(s, Shape::Bar { height, .. } if *height == BAR_MAX)));

    hud.transcribing(Some((1, 3)), MS(3_200));
    assert!(has(&hud.frame(MS(3_290)), |s| matches!(s, Shape::Dot { .. })), "settling");
    assert!(!has(&hud.frame(MS(4_000)), |s| matches!(s, Shape::Dot { .. })), "the wave");
    assert_eq!(hud.text().as_deref(), Some("2/3"));

    hud.collapse(MS(5_000));
    assert_eq!(hud.mode(), Mode::Closing);
    let mid = hud.frame(MS(5_110));
    assert!(mid.alpha == 1.0 && mid.width < WIDTH && mid.width > HEIGHT, "narrowing, still there");
    let faded =
      |s: &Shape| matches!(s, Shape::Bar { alpha: 0.0, .. } | Shape::Time { alpha: 0.0, .. });
    assert!(mid.shapes.iter().all(faded));
    assert_eq!(hud.frame(MS(5_200)).width, HEIGHT, "a circle");
    hud.tick(0.0, MS(5_100));
    assert_eq!(hud.mode(), Mode::Closing);
    assert_eq!(hud.frame(MS(5_260)).alpha, 0.0);

    // A new dictation during the collapse: back at once, full and opaque.
    hud.recording(MS(5_100), MS(5_100));
    let back = hud.frame(MS(5_100));
    assert_eq!((back.width, back.alpha, hud.mode()), (WIDTH, 1.0, Mode::Recording));
    hud.collapse(MS(6_000));
    hud.tick(0.0, MS(6_000) + COLLAPSE);
    assert_eq!(hud.mode(), Mode::Hidden);
    hud.transcribing(None, MS(7_000));
    assert_eq!(hud.mode(), Mode::Hidden, "not of a recording shown");
  }

  #[test]
  fn an_error_widens_holds_then_collapses() {
    let mut hud = State::default();
    hud.recording(MS(0), MS(0));
    hud.transcribing(None, MS(1_000));
    hud.error("not pasted: focus moved", 200.0, MS(2_000));
    assert_eq!(hud.text().as_deref(), Some("warning: not pasted: focus moved"));
    assert!(hud.frame(MS(2_100)).width < reason_width(200.0), "widening");
    assert_eq!(hud.frame(MS(2_200)).width, reason_width(200.0));
    hud.tick(0.0, MS(5_999));
    assert_eq!(hud.mode(), Mode::Error);
    hud.tick(0.0, MS(2_000) + ERROR_HOLD);
    assert_eq!(hud.mode(), Mode::Closing);
    let closing = hud.frame(MS(6_050));
    assert!(closing.width < reason_width(200.0), "collapses from its width");
    let reason =
      |s: &Shape| matches!(s, Shape::Warning { text, .. } if text.starts_with("not pasted"));
    assert!(has(&closing, reason), "the reason fades out, no bars come instead");
    assert!(!has(&closing, |s| matches!(s, Shape::Bar { .. })));

    // From hidden it shows at its width at once.
    let mut hud = State::default();
    hud.error("no model", 60.0, MS(0));
    assert_eq!(hud.frame(MS(0)).width, reason_width(60.0));
  }

  #[test]
  fn a_recording_warns_until_its_warning_goes() {
    let mut hud = State::default();
    hud.warning(Some(("no sound from Mic", 150.0)), MS(0));
    assert_eq!(hud.text(), None, "only while recording");
    hud.recording(MS(0), MS(0));
    hud.warning(Some(("no sound from Mic", 150.0)), MS(1_000));
    assert_eq!(hud.mode(), Mode::Recording, "still recording");
    assert_eq!(hud.text().as_deref(), Some("warning: no sound from Mic"));
    let warned = hud.frame(MS(1_000) + RESIZE);
    assert_eq!(warned.width, reason_width(150.0));
    assert!(has(&warned, |s| matches!(s, Shape::Warning { .. })));
    assert!(!has(&warned, |s| matches!(s, Shape::Bar { .. } | Shape::Dot { .. })));

    hud.warning(Some(("no model · open models", 140.0)), MS(1_500));
    assert_eq!(hud.text().as_deref(), Some("warning: no model · open models"), "another one");
    hud.warning(None, MS(2_000));
    assert_eq!(hud.text(), None);
    let back = hud.frame(MS(2_000) + RESIZE);
    assert_eq!(back.width, WIDTH);
    assert!(back.shapes.contains(&Shape::Dot { alpha: 1.0 }));

    hud.warning(Some(("no sound from Mic", 150.0)), MS(3_000));
    hud.transcribing(None, MS(4_000));
    assert_eq!(hud.frame(MS(4_000) + RESIZE).width, WIDTH, "the wave in the recording's pill");
  }

  #[test]
  fn the_meter_takes_a_level_per_tick() {
    let mut hud = State::default();
    hud.recording(MS(0), MS(0));
    let tall = |f: &Frame| {
      let tall = |s: &&Shape| matches!(s, Shape::Bar { height, .. } if *height == BAR_MAX);
      f.shapes.iter().filter(tall).count()
    };
    for t in 0..10 {
      hud.tick(0.5, MS(t)); // every frame, faster than the meter
    }

    assert_eq!(tall(&hud.frame(MS(10))), 0);
    hud.tick(0.5, METER_TICK);
    assert_eq!(tall(&hud.frame(METER_TICK)), 1);
  }
}
