// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD: one pill in an overlay at the bottom or top of the screen with the pointer, over every
//! app, never focused. Recording: a red dot, the level as bars, the time. Transcribing: the bars
//! become a travelling wave, the time freezes (`n/m` for a recording in pieces). Then the pill
//! collapses to a circle and fades: no "done" state. An error shows a glyph and a short lowercase
//! reason for a few seconds, the pill widening to fit; while recording, a warning (no sound, no
//! model, a permission) takes the level's place.

use std::time::Duration;

use nimbo::ui::widgets::icon;
use nimbo::{
  Anchor, App, Color, Config, Cx, Event, Font, Frame, Icon, Quad, Rect, Theme, Weight, alpha,
};

use crate::audio::Meter;
use crate::session::{NoModel, Outcome, Status};
use crate::settings::HudPosition;

// Points.
const WIDTH: f32 = 176.0;
const HEIGHT: f32 = 36.0;
const MAX_WIDTH: f32 = 320.0;
// Room for the shadow around the pill.
const MARGIN: f32 = 12.0;
const PAD: f32 = 14.0;
const DOT: f32 = 10.0;
const BARS: usize = 16;
const BAR_WIDTH: f32 = 3.0;
const BAR_STEP: f32 = 5.0;
const BAR_MIN: f32 = 3.0;
const BAR_MAX: f32 = 20.0;
const BARS_X: f32 = PAD + DOT + 10.0;
const TIME_X: f32 = BARS_X + BARS as f32 * BAR_STEP + 8.0;
const GLYPH: f32 = 20.0;
const REASON_X: f32 = PAD + GLYPH + 8.0;
const WAVE_LOW: f32 = 3.0;
const WAVE_HIGH: f32 = 9.0;

const APPEAR: Duration = ms(100);
// The level's bars become the wave.
const SETTLE: Duration = ms(180);
const RESIZE: Duration = ms(200);
const COLLAPSE: Duration = ms(260);
const ERROR_HOLD: Duration = ms(4000);
const METER_TICK: Duration = ms(33);
const WAVE_HALF: Duration = ms(600);
const WAVE_STAGGER: Duration = ms(75);

const FONT: Font = Font::ui(13.0);
const TIME: Font = Font::mono(13.0).weight(Weight::Medium);
// A triangle with an exclamation mark cut out of it.
const TRIANGLE: Icon =
  Icon::filled("M10.3 3.9a2 2 0 0 1 3.4 0l8 13.6a2 2 0 0 1-1.7 3H4a2 2 0 0 1-1.7-3Z");
const MARK: Icon = Icon::new("M12 9v4.5M12 16.8v.1");

const fn ms(n: u64) -> Duration {
  Duration::from_millis(n)
}

pub fn config(position: HudPosition) -> Config {
  let anchor = match position {
    HudPosition::Bottom => Anchor::Bottom(44.0),
    HudPosition::Top => Anchor::Top(0.0),
  };

  let cfg = Config::new("Wavo", MAX_WIDTH + 2.0 * MARGIN, HEIGHT + 2.0 * MARGIN);
  crate::with_icon(cfg.overlay(anchor).hidden())
}

// MARK: What it says

/// What came of a dictation that ended since `before`, or a new error of a recording that did not
/// start; `None` is no news. With `auto_paste` a refused paste is an error.
pub fn after(status: &Status, before: Option<&Status>, auto_paste: bool) -> Option<Show> {
  let take = |s: &Status| s.last.as_ref().map(|l| l.take);
  let Some(last) = status.last.as_ref().filter(|l| Some(l.take) != before.and_then(take)) else {
    let shown = before.and_then(|s| s.error.as_deref());
    let error = status.error.as_deref().filter(|e| shown != Some(*e))?;
    return Some(Show::Error(short(error)));
  };

  let error = match last.outcome {
    Outcome::Done if last.pasted => None,
    Outcome::Done => match last.not_pasted.as_deref() {
      Some(why) if auto_paste => Some(format!("not pasted: {}", short(why))),
      // Without auto-paste the text is on the clipboard: only a failed copy is news.
      Some(why) if why.contains("copying failed") => Some("copy failed".into()),
      _ => None,
    },
    Outcome::NoSpeech | Outcome::Cancelled => None,
    Outcome::NoText => Some("nothing recognized".into()),
    Outcome::NoModel => {
      Some(no_model(last.error.as_deref().is_some_and(|e| e.contains("could not be loaded"))))
    }
    Outcome::Failed => Some(short(last.error.as_deref().unwrap_or("failed"))),
  };

  Some(error.map_or(Show::Collapse, Show::Error))
}

/// The warning while recording when no model can transcribe: the recording is kept for later.
pub fn during(status: &Status) -> Option<String> {
  let why = status.no_model.as_ref()?;
  Some(no_model(matches!(why, NoModel::LoadFailed { .. })))
}

fn no_model(load: bool) -> String {
  let what = if load { "model failed to load" } else { "download a model" };
  what.to_owned()
}

/// The known causes in a few words, else the first clause, lowercase.
fn short(error: &str) -> String {
  let lower = error.to_lowercase();
  // Refused by macOS's or Windows' privacy settings: a recording's error only, as a folder that
  // isn't writable says "permission denied" too.
  let mic = lower.contains("recording") || lower.contains("microphone");
  let refused = ["microphone access", "not authorized", "permission", "access is denied"];
  if mic && refused.iter().any(|k| lower.contains(k)) {
    return "no microphone access".into();
  }

  let known = [
    (&["no speech model", "no model"][..], "download a model"),
    // audio.rs's: none plugged in, or the chosen one gone with no default to fall back to.
    (&["no microphone found", "no default microphone"][..], "no microphone"),
    (&["could not open the microphone"][..], "microphone failed"),
    (&["password", "secure"][..], "password field"),
    (&["focus moved"][..], "focus moved"),
    (&["crash", "died", "panicked"][..], "engine crashed"),
    (&["timed out", "took longer"][..], "engine timed out"),
  ];
  if let Some((_, short)) = known.iter().find(|(keys, _)| keys.iter().any(|k| lower.contains(k))) {
    return (*short).to_owned();
  }

  let clause =
    lower.split([':', ';', '(', '\n']).next().unwrap().trim().trim_end_matches(['.', ',']);
  match clause.chars().count() {
    0 => "failed".into(),
    n if n > 40 => clause.chars().take(39).collect::<String>() + "…",
    _ => clause.to_owned(),
  }
}

// MARK: Hud

pub enum Show {
  /// `seconds` in so far; `warning` takes the level's place while the microphone sends sound.
  Recording {
    meter: Option<Meter>,
    device: String,
    seconds: f64,
    warning: Option<String>,
  },
  Transcribing(Option<(usize, usize)>),
  Error(String),
  Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  Hidden,
  Recording,
  Transcribing,
  Error,
  Closing,
}

/// Times are the overlay's clock (`Frame::now`): the shows apply at the next frame.
pub struct Hud {
  mode: Mode,
  // What a collapse fades out.
  from: Mode,
  since: Duration,
  appeared: Duration,
  started: Duration,
  ended: Duration,
  // The last levels, oldest first, and when the last came.
  levels: [f32; BARS],
  ticked: Duration,
  progress: Option<(usize, usize)>,
  // An error, or the warning in place of the level.
  text: Option<String>,
  // The pill's width on its way: from, to, since.
  width: (f32, f32, Duration),
  meter: Option<Meter>,
  device: String,
  warning: Option<String>,
  silent: bool,
  pending: Vec<Show>,
  theme: Theme,
  hidden: Box<dyn FnMut()>,
}

impl Hud {
  /// `hidden` runs when the collapse has ended.
  pub fn new(hidden: impl FnMut() + 'static) -> Self {
    Self {
      mode: Mode::Hidden,
      from: Mode::Hidden,
      since: Duration::ZERO,
      appeared: Duration::ZERO,
      started: Duration::ZERO,
      ended: Duration::ZERO,
      levels: [0.0; BARS],
      ticked: Duration::ZERO,
      progress: None,
      text: None,
      width: (WIDTH, WIDTH, Duration::ZERO),
      meter: None,
      device: String::new(),
      warning: None,
      silent: false,
      pending: Vec::new(),
      theme: Theme::PLATES_LIGHT,
      hidden: Box::new(hidden),
    }
  }

  pub fn show(&mut self, show: Show) {
    self.pending.push(show);
  }

  /// What it shows, or will at its next frame.
  pub fn mode(&self) -> Mode {
    self.pending.iter().fold(self.mode, |mode, show| match (show, mode) {
      (Show::Recording { .. }, _) => Mode::Recording,
      (Show::Error(_), _) => Mode::Error,
      (Show::Transcribing(_), Mode::Recording) => Mode::Transcribing,
      (Show::Collapse, Mode::Recording | Mode::Transcribing | Mode::Error) => Mode::Closing,
      (_, mode) => mode,
    })
  }

  fn enter(&mut self, mode: Mode, now: Duration) {
    match self.mode {
      Mode::Hidden => self.appeared = now,
      // Back at once, no fade-in for a pill still on screen.
      Mode::Closing => self.appeared = now.saturating_sub(APPEAR),
      _ => {}
    }

    (self.mode, self.since) = (mode, now);
  }

  fn width_at(&self, now: Duration) -> f32 {
    let (from, to, since) = self.width;
    lerp(from, to, eased(now.saturating_sub(since), RESIZE))
  }

  fn resize(&mut self, to: f32, now: Duration) {
    self.width = (self.width_at(now), to, now);
  }

  fn apply(&mut self, show: Show, f: &mut Frame) {
    let now = f.now;
    if !matches!(show, Show::Recording { .. }) {
      self.meter = None;
    }

    match show {
      Show::Recording { meter, device, seconds, warning } => {
        if self.mode != Mode::Recording {
          self.enter(Mode::Recording, now);
          self.started = now.saturating_sub(Duration::from_secs_f64(seconds));
          (self.levels, self.ticked, self.text) = ([0.0; BARS], now, None);
          self.width = (WIDTH, WIDTH, now);
        }

        (self.meter, self.device, self.warning) = (meter, device, warning);
      }
      Show::Transcribing(progress) => {
        self.progress = progress;
        // Not of a recording it did not show (one started while hidden).
        if self.mode == Mode::Recording {
          self.ended = now;
          self.enter(Mode::Transcribing, now);
          self.text = None;
          self.resize(WIDTH, now);
        }
      }
      Show::Error(reason) => {
        let reason = fit(f, reason);
        let to = reason_width(f.measure(&reason, FONT));
        let from = if self.mode == Mode::Hidden { to } else { self.width_at(now) };
        self.enter(Mode::Error, now);
        self.text = Some(reason);
        self.width = (from, to, now);
      }
      Show::Collapse => self.collapse(now),
    }
  }

  fn collapse(&mut self, now: Duration) {
    if matches!(self.mode, Mode::Hidden | Mode::Closing) {
      return;
    }

    if self.mode == Mode::Recording {
      self.ended = now;
    }

    let width = self.width_at(now);
    self.from = self.mode;
    self.enter(Mode::Closing, now);
    self.width = (width, width, now);
  }

  /// While recording: no sound first (the recording's own), else the shell's warning.
  fn warn(&mut self, f: &mut Frame) {
    let warning = match self.silent {
      true => Some(format!("no sound from {}", self.device)),
      false => self.warning.clone(),
    };
    let warning = warning.map(|w| fit(f, w));
    if self.mode != Mode::Recording || self.text == warning {
      return;
    }

    let to = warning.as_ref().map_or(WIDTH, |w| reason_width(f.measure(w, FONT)));
    self.resize(to, f.now);
    self.text = warning;
  }

  fn tick(&mut self, level: f32, now: Duration) {
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

  /// The pill's width, its opacity and its contents' at `now`.
  fn pill(&self, now: Duration) -> (f32, f32, f32) {
    match self.mode {
      // To a circle and gone, the contents first.
      Mode::Closing => {
        let k = eased(now.saturating_sub(self.since), COLLAPSE);
        (lerp(self.width.0, HEIGHT, k), 1.0 - k, (1.0 - 2.0 * k).max(0.0))
      }
      _ => (self.width_at(now), eased(now.saturating_sub(self.appeared), APPEAR), 1.0),
    }
  }

  fn draw(&self, f: &mut Frame) {
    let (th, now) = (&self.theme, f.now);
    let (width, pill_alpha, a) = self.pill(now);
    let fade = |c: Color, k: f32| alpha(c, c[3] * pill_alpha * k);
    let pill = Rect::new((f.size.w - width) / 2.0, MARGIN, width, HEIGHT);
    let shadow = Rect::new(pill.x, pill.y + 2.0, pill.w, pill.h);
    f.shadow(shadow, HEIGHT / 2.0, 6.0, fade(th.shadow, 1.0));
    let quad =
      Quad::new(pill, fade(th.menu, 1.0)).radius(HEIGHT / 2.0).border(1.0, fade(th.line, 1.0));
    f.quad(quad);
    f.layer();
    f.push_clip(pill);

    // The contents keep their place as the pill narrows: centred at the recording's width.
    let x = (f.size.w - width.max(WIDTH)) / 2.0;
    let mid = pill.y + HEIGHT / 2.0;
    let baseline = |f: &mut Frame, font| mid + (f.text.metrics(font).cap_height / 2.0).round();
    let dot = |f: &mut Frame, k: f32| {
      let rect = Rect::new(x + PAD, mid - DOT / 2.0, DOT, DOT);
      f.rect(rect, fade(th.danger, k), DOT / 2.0);
    };
    let bar = |f: &mut Frame, i: usize, height: f32, k: f32| {
      let rect = Rect::new(x + BARS_X + i as f32 * BAR_STEP, mid - height / 2.0, BAR_WIDTH, height);
      f.rect(rect, fade(th.text, k), BAR_WIDTH / 2.0);
    };
    let time = |f: &mut Frame, text: &str, k: f32| {
      let y = baseline(f, TIME);
      f.text(x + TIME_X, y, text, TIME, fade(th.text, k));
    };
    let clock = |end: Duration| {
      let secs = end.saturating_sub(self.started).as_secs();
      format!("{}:{:02}", secs / 60, secs % 60)
    };

    let shown = if self.mode == Mode::Closing { self.from } else { self.mode };
    let t = now.saturating_sub(self.since);
    match (shown, &self.text) {
      (Mode::Recording | Mode::Error, Some(text)) => {
        let glyph = Rect::new(x + PAD, mid - GLYPH / 2.0, GLYPH, GLYPH);
        icon::draw(f, TRIANGLE, glyph, fade(th.warning, a), 0.0);
        icon::draw(f, MARK, glyph, fade(th.menu, a), 2.2);
        let y = baseline(f, FONT);
        f.text(x + REASON_X, y, text, FONT, fade(th.text, a));
      }
      (Mode::Recording, None) => {
        dot(f, a);
        for (i, level) in self.levels.iter().enumerate() {
          bar(f, i, bar_height(*level), a);
        }

        let end = if self.mode == Mode::Closing { self.ended } else { now };
        time(f, &clock(end), a);
      }
      (Mode::Transcribing, _) => {
        let settle = if self.mode == Mode::Closing { 1.0 } else { eased(t, SETTLE) };
        if settle < 1.0 {
          dot(f, a * (1.0 - settle));
        }

        for (i, level) in self.levels.iter().enumerate() {
          let height = match self.mode {
            _ if settle < 1.0 => lerp(bar_height(*level), WAVE_LOW, settle),
            Mode::Closing => WAVE_LOW,
            _ => wave(i, t.saturating_sub(SETTLE)),
          };
          bar(f, i, height, 0.6 * a);
        }

        match self.progress {
          Some((done, total)) if total > 1 => {
            time(f, &format!("{}/{total}", (done + 1).min(total)), 0.6 * a)
          }
          _ => time(f, &clock(self.ended), 0.4 * a),
        }
      }
      _ => {}
    }

    f.pop_clip();
  }
}

impl App for Hud {
  fn event(&mut self, _cx: &mut Cx, ev: &Event) -> bool {
    let Event::Appearance(a) = ev else { return false };
    self.theme = Theme::system(*a);
    true
  }

  fn paint(&mut self, f: &mut Frame) {
    let shown = self.mode != Mode::Hidden;
    let shows = std::mem::take(&mut self.pending);
    let silent = self.meter.as_ref().is_some_and(Meter::silent);
    if !shows.is_empty() || silent != self.silent {
      for show in shows {
        self.apply(show, f);
      }

      self.silent = silent;
      self.warn(f);
    }

    self.tick(self.meter.as_ref().map_or(0.0, Meter::level), f.now);
    if self.mode == Mode::Hidden {
      if shown {
        (self.hidden)();
      }

      return;
    }

    self.draw(f);
    f.animate();
  }
}

/// `text` cut with "…" to what the widest pill holds: device names run long (Windows').
fn fit(f: &mut Frame, text: String) -> String {
  let room = MAX_WIDTH - REASON_X - PAD;
  if f.measure(&text, FONT) <= room {
    return text;
  }

  let mut cut: Vec<char> = text.chars().collect();
  while cut.pop().is_some() {
    let shown = format!("{}…", cut.iter().collect::<String>().trim_end());
    if f.measure(&shown, FONT) <= room {
      return shown;
    }
  }

  "…".into()
}

/// The pill's width for a reason `text` wide.
fn reason_width(text: f32) -> f32 {
  (REASON_X + text + PAD).clamp(WIDTH, MAX_WIDTH)
}

/// A level (linear RMS) on a dB scale: −60 dBFS and below is the shortest bar, −12 the tallest.
fn bar_height(level: f32) -> f32 {
  let db = 20.0 * level.max(1e-6).log10();
  lerp(BAR_MIN, BAR_MAX, ((db + 60.0) / 48.0).clamp(0.0, 1.0))
}

/// Bar `i` of the wave: low to high and back, each bar [`WAVE_STAGGER`] after the one before.
fn wave(i: usize, elapsed: Duration) -> f32 {
  let Some(t) = elapsed.checked_sub(WAVE_STAGGER * i as u32) else { return WAVE_LOW };
  let half = WAVE_HALF.as_secs_f32();
  let phase = t.as_secs_f32() % (2.0 * half) / half;
  lerp(WAVE_LOW, WAVE_HIGH, ease(if phase <= 1.0 { phase } else { 2.0 - phase }))
}

fn lerp(a: f32, b: f32, k: f32) -> f32 {
  a + (b - a) * k
}

/// How far `t` is into `span`, eased in and out.
fn eased(t: Duration, span: Duration) -> f32 {
  ease((t.as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0))
}

fn ease(t: f32) -> f32 {
  t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::rc::Rc;

  use nimbo::{Appearance, Harness};

  use super::*;
  use crate::session::{Last, NotPasted};

  fn said(status: &Status, before: Option<&Status>, auto_paste: bool) -> Option<String> {
    match after(status, before, auto_paste)? {
      Show::Error(reason) => Some(reason),
      _ => Some("collapse".into()),
    }
  }

  #[test]
  fn after_says_only_what_went_wrong() {
    let ended = |outcome, error: Option<&str>, not_pasted: Option<String>| Status {
      last: Some(Last {
        take: 1,
        outcome,
        pasted: not_pasted.is_none(),
        error: error.map(str::to_owned),
        not_pasted,
        ..Last::default()
      }),
      ..Status::default()
    };
    let refused = |why: NotPasted| ended(Outcome::Done, None, Some(why.to_string()));
    let cases = [
      (ended(Outcome::Done, None, None), true, "collapse"),
      (ended(Outcome::NoSpeech, None, None), true, "collapse"),
      (refused(NotPasted::FocusChanged), true, "not pasted: focus moved"),
      (refused(NotPasted::SecureField), true, "not pasted: password field"),
      (refused(NotPasted::Failed("x".into())), false, "collapse"),
      (ended(Outcome::NoText, None, None), false, "nothing recognized"),
      (ended(Outcome::NoModel, Some("x is not on disk"), None), true, "download a model"),
      (ended(Outcome::Failed, Some("the engine panicked"), None), true, "engine crashed"),
    ];
    for (status, auto_paste, expected) in cases {
      assert_eq!(said(&status, None, auto_paste).as_deref(), Some(expected));
    }

    let done = ended(Outcome::Done, None, None);
    assert_eq!(said(&done, Some(&done), true), None, "told already");
    // A recording that did not start: its error, once.
    let failed =
      Status { error: Some("could not start recording: no input".into()), ..done.clone() };
    assert_eq!(said(&failed, Some(&done), true).as_deref(), Some("could not start recording"));
    assert_eq!(said(&failed, Some(&failed), true), None);
    let errors = [
      ("could not start recording: no microphone found", "no microphone"),
      (
        "could not start recording: the microphone \"USB Mic\" is not connected, and there is no \
         default microphone",
        "no microphone",
      ),
      (
        "could not start recording: could not open the microphone \"USB Mic\": busy",
        "microphone failed",
      ),
      ("could not start recording: access is denied", "no microphone access"),
      (
        "could not save the settings: Permission denied (os error 13)",
        "could not save the settings",
      ),
    ];
    for (error, expected) in errors {
      let failed = Status { error: Some(error.into()), ..done.clone() };
      assert_eq!(said(&failed, Some(&done), true).as_deref(), Some(expected), "{error}");
    }
  }

  /// The HUD in the harness, transparent as an overlay is, and how often it asked to be hidden.
  fn hud() -> (Harness<Hud>, Rc<Cell<u32>>) {
    let hidden = Rc::new(Cell::new(0));
    let count = hidden.clone();
    let config = config(HudPosition::Bottom);
    let mut h = Harness::new(Hud::new(move || count.set(count.get() + 1)), config.size, 2.0);
    h.cpu.transparent = true;
    h.event(Event::Appearance(Appearance::DARK));
    (h, hidden)
  }

  #[test]
  fn a_dictation_records_transcribes_and_collapses() {
    let (mut h, hidden) = hud();
    let recording =
      Show::Recording { meter: None, device: "Mic".into(), seconds: 3.0, warning: None };
    h.app_mut().show(recording);
    assert_eq!(h.app().mode(), Mode::Recording, "as soon as it is asked");
    h.wait(ms(500));
    let img = h.render();
    let size = config(HudPosition::Bottom).size;
    assert_eq!(img.pixel(1.0, 1.0), [0; 4], "transparent around the pill");
    let (left, mid) = ((size.w - WIDTH) / 2.0, MARGIN + HEIGHT / 2.0);
    let [r, g, _, a] = img.pixel(left + PAD + DOT / 2.0, mid);
    assert!(r > 200 && g < 100 && a == 255, "the red dot: {r} {g} {a}");

    h.app_mut().show(Show::Transcribing(Some((0, 2))));
    h.wait(ms(300));
    assert_eq!(h.app().mode(), Mode::Transcribing);
    h.app_mut().show(Show::Collapse);
    h.wait(COLLAPSE + ms(100));
    assert_eq!((h.app().mode(), hidden.get()), (Mode::Hidden, 1));
    assert!(!h.rt.needs_frame(), "no frames once hidden");
  }

  #[test]
  fn an_error_widens_the_pill_holds_then_hides() {
    let (mut h, hidden) = hud();
    h.app_mut().show(Show::Error("not pasted: password field".into()));
    h.wait(ms(300));
    let img = h.render();
    let mid = MARGIN + HEIGHT / 2.0;
    let left = (0..400).map(|x| x as f32).find(|&x| img.pixel(x, mid)[3] == 255).unwrap();
    let recording = (config(HudPosition::Bottom).size.w - WIDTH) / 2.0;
    assert!(left < recording - 10.0, "wider than the recording's pill: {left}");
    let [r, g, b, _] = img.pixel(left + PAD + GLYPH / 2.0 - 4.0, mid + 5.0);
    assert!(r > 200 && g > 100 && b < 100, "the orange glyph: {r} {g} {b}");

    h.wait(ERROR_HOLD - ms(400));
    assert_eq!(h.app().mode(), Mode::Error);
    h.wait(COLLAPSE + ms(200));
    assert_eq!((h.app().mode(), hidden.get()), (Mode::Hidden, 1));
  }

  /// A Windows microphone's name in full would run past the pill's rounded end.
  #[test]
  fn a_long_reason_is_cut_to_the_widest_pill() {
    let (mut h, _) = hud();
    let device = "Microphone Array (Intel® Smart Sound Technology for Digital Microphones)";
    h.app_mut().show(Show::Error(format!("no sound from {device}")));
    h.wait(ms(300));
    let text = h.app().text.clone().unwrap();
    assert!(text.starts_with("no sound from Microphone") && text.ends_with('…'), "{text}");
  }
}
