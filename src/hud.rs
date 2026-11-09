// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD: one pill in an overlay at the bottom or top of the screen with the pointer, over every
//! app, never focused. Recording: a red dot, the level as bars, the time. Transcribing: the bars
//! become a travelling wave, the time freezes (`n/m` for a recording in pieces). Then the pill
//! collapses to a circle and fades: no "done" state. An error shows a glyph and a short lowercase
//! reason for a few seconds, the pill widening to fit; while recording, a warning (no sound, no
//! model, a permission) takes the level's place. The pill is dark in either appearance: the
//! icon's colours (pink, violet, blue, cyan) travelling along it under a dictation; a still grey
//! under an error or a warning.

use std::f32::consts::TAU;
use std::sync::LazyLock;
use std::time::Duration;

use nimbo::ui::widgets::icon;
use nimbo::{
  Anchor, App, Color, Config, Font, Frame, Icon, ImageId, Quad, Rect, Theme, Ui, Weight, alpha, rgb,
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
// The white ring around the red dot, for the gradient under it.
const RING: f32 = 1.2;

const APPEAR: Duration = ms(100);
// The level's bars become the wave.
const SETTLE: Duration = ms(180);
const RESIZE: Duration = ms(200);
const COLLAPSE: Duration = ms(260);
const ERROR_HOLD: Duration = ms(4000);
// The time a dictation took, when shown (Advanced), before the collapse.
const DONE_HOLD: Duration = ms(1000);
const METER_TICK: Duration = ms(33);
const WAVE_HALF: Duration = ms(600);
const WAVE_STAGGER: Duration = ms(75);
// The gradient is made again this often while it shows, and at every new width: at the meter's
// tick, so a recording's frames carry both.
const FLOW_TICK: Duration = METER_TICK;

// The icon's colours travel along the pill, left to right: a band of them, pink to cyan and back,
// this long, a band each period; slanted as on the icon, this far right a point down.
const BAND: f32 = 2.0 * WIDTH;
const FLOW_PERIOD: f64 = 5.0;
const SLANT: f32 = 0.8;
// A slower ripple the other way bends the band a little: the colours bunch and spread.
const RIPPLE: f32 = 0.9 * WIDTH;
const RIPPLE_PERIOD: f64 = 7.0;
const RIPPLE_DEPTH: f32 = 0.05;
// The icon's colours (assets/icon.png's pixels, from its top left to its bottom right): pink
// (234, 234), violet (392, 392), indigo (560, 345), blue (646, 431), cyan (668, 668). Their hue
// and chroma at one lightness, deep enough for white text on all between (WCAG AA, 4.5:1, with
// room); one lightness, so the flow changes hue alone and never flickers.
const BRAND: [u32; 5] = [0xff50d0, 0x9b42fe, 0x5950fc, 0x1080f7, 0x2adcfc];
const LIGHTNESS: f32 = 0.5;
// Under the gradient, where it thins out at the rounded ends; an error's and a warning's.
const GROUND: Color = rgb(0x181028);
const GREY: Color = rgb(0x3a3a3c);
const EDGE: Color = alpha([1.0; 4], 0.24);
// White text on the gradient: the dark look in either appearance.
const THEME: Theme = Theme::PLATES_DARK;

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

  let cfg = Config::new(crate::INFO.name, MAX_WIDTH + 2.0 * MARGIN, HEIGHT + 2.0 * MARGIN);
  crate::with_icon(cfg.overlay(anchor).hidden())
}

// MARK: What it says

/// What came of a dictation that ended since `before`, or a new error of a recording that did not
/// start; `None` is no news. With `auto_paste` a refused paste is an error; with `took` a dictation
/// without one says how long its text took.
pub fn after(
  status: &Status,
  before: Option<&Status>,
  auto_paste: bool,
  took: bool,
) -> Option<Show> {
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
      // Without auto-paste only a failed copy is news.
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

  let done = last.took_ms.filter(|_| took && last.outcome == Outcome::Done).map(took_text);
  Some(match (error, done) {
    (Some(error), _) => Show::Error(error),
    (None, Some(done)) => Show::Done(done),
    (None, None) => Show::Collapse,
  })
}

/// `420 ms`: whole milliseconds.
fn took_text(ms: u64) -> String {
  format!("{ms} ms")
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
    // Automatic's, when only a closed lid's microphone is there.
    (&["the lid is closed"][..], "no microphone: lid closed"),
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
  /// `seconds` in so far; `warning` takes the level's place, over no sound too.
  Recording {
    meter: Option<Meter>,
    device: String,
    seconds: f64,
    warning: Option<String>,
  },
  Transcribing(Option<(usize, usize)>),
  Error(String),
  /// The dictation's text came this long after the stop.
  Done(String),
  Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  Hidden,
  Recording,
  Transcribing,
  Error,
  Done,
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
  // The gradient's image, the pill's width and the scale it was made for, and when.
  field: Option<(ImageId, f32, f32, Duration)>,
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
      field: None,
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
      (Show::Done(_), _) => Mode::Done,
      (Show::Transcribing(_), Mode::Recording) => Mode::Transcribing,
      (Show::Collapse, Mode::Recording | Mode::Transcribing | Mode::Error | Mode::Done) => {
        Mode::Closing
      }
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
      Show::Done(took) => {
        let from = if self.mode == Mode::Hidden { WIDTH } else { self.width_at(now) };
        self.enter(Mode::Done, now);
        self.text = Some(took);
        self.width = (from, WIDTH, now);
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

  /// While recording: the shell's warning first, as it blocks the dictation (no model, a
  /// permission), so the pill keeps it through the recording; no sound only without one.
  fn warn(&mut self, f: &mut Frame) {
    let silent = self.silent.then(|| format!("no sound from {}", self.device));
    let warning = self.warning.clone().or(silent).map(|w| fit(f, w));
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
      Mode::Done if t >= DONE_HOLD => self.collapse(now),
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

  /// What the contents show: a collapse's, what it fades out.
  fn shown(&self) -> Mode {
    if self.mode == Mode::Closing { self.from } else { self.mode }
  }

  /// The gradient under a dictation, not under an error or a warning.
  fn flows(&self) -> bool {
    let shown = self.shown();
    shown != Mode::Error && !(shown == Mode::Recording && self.text.is_some())
  }

  /// Makes the gradient again when [FLOW_TICK] has passed or the pill's width changed: an image
  /// is never updated, so the old one goes and the new one takes its id.
  fn flow(&mut self, f: &mut Frame) {
    let (now, width) = (f.now, self.pill(f.now).0);
    let fresh = self
      .field
      .is_some_and(|(_, w, scale, at)| (w, scale) == (width, f.scale) && now < at + FLOW_TICK);
    if !self.flows() || fresh {
      return;
    }

    // Of their periods, which they wrap at unseen: f32 keeps its precision over days on.
    let of = |period: f64| (now.as_secs_f64() % period / period) as f32;
    let (w, h, px) = field(width, of(FLOW_PERIOD), of(RIPPLE_PERIOD), f.scale);
    if let Some((id, ..)) = self.field.take() {
      f.images().remove(id);
    }

    self.field = Some((f.images().add(w, h, &px), width, f.scale, now));
  }

  /// When the next frame is due: `None` for the next refresh, while something moves (a fade, a
  /// resize, the wave, the collapse); else at the next change: the meter's tick (which polls it
  /// too), the gradient's, a hold's end. A tick due now is taken by this frame's paint: the next
  /// is a period on.
  fn due(&self, now: Duration) -> Option<Duration> {
    if now < self.appeared + APPEAR || now < self.width.2 + RESIZE {
      return None;
    }

    let next =
      |at: Duration, every: Duration| if at + every > now { at + every } else { now + every };
    let made = self.field.map_or(now, |(.., at)| at);
    let flow = if self.flows() { next(made, FLOW_TICK) } else { Duration::MAX };
    match self.mode {
      Mode::Recording => Some(next(self.ticked, METER_TICK).min(flow)),
      Mode::Error => Some(self.since + ERROR_HOLD),
      Mode::Done => Some((self.since + DONE_HOLD).min(flow)),
      _ => None,
    }
  }

  fn draw(&self, f: &mut Frame) {
    let (th, now) = (&THEME, f.now);
    let (width, pill_alpha, a) = self.pill(now);
    let fade = |c: Color, k: f32| alpha(c, c[3] * pill_alpha * k);
    let pill = Rect::new((f.size.w - width) / 2.0, MARGIN, width, HEIGHT);
    let shadow = Rect::new(pill.x, pill.y + 2.0, pill.w, pill.h);
    f.shadow(shadow, HEIGHT / 2.0, 6.0, fade(th.shadow, 1.0));
    let flows = self.flows();
    let ground = if flows { GROUND } else { GREY };
    f.rect(pill, fade(ground, 1.0), HEIGHT / 2.0);
    f.push_clip(pill);
    if let Some((id, w, ..)) = self.field.filter(|_| flows) {
      // On the pixel grid, drawn 1:1.
      let s = f.scale;
      let rect = Rect::new((pill.x * s).round() / s, pill.y, (w * s).ceil() / s, HEIGHT);
      f.image(id, rect, pill_alpha);
    }

    // The edge over the gradient's soft one.
    f.layer();
    let edge = Quad::new(pill, [0.0; 4]).radius(HEIGHT / 2.0).border(1.0, fade(EDGE, 1.0));
    f.quad(edge);

    // The contents keep their place as the pill narrows: centred at the recording's width.
    let x = (f.size.w - width.max(WIDTH)) / 2.0;
    let mid = pill.y + HEIGHT / 2.0;
    let baseline = |f: &mut Frame, font| mid + (f.metrics(font).cap_height / 2.0).round();
    let dot = |f: &mut Frame, k: f32| {
      let side = DOT + 2.0 * RING;
      let rect = Rect::new(x + PAD - RING, mid - side / 2.0, side, side);
      let ring = fade(alpha([1.0; 4], 0.9), k);
      f.quad(Quad::new(rect, fade(th.danger, k)).radius(side / 2.0).border(RING, ring));
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

    let t = now.saturating_sub(self.since);
    match (self.shown(), &self.text) {
      // The reason centred in the room right of the glyph.
      (Mode::Recording | Mode::Error, Some(text)) => {
        let glyph = Rect::new(x + PAD, mid - GLYPH / 2.0, GLYPH, GLYPH);
        icon::draw(f, TRIANGLE, glyph, fade(th.warning, a));
        icon::draw(f, MARK.stroke(2.2), glyph, fade(ground, a));
        // Never left of its place: a pill still widening to fit the reason has less room.
        let room = width.max(WIDTH) - REASON_X - PAD;
        let gap = ((room - f.measure(text, FONT)) / 2.0).max(0.0);
        let y = baseline(f, FONT);
        f.text(x + REASON_X + gap, y, text, FONT, fade(th.text, a));
      }
      // The check and the time, centred.
      (Mode::Done, Some(took)) => {
        let start = x + (WIDTH - GLYPH - 6.0 - f.measure(took, TIME)) / 2.0;
        let glyph = Rect::new(start, mid - GLYPH / 2.0, GLYPH, GLYPH);
        icon::draw(f, Icon::CHECK.stroke(2.2), glyph, fade(th.success, a));
        let y = baseline(f, TIME);
        f.text(start + GLYPH + 6.0, y, took, TIME, fade(th.text, a));
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
  /// Asks for the next frame when nothing moves: the HUD paints only what changed.
  fn view<'a>(&'a mut self, ui: &mut Ui<'a>) {
    if let Some(at) = self.due(ui.now()) {
      ui.request_frame_in(at.saturating_sub(ui.now()));
    }
  }

  fn paint(&mut self, f: &mut Frame) {
    // What the view asked for, from the state before this frame's changes.
    let asked = self.due(f.now);
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

      if let Some((id, ..)) = self.field.take() {
        f.images().remove(id);
      }

      return;
    }

    self.flow(f);
    self.draw(f);
    // The next refresh while something moves, or to ask for the new time when it changed.
    let due = self.due(f.now);
    if due.is_none() || due != asked {
      f.animate();
    }
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

// MARK: Gradient

/// The gradient of a pill `width` points wide, `flow` and `ripple` into their periods (0..1):
/// straight RGBA rows, a pixel per pixel at `scale`, drawn 1:1. A row is a strip of [BRAND]'s
/// band from its slant on, so a pixel is a lookup; it thins out to nothing at the pill's edge (a
/// rounded clip it doesn't have), where [GROUND] and the edge take over.
fn field(width: f32, flow: f32, ripple: f32, scale: f32) -> (usize, usize, Vec<u8>) {
  let (cols, rows) = ((width * scale).ceil() as usize, (HEIGHT * scale).round() as usize);
  let r = HEIGHT / 2.0;
  let at = |i: usize| (i as f32 + 0.5) / scale;
  let slant = |j: usize| (SLANT * j as f32).round() as usize;
  // From the pill's middle, so the colours stay put as it narrows; cos makes the band's ends
  // (pink, cyan) linger and turn back smoothly.
  let strip: Vec<[u8; 3]> = (0..cols + slant(rows))
    .map(|k| {
      let s = at(k) - width / 2.0;
      let u = s / BAND - flow + RIPPLE_DEPTH * (TAU * (s / RIPPLE + ripple)).sin();
      SHADES[((0.5 - 0.5 * (TAU * u).cos()) * 1023.0).round() as usize]
    })
    .collect();

  let mut px = Vec::with_capacity(cols * rows * 4);
  for j in 0..rows {
    let y = at(j);
    for (i, c) in strip[slant(j)..][..cols].iter().enumerate() {
      // Opaque a point inside the pill, the edge's width: the rows along the top and bottom too.
      let x = at(i);
      let dx = (r - x).max(x - (width - r)).max(0.0);
      let inside = r - (dx * dx + (y - r) * (y - r)).sqrt();
      px.extend_from_slice(&[c[0], c[1], c[2], (inside.clamp(0.0, 1.0) * 255.0) as u8]);
    }
  }

  (cols, rows, px)
}

/// [BRAND] from 0 to 1, mixed in OKLCH, for a lookup per pixel.
static SHADES: LazyLock<Vec<[u8; 3]>> = LazyLock::new(|| {
  let shade = |k| srgb(brand(k as f32 / 1023.0)).map(|v| v.round() as u8);
  (0..1024).map(shade).collect()
});

/// [BRAND] at `t`, 0..1, at [LIGHTNESS], mixed in OKLCH: a Catmull-Rom spline through the
/// stops' chroma and hue, the hue the short way round, so no grey between two colours and no
/// crease at a stop.
fn brand(t: f32) -> [f32; 3] {
  let last = BRAND.len() - 1;
  let at = t.clamp(0.0, 1.0) * last as f32;
  let i = (at as usize).min(last - 1);
  let k = at - i as f32;
  // The stops around the segment, the ends repeated.
  let mut p = [0, 1, 2, 3].map(|n| oklch(BRAND[(i + n).saturating_sub(1).min(last)]));
  for n in 1..4 {
    p[n][2] = p[n - 1][2] + (p[n][2] - p[n - 1][2] + 540.0) % 360.0 - 180.0;
  }

  let spline = |v: [f32; 4]| {
    let (a, b, c) = (v[2] - v[0], 2.0 * v[0] - 5.0 * v[1] + 4.0 * v[2] - v[3], v[3] - v[0]);
    v[1] + 0.5 * k * (a + k * (b + k * (c + 3.0 * (v[1] - v[2]))))
  };
  [LIGHTNESS, spline(p.map(|c| c[1])).max(0.0), spline(p.map(|c| c[2]))]
}

/// An sRGB colour (0xrrggbb) in OKLCH: lightness 0..1, chroma, hue in degrees (Björn Ottosson's
/// OKLab).
fn oklch(c: u32) -> [f32; 3] {
  let [r, g, b] = [16, 8, 0].map(|s| {
    let v = ((c >> s) & 0xff) as f32 / 255.0;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
  });
  let l = (0.4122215 * r + 0.5363325 * g + 0.0514460 * b).cbrt();
  let m = (0.2119035 * r + 0.6806995 * g + 0.107397 * b).cbrt();
  let s = (0.0883025 * r + 0.2817188 * g + 0.6299787 * b).cbrt();
  let a = 1.9779985 * l - 2.4285922 * m + 0.4505937 * s;
  let b = 0.0259040 * l + 0.7827718 * m - 0.8086758 * s;
  [0.2104543 * l + 0.7936178 * m - 0.0040720 * s, a.hypot(b), b.atan2(a).to_degrees()]
}

/// An OKLCH colour in sRGB, 0..255, its chroma cut to the most that fits: found by halving, so
/// colours side by side cut alike, without steps.
fn srgb([l, c, h]: [f32; 3]) -> [f32; 3] {
  let (sin, cos) = h.to_radians().sin_cos();
  let linear = |c: f32| {
    let (a, b) = (c * cos, c * sin);
    let l_ = (l + 0.3963378 * a + 0.2158038 * b).powi(3);
    let m_ = (l - 0.1055613 * a - 0.0638542 * b).powi(3);
    let s_ = (l - 0.0894842 * a - 1.2914855 * b).powi(3);
    [
      4.0767417 * l_ - 3.3077116 * m_ + 0.2309699 * s_,
      -1.268438 * l_ + 2.6097574 * m_ - 0.3413194 * s_,
      -0.0041961 * l_ - 0.7034186 * m_ + 1.7076147 * s_,
    ]
  };
  let fits = |c| linear(c).iter().all(|v| (-1e-4..=1.0 + 1e-4).contains(v));
  let (mut lo, mut hi) = if fits(c) { (c, c) } else { (0.0, c) };
  while hi - lo > 1e-5 {
    let mid = (lo + hi) / 2.0;
    if fits(mid) { lo = mid } else { hi = mid }
  }

  linear(lo).map(|v| {
    let v = v.clamp(0.0, 1.0);
    255.0 * if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
  })
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::path::PathBuf;
  use std::rc::Rc;

  use nimbo::Harness;

  use super::*;
  use crate::session::{Last, NotPasted};

  fn said(status: &Status, before: Option<&Status>, auto_paste: bool) -> Option<String> {
    match after(status, before, auto_paste, false)? {
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
      ("could not start recording: no microphone: the lid is closed", "no microphone: lid closed"),
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

  fn recording(warning: Option<&str>) -> Show {
    let warning = warning.map(str::to_owned);
    Show::Recording { meter: None, device: "Mic".into(), seconds: 7.0, warning }
  }

  /// What the HUD says of a dictation that ended as `last`, made by [after] as in the app.
  fn reason(last: Last) -> String {
    let status = Status { last: Some(Last { take: 1, ..last }), ..Status::default() };
    said(&status, None, true).unwrap()
  }

  fn refused(why: NotPasted) -> String {
    reason(Last { outcome: Outcome::Done, not_pasted: Some(why.to_string()), ..Last::default() })
  }

  fn crashed() -> String {
    let error = Some("the engine panicked".into());
    reason(Last { outcome: Outcome::Failed, error, ..Last::default() })
  }

  /// The longest a refused paste says: the app's own reason when it was too busy.
  fn too_busy() -> String {
    refused(NotPasted::Failed("the app was too busy to paste".into()))
  }

  /// The HUD in the harness, transparent as an overlay is, and how often it asked to be hidden.
  fn hud() -> (Harness<Hud>, Rc<Cell<u32>>) {
    let hidden = Rc::new(Cell::new(0));
    let count = hidden.clone();
    let config = config(HudPosition::Bottom);
    let mut h = Harness::new(Hud::new(move || count.set(count.get() + 1)), config.size, 2.0);
    h.set_transparent(true);
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
    assert!(!h.needs_frame(), "no frames once hidden");
  }

  /// With the setting, a dictation's time to text shows instead of the plain collapse; an error
  /// still wins.
  #[test]
  fn the_time_to_text_shows_only_when_asked() {
    let done = |took_ms, pasted| Status {
      last: Some(Last { take: 1, outcome: Outcome::Done, pasted, took_ms, ..Last::default() }),
      ..Status::default()
    };
    let shown = |s: &Status, took| after(s, None, true, took);
    assert!(matches!(shown(&done(Some(420), true), false), Some(Show::Collapse)));
    assert!(matches!(shown(&done(Some(420), true), true), Some(Show::Done(t)) if t == "420 ms"));
    assert!(
      matches!(shown(&done(Some(12_340), true), true), Some(Show::Done(t)) if t == "12340 ms")
    );
    let refused = Status {
      last: Some(Last {
        not_pasted: Some("focus moved".into()),
        ..done(Some(420), false).last.unwrap()
      }),
      ..Status::default()
    };
    assert!(matches!(shown(&refused, true), Some(Show::Error(_))), "an error wins");
  }

  #[test]
  fn the_time_holds_a_second_then_hides() {
    let (mut h, hidden) = hud();
    h.app_mut().show(Show::Done("420 ms".into()));
    h.wait(ms(300));
    assert_eq!(h.app().mode(), Mode::Done);
    let img = h.render();
    let mid = MARGIN + HEIGHT / 2.0;
    let green = (0..400).any(|x| {
      let [r, g, b, _] = img.pixel(x as f32, mid);
      g > 120 && g > r + 40 && g > b + 20
    });
    assert!(green, "the check");
    h.wait(DONE_HOLD);
    h.wait(COLLAPSE + ms(200));
    assert_eq!((h.app().mode(), hidden.get()), (Mode::Hidden, 1));
  }

  #[test]
  fn an_error_widens_the_pill_holds_then_hides() {
    let (mut h, hidden) = hud();
    h.app_mut().show(Show::Error(refused(NotPasted::SecureField)));
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

  /// A model still downloading and a silent microphone: the model's warning stays, as it blocks
  /// the dictation.
  #[test]
  fn a_blocking_warning_stays_over_no_sound() {
    let (mut h, _) = hud();
    let meter = Meter::default();
    let warning = Some("download a model".to_owned());
    let device = "Default Audio Device".to_owned();
    h.app_mut().show(Show::Recording { meter: Some(meter.clone()), device, seconds: 1.0, warning });
    h.wait(ms(300));
    assert_eq!(h.app().text.as_deref(), Some("download a model"));
    meter.set_silent(true);
    h.wait(ms(300));
    assert_eq!(h.app().text.as_deref(), Some("download a model"), "no switch to no sound");
  }

  /// Frames only when something changes: the meter's ticks while recording, none through an
  /// error's hold.
  #[test]
  fn frames_come_only_when_something_changes() {
    let (mut h, _) = hud();
    h.app_mut().show(recording(None));
    h.wait(ms(300));
    let frames = h.wait(ms(1000));
    assert!((28..=32).contains(&frames), "{frames} a second");
    h.app_mut().show(Show::Error(crashed()));
    h.wait(ms(300));
    assert_eq!(h.wait(ms(1000)), 0);
  }

  /// A short reason sits in the middle of the room right of the glyph, not against it; a long
  /// one never runs over the glyph, also while the pill widens to fit it.
  #[test]
  fn a_short_reason_is_centred_right_of_the_glyph() {
    let (mut h, _) = hud();
    h.app_mut().show(Show::Error(crashed()));
    h.wait(ms(300));
    let img = h.render();
    let left = (config(HudPosition::Bottom).size.w - WIDTH) / 2.0;
    let (from, to) = (left + REASON_X, left + WIDTH - PAD);
    let ink = |x: f32| (-6..=6).any(|dy| img.pixel(x, MARGIN + HEIGHT / 2.0 + dy as f32)[0] > 150);
    let xs: Vec<f32> = (0..((to - from) * 2.0) as usize).map(|i| from + i as f32 / 2.0).collect();
    let first = xs.iter().find(|&&x| ink(x)).unwrap();
    let last = xs.iter().rev().find(|&&x| ink(x)).unwrap();
    let (before, after) = (first - from, to - last);
    assert!(before > 10.0 && (before - after).abs() < 2.0, "{before} {after}");

    let (mut h, _) = hud();
    h.app_mut().show(recording(None));
    h.wait(ms(300));
    h.app_mut().show(Show::Error(too_busy()));
    h.wait(RESIZE / 2);
    let img = h.render();
    let mid = MARGIN + HEIGHT / 2.0;
    let left = (0..800).map(|x| x as f32 / 2.0).find(|&x| img.pixel(x, mid)[3] == 255).unwrap();
    let text =
      |x: f32| (-6..=6).any(|dy| img.pixel(x, mid + dy as f32)[..3].iter().all(|&c| c > 150));
    let over = (0..(REASON_X * 2.0) as usize).map(|i| left + i as f32 / 2.0).find(|&x| text(x));
    assert_eq!(over, None, "text over the glyph mid-resize, the pill from {left}");
  }

  /// Every state draws on the gradient or the grey, transparent around the pill; with
  /// `WAVO_SHOTS` set (`make shots`), written there.
  #[test]
  fn every_state_renders() {
    let states = [
      ("recording", vec![recording(None)]),
      ("transcribing", vec![recording(None), Show::Transcribing(None)]),
      ("done", vec![Show::Done("420 ms".into())]),
      ("error", vec![Show::Error(refused(NotPasted::SecureField))]),
      ("error-short", vec![Show::Error(crashed())]),
      ("error-long", vec![Show::Error(too_busy())]),
      ("warning", vec![recording(Some(&no_model(false)))]),
    ];
    let dir = std::env::var_os("WAVO_SHOTS").map(PathBuf::from);
    for (name, shows) in states {
      let (mut h, _) = hud();
      // A clock far enough on for the recording's seconds.
      h.advance(ms(60_000));
      for show in shows {
        h.app_mut().show(show);
        h.wait(ms(500));
      }

      h.app_mut().levels =
        std::array::from_fn(|i| 0.002 * (1.0 + (i as f32 * 1.7).sin()) * i as f32);
      h.redraw();
      let img = h.render();
      let mid = MARGIN + HEIGHT / 2.0;
      let [r, g, b, a] = img.pixel(config(HudPosition::Bottom).size.w / 2.0, MARGIN + 3.0);
      assert!(a == 255 && img.pixel(1.0, mid)[3] == 0, "{name}: {r} {g} {b} {a}");
      let grey = r.abs_diff(g) < 8 && g.abs_diff(b) < 8;
      assert_eq!(grey, name.starts_with("error") || name == "warning", "{name}: {r} {g} {b}");
      if let Some(dir) = &dir {
        std::fs::create_dir_all(dir).unwrap();
        h.shot(&dir.join(format!("hud-{name}.png"))).unwrap();
      }
    }
  }

  /// White reads on every colour of the gradient: WCAG AA's 4.5:1 for the time's text.
  #[test]
  fn white_text_reads_on_the_whole_gradient() {
    let luminance = |c: [u8; 3]| {
      let [r, g, b] = c.map(|v| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
      });
      0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let worst = SHADES.iter().map(|&c| 1.05 / (luminance(c) + 0.05)).fold(f32::MAX, f32::min);
    assert!(worst >= 4.5, "{worst:.2}:1");
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
