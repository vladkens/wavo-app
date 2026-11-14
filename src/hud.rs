// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD: one pill in an overlay at the bottom or top of the screen with the pointer, over every
//! app, never focused. Recording: a red dot, the level as bars, the time, and after it the
//! dictation's language when it follows the keyboard layout (`RU`). Transcribing: the bars
//! become a travelling wave, the time freezes (`n/m` for a recording in pieces). Then the pill
//! collapses to a circle and fades: no "done" state. An error shows a glyph and a short lowercase
//! reason for a few seconds, the pill widening to fit; while recording, a warning (no sound, no
//! model, a permission) takes the level's place. Two styles (the settings'): the gradient, dark
//! in either appearance, the icon's colours (pink, violet, blue) drifting by as soft clouds under
//! a dictation and a still grey under an error or a warning; or the plain pill of the system's
//! appearance.

use std::f32::consts::TAU;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::LazyLock;
use std::time::Duration;

use nimbo::ui::widgets::icon;
use nimbo::{
  Anchor, App, Color, Config, Cx, Event, Font, Frame, Icon, ImageId, Quad, Rect, Theme, Ui, Weight,
  alpha, rgb,
};

use crate::audio::Meter;
use crate::session::{NoModel, Outcome, Status};
use crate::settings::{HudPosition, HudStyle};

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
// The language's chip after the time, and the room the pill widens by for it.
const CHIP_W: f32 = 24.0;
const CHIP_H: f32 = 17.0;
const CHIP_GAP: f32 = 2.0;
const CHIP_ROOM: f32 = CHIP_W + CHIP_GAP;

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

// The icon's colours drift by as soft clouds: three wide blobs side by side, each a colour, mixed
// by nearness, so the pill shows three at a time with no edge between them. A dictation takes a
// few seconds, so it all moves within the first two: the colours go round in this long, each
// cloud this far along them from the one before, together a good half of them.
const HUES_PERIOD: f64 = 9.0;
const HUES_APART: f32 = 0.3;
// A cloud's reach (a Gaussian's sigma); how far apart they sit, and how far from there they
// wander: across and up and down, in these periods (seconds), unalike, so they never line up the
// same way twice.
const CLOUD: f32 = 0.3 * WIDTH;
const SPREAD: f32 = WIDTH / 3.0;
const WANDER: (f32, f32) = (0.2 * WIDTH, 0.5 * HEIGHT);
const CLOUDS: [(f64, f64); 3] = [(6.1, 4.3), (7.3, 5.3), (8.9, 6.7)];
// A dictation starts anywhere in this much of the flow, a new place each time the HUD appears.
const STARTS: u64 = 3_600_000;
// Between the clouds the colour deepens, this much at most: the clouds' depth.
const DEEPEN: f32 = 0.25;
// The icon's colours (assets/icon.png), calmed, in OKLCH (lightness, chroma, hue): a soft orchid
// for its pink, its violet, indigo and blue, and back by a periwinkle and an iris, no two
// neighbours far apart. All about as light, dark enough for white text (WCAG AA's 4.5:1 with
// room), and as colourful: chroma 0.15 to 0.18, the orchid's less, as a warm hue glares sooner;
// the icon's own sit at sRGB's edge, where its pinks glare. Not its cyan: dark enough for white
// text, it is a muddy teal.
const BRAND: [[f32; 3]; 6] = [
  [0.55, 0.13, 328.0],
  [0.54, 0.17, 302.0],
  [0.53, 0.18, 282.0],
  [0.55, 0.155, 260.0],
  [0.54, 0.165, 272.0],
  [0.54, 0.17, 293.0],
];
// An error's and a warning's.
const GREY: Color = rgb(0x3a3a3c);
// The edge, a glass pill lit from above: white over the fill a point inside the pill, at its top
// and its bottom, so the rim is the fill's own colour, lighter, and no dark line, seen on any
// wallpaper; and a glow down from the top, its strength. An even rim: (0.35, 0.35) and 0.0.
const RIM: (f32, f32) = (0.5, 0.08);
const GLOW: f32 = 0.16;
// The time while transcribing, dimmed: still 3:1 on every colour.
const DIM: f32 = 0.76;
// White text on the gradient: the dark look in either appearance, its text white, as the
// contrast is reckoned for.
const DARK: Theme = Theme { text: rgb(0xffffff), ..Theme::PLATES_DARK };

const FONT: Font = Font::ui(13.0);
const CHIP: Font = Font::ui(11.0).weight(Weight::Semibold);
// The chip's code on its white in the gradient's style: the icon's violet, deep.
const CHIP_TEXT: Color = rgb(0x3b2a78);
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
    /// The keyboard layout's (`ru`), when the dictation follows it.
    language: Option<String>,
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
  // The dictation's language, when it follows the keyboard layout.
  language: Option<String>,
  pending: Vec<Show>,
  style: HudStyle,
  // The plain style's: the system's appearance.
  theme: Theme,
  // The gradient's image, the pill's width and the scale it was made for, and when; the edge's,
  // for its width and scale.
  field: Option<(ImageId, f32, f32, Duration)>,
  edge: Option<(ImageId, f32, f32)>,
  // Where in its flow the gradient is at the clock's zero: a new place, of `seed`'s, each time the
  // pill appears, which the dictation keeps to its end.
  start: Duration,
  seed: Box<dyn FnMut() -> u64>,
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
      language: None,
      pending: Vec::new(),
      style: HudStyle::Gradient,
      theme: Theme::PLATES_LIGHT,
      field: None,
      edge: None,
      start: Duration::ZERO,
      seed: Box::new(|| RandomState::new().build_hasher().finish()),
      hidden: Box::new(hidden),
    }
  }

  pub fn show(&mut self, show: Show) {
    self.pending.push(show);
  }

  /// The settings' style, from the next frame.
  pub fn set_style(&mut self, style: HudStyle) {
    self.style = style;
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
      Mode::Hidden => {
        self.appeared = now;
        self.start = Duration::from_millis((self.seed)() % STARTS);
      }
      // Back at once, no fade-in for a pill still on screen.
      Mode::Closing => self.appeared = now.saturating_sub(APPEAR),
      _ => {}
    }

    (self.mode, self.since) = (mode, now);
  }

  /// The width of a dictation's pill: the language's chip widens it.
  fn base(&self) -> f32 {
    WIDTH + if self.language.is_some() { CHIP_ROOM } else { 0.0 }
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
      Show::Recording { meter, device, seconds, warning, language } => {
        let new = self.mode != Mode::Recording;
        self.language = language;
        if new {
          self.enter(Mode::Recording, now);
          self.started = now.saturating_sub(Duration::from_secs_f64(seconds));
          (self.levels, self.ticked, self.text) = ([0.0; BARS], now, None);
          self.width = (self.base(), self.base(), now);
        } else if self.text.is_none() && self.width.1 != self.base() {
          self.resize(self.base(), now);
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
          self.resize(self.base(), now);
        }
      }
      Show::Error(reason) => {
        self.language = None;
        let reason = fit(f, reason);
        let to = reason_width(f.measure(&reason, FONT));
        let from = if self.mode == Mode::Hidden { to } else { self.width_at(now) };
        self.enter(Mode::Error, now);
        self.text = Some(reason);
        self.width = (from, to, now);
      }
      Show::Done(took) => {
        self.language = None;
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

    let to = warning.as_ref().map_or(self.base(), |w| reason_width(f.measure(w, FONT)));
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

  /// The gradient under a dictation, not under an error or a warning; never in the plain style.
  fn flows(&self) -> bool {
    let shown = self.shown();
    self.style == HudStyle::Gradient
      && shown != Mode::Error
      && !(shown == Mode::Recording && self.text.is_some())
  }

  /// Makes the gradient again when [FLOW_TICK] has passed or the pill's width changed, the edge
  /// at a new width: an image is never updated, so the old one goes and the new one takes its id.
  fn flow(&mut self, f: &mut Frame) {
    let (now, width, scale) = (f.now, self.pill(f.now).0, f.scale);
    let made = |w, s| (w, s) == (width, scale);
    if self.style == HudStyle::Gradient && !self.edge.is_some_and(|(_, w, s)| made(w, s)) {
      let id = renew(f, self.edge.map(|(id, ..)| id), edge(width, scale));
      self.edge = Some((id, width, scale));
    }

    let fresh = self.field.is_some_and(|(_, w, s, at)| made(w, s) && now < at + FLOW_TICK);
    if !self.flows() || fresh {
      return;
    }

    let id = renew(f, self.field.map(|(id, ..)| id), field(width, now + self.start, scale));
    self.field = Some((id, width, scale, now));
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
    let plain = self.style == HudStyle::Plain;
    let (th, now) = (if plain { &self.theme } else { &DARK }, f.now);
    let (width, pill_alpha, a) = self.pill(now);
    let fade = |c: Color, k: f32| alpha(c, c[3] * pill_alpha * k);
    // The gradient's pill starts on the pixel grid, where its images, drawn 1:1, start too, so
    // the grey under the edge meets it.
    let left = (f.size.w - width) / 2.0;
    let left = if plain { left } else { (left * f.scale).round() / f.scale };
    let pill = Rect::new(left, MARGIN, width, HEIGHT);
    let shadow = Rect::new(pill.x, pill.y + 2.0, pill.w, pill.h);
    f.shadow(shadow, HEIGHT / 2.0, 6.0, fade(th.shadow, 1.0));
    let flows = self.flows();
    let ground = if plain { th.menu } else { GREY };
    if plain {
      let quad = Quad::new(pill, fade(ground, 1.0)).radius(HEIGHT / 2.0);
      f.quad(quad.border(1.0, fade(th.line, 1.0)));
      f.layer();
      f.push_clip(pill);
    } else {
      // The gradient, else the grey; the edge over either.
      if !flows {
        f.rect(pill, fade(ground, 1.0), HEIGHT / 2.0);
      }

      f.push_clip(pill);
      let field = self.field.filter(|_| flows).map(|(id, w, ..)| (id, w));
      for (id, w) in [field, self.edge.map(|(id, w, _)| (id, w))].into_iter().flatten() {
        let rect = Rect::new(pill.x, pill.y, (w * f.scale).ceil() / f.scale, HEIGHT);
        f.image(id, rect, pill_alpha);
      }

      f.layer();
    }

    // The contents keep their place as the pill narrows: centred at the recording's width, a
    // dictation's at its own, the language's chip after the time.
    let x = (f.size.w - width.max(WIDTH)) / 2.0;
    let xd = (f.size.w - width.max(self.base())) / 2.0;
    let mid = pill.y + HEIGHT / 2.0;
    let baseline = |f: &mut Frame, font| mid + (f.metrics(font).cap_height / 2.0).round();
    let dot = |f: &mut Frame, k: f32| {
      if plain {
        let rect = Rect::new(xd + PAD, mid - DOT / 2.0, DOT, DOT);
        return f.rect(rect, fade(th.danger, k), DOT / 2.0);
      }

      let side = DOT + 2.0 * RING;
      let rect = Rect::new(xd + PAD - RING, mid - side / 2.0, side, side);
      let ring = fade(alpha([1.0; 4], 0.9), k);
      f.quad(Quad::new(rect, fade(th.danger, k)).radius(side / 2.0).border(RING, ring));
    };
    // White with the code in violet on the gradient; the plain pill's text colour, its code in the
    // pill's.
    let chip = |f: &mut Frame, k: f32| {
      let Some(language) = &self.language else { return };
      let rect = Rect::new(xd + WIDTH - PAD + CHIP_GAP, mid - CHIP_H / 2.0, CHIP_W, CHIP_H);
      let (fill, ink) = if plain { (th.text, th.menu) } else { ([1.0; 4], CHIP_TEXT) };
      f.rect(rect, fade(fill, k), 4.0);
      let code = language.to_uppercase();
      let at = rect.x + (rect.w - f.measure(&code, CHIP)) / 2.0;
      let y = baseline(f, CHIP);
      f.text(at, y, &code, CHIP, fade(ink, k));
    };
    let bar = |f: &mut Frame, i: usize, height: f32, k: f32| {
      let rect =
        Rect::new(xd + BARS_X + i as f32 * BAR_STEP, mid - height / 2.0, BAR_WIDTH, height);
      f.rect(rect, fade(th.text, k), BAR_WIDTH / 2.0);
    };
    let time = |f: &mut Frame, text: &str, k: f32| {
      let y = baseline(f, TIME);
      f.text(xd + TIME_X, y, text, TIME, fade(th.text, k));
    };
    let clock = |end: Duration| {
      let secs = end.saturating_sub(self.started).as_secs();
      format!("{}:{:02}", secs / 60, secs % 60)
    };

    let t = now.saturating_sub(self.since);
    match (self.shown(), &self.text) {
      // The reason centred in the room right of the glyph; in the plain style against it.
      (Mode::Recording | Mode::Error, Some(text)) => {
        let glyph = Rect::new(x + PAD, mid - GLYPH / 2.0, GLYPH, GLYPH);
        icon::draw(f, TRIANGLE, glyph, fade(th.warning, a));
        icon::draw(f, MARK.stroke(2.2), glyph, fade(ground, a));
        // Never left of its place: a pill still widening to fit the reason has less room.
        let room = width.max(WIDTH) - REASON_X - PAD;
        let gap = if plain { 0.0 } else { ((room - f.measure(text, FONT)) / 2.0).max(0.0) };
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
        chip(f, a);
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

        chip(f, a);
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
          _ => time(f, &clock(self.ended), if plain { 0.4 } else { DIM } * a),
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

      let images = [self.field.take().map(|(id, ..)| id), self.edge.take().map(|(id, ..)| id)];
      for id in images.into_iter().flatten() {
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

/// The gradient of a pill `width` points wide at `t` in its flow: straight RGBA rows, a pixel per
/// pixel at `scale`, drawn 1:1. A pixel's colour is the clouds' (see [CLOUDS]) mixed by nearness,
/// a Gaussian's weight each: a place among [SHADES], so every pixel is one of the icon's colours,
/// never a grey mix of two, and the change from cloud to cloud is as soft as the clouds are wide.
/// Dithered, so a slow change shows no bands. Its edge is the pill's, smoothed as a quad's.
fn field(width: f32, t: Duration, scale: f32) -> (usize, usize, Vec<u8>) {
  let r = HEIGHT / 2.0;
  // Of their periods, which they wrap at unseen: f32 keeps its precision over days on.
  let of = |period: f64| (t.as_secs_f64() % period / period) as f32;
  let hues = of(HUES_PERIOD);
  // From the pill's middle, so the colours stay put as it narrows.
  let clouds = CLOUDS.iter().enumerate().map(|(n, &(across, up))| {
    let phase = n as f32 / CLOUDS.len() as f32;
    let home = width / 2.0 + (n as f32 - 1.0) * SPREAD;
    let x = home + WANDER.0 * (TAU * (of(across) + phase)).sin();
    let y = r + WANDER.1 * (TAU * (of(up) + phase)).sin();
    (x, y, hues + n as f32 * HUES_APART)
  });
  let clouds: Vec<_> = clouds.collect();

  pixels(width, scale, |(i, j), (x, y)| {
    // The clouds' cores, their weights squared, are narrower: the depth.
    let (mut sum, mut hue, mut core) = (0.0, 0.0, 0.0);
    for &(cx, cy, h) in &clouds {
      let w = (-((x - cx).powi(2) + (y - cy).powi(2)) / (2.0 * CLOUD * CLOUD)).exp();
      (sum, hue, core) = (sum + w, hue + w * h, core + w * w);
    }

    let shade = SHADES[((hue / sum).rem_euclid(1.0) * SHADES.len() as f32) as usize % SHADES.len()];
    let deep = 1.0 - DEEPEN * (1.0 - core.min(1.0));
    // Interleaved gradient noise (Jimenez), half a step either way, still from frame to frame.
    let noise = (52.982_918 * (0.067_110_56 * i as f32 + 0.005_837_15 * j as f32).fract()).fract();
    let [red, green, blue] =
      shade.map(|v| (v * deep + noise - 0.5).round().clamp(0.0, 255.0) as u8);
    [red, green, blue, 255]
  })
}

/// The pill's edge, white over its fill (see [RIM] and [GLOW]), as [field]'s pixels.
fn edge(width: f32, scale: f32) -> (usize, usize, Vec<u8>) {
  pixels(width, scale, |_, (x, y)| {
    // A point wide, its inner side smoothed too.
    let rim = ((1.0 - inside(width, x, y)) * scale + 0.5).clamp(0.0, 1.0);
    let rim = rim * lerp(RIM.1, RIM.0, 1.0 - y / HEIGHT);
    let glow = GLOW * (1.0 - y / (0.4 * HEIGHT)).max(0.0).powi(2);
    [255, 255, 255, (rim.max(glow) * 255.0).round() as u8]
  })
}

/// The straight RGBA rows of an image of a pill `width` points wide, a pixel per pixel at `scale`:
/// `pixel` of each one's column and row and its middle in points, its alpha cut to the pill as a
/// quad's edge is, so the two meet with no seam.
fn pixels(
  width: f32,
  scale: f32,
  mut pixel: impl FnMut((usize, usize), (f32, f32)) -> [u8; 4],
) -> (usize, usize, Vec<u8>) {
  let (cols, rows) = ((width * scale).ceil() as usize, (HEIGHT * scale).round() as usize);
  let at = |i: usize| (i as f32 + 0.5) / scale;
  let mut px = Vec::with_capacity(cols * rows * 4);
  for j in 0..rows {
    for i in 0..cols {
      let (x, y) = (at(i), at(j));
      let [red, green, blue, a] = pixel((i, j), (x, y));
      let cover = (0.5 + inside(width, x, y) * scale).clamp(0.0, 1.0);
      px.extend_from_slice(&[red, green, blue, (a as f32 * cover).round() as u8]);
    }
  }

  (cols, rows, px)
}

/// How far a point is inside a pill `width` points wide, in points: below 0 outside.
fn inside(width: f32, x: f32, y: f32) -> f32 {
  let r = HEIGHT / 2.0;
  let dx = (r - x).max(x - (width - r)).max(0.0);
  r - (dx * dx + (y - r) * (y - r)).sqrt()
}

/// `image` in place of `old`: an image is never updated, so the old one goes, its id taken anew.
fn renew(f: &mut Frame, old: Option<ImageId>, (w, h, px): (usize, usize, Vec<u8>)) -> ImageId {
  if let Some(id) = old {
    f.images().remove(id);
  }

  f.images().add(w, h, &px)
}

/// [BRAND] round, from 0 to 1, mixed in OKLCH, 0..255 unrounded for the dither: a lookup per
/// pixel.
static SHADES: LazyLock<Vec<[f32; 3]>> =
  LazyLock::new(|| (0..4096).map(|k| srgb(brand(k as f32 / 4096.0))).collect());

/// [BRAND] round at `t`, 0..1 (the last back to the first), mixed in OKLCH: a Catmull-Rom spline
/// through their lightness, chroma and hue, the hue the short way round, so no grey between two
/// colours and no crease at a stop.
fn brand(t: f32) -> [f32; 3] {
  let n = BRAND.len();
  let at = t.rem_euclid(1.0) * n as f32;
  let (i, k) = (at as usize % n, at.fract());
  // The stops around the segment.
  let mut p = [0, 1, 2, 3].map(|m| BRAND[(i + n + m - 1) % n]);
  for m in 1..4 {
    p[m][2] = p[m - 1][2] + (p[m][2] - p[m - 1][2] + 540.0) % 360.0 - 180.0;
  }

  let spline = |v: [f32; 4]| {
    let (a, b, c) = (v[2] - v[0], 2.0 * v[0] - 5.0 * v[1] + 4.0 * v[2] - v[3], v[3] - v[0]);
    v[1] + 0.5 * k * (a + k * (b + k * (c + 3.0 * (v[1] - v[2]))))
  };
  let [l, c, h] = [0, 1, 2].map(|n| spline(p.map(|c| c[n])));
  [l, c.max(0.0), h]
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

  use nimbo::{Appearance, Harness};

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
    Show::Recording { meter: None, device: "Mic".into(), seconds: 7.0, warning, language: None }
  }

  /// A recording that follows the keyboard layout, now in `language`.
  fn speaking(language: &str) -> Show {
    let language = Some(language.to_owned());
    Show::Recording { meter: None, device: "Mic".into(), seconds: 7.0, warning: None, language }
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
    styled(HudStyle::Gradient)
  }

  /// [hud] in `style`, the system dark.
  fn styled(style: HudStyle) -> (Harness<Hud>, Rc<Cell<u32>>) {
    let hidden = Rc::new(Cell::new(0));
    let count = hidden.clone();
    let config = config(HudPosition::Bottom);
    let mut h = Harness::new(Hud::new(move || count.set(count.get() + 1)), config.size, 2.0);
    h.set_transparent(true);
    // The gradient's starts, the same in every run.
    let mut n = 0u64;
    h.app_mut().seed = Box::new(move || {
      n += 1;
      n.wrapping_mul(0x9e37_79b9_7f4a_7c15)
    });
    h.event(Event::Appearance(Appearance::DARK));
    h.app_mut().set_style(style);
    (h, hidden)
  }

  /// The pill's colour near its top in the middle, where no content is.
  fn fill(h: &mut Harness<Hud>) -> [u8; 4] {
    h.render().pixel(config(HudPosition::Bottom).size.w / 2.0, MARGIN + 3.0)
  }

  fn grey([r, g, b, _]: [u8; 4]) -> bool {
    r.abs_diff(g) < 8 && g.abs_diff(b) < 8
  }

  /// White's contrast on an sRGB colour, 0..255 (WCAG's).
  fn contrast(c: [f32; 3]) -> f32 {
    1.05 / (luminance(c) + 0.05)
  }

  /// An sRGB colour's relative luminance, 0..255 (WCAG's).
  fn luminance(c: [f32; 3]) -> f32 {
    let [r, g, b] = c.map(|v| {
      let v = v / 255.0;
      if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
  }

  #[test]
  fn a_dictation_records_transcribes_and_collapses() {
    let (mut h, hidden) = hud();
    let recording = Show::Recording {
      meter: None,
      device: "Mic".into(),
      seconds: 3.0,
      warning: None,
      language: None,
    };
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

  /// A dictation that follows the keyboard layout: its language's chip after the time, white, in
  /// a pill that widens for it, the dot where it was; a change of language changes the code and
  /// nothing moves; the dictation's end goes back to the narrow pill.
  #[test]
  fn the_language_shows_after_the_time() {
    let (mut h, _) = hud();
    h.app_mut().show(speaking("ru"));
    h.wait(ms(500));
    assert_eq!(h.app().width.1, WIDTH + CHIP_ROOM);
    let size = config(HudPosition::Bottom).size;
    let (left, mid) = ((size.w - WIDTH - CHIP_ROOM) / 2.0, MARGIN + HEIGHT / 2.0);
    let [r, g, _, _] = h.render().pixel(left + PAD + DOT / 2.0, mid);
    assert!(r > 200 && g < 100, "the red dot where it was: {r} {g}");
    let [r, g, b, _] =
      h.render().pixel(left + WIDTH - PAD + CHIP_GAP + 1.5, mid - CHIP_H / 2.0 + 1.5);
    assert!(r > 230 && g > 230 && b > 230, "the chip's white: {r} {g} {b}");

    h.app_mut().show(speaking("en"));
    h.wait(ms(500));
    assert_eq!((h.app().language.as_deref(), h.app().width.1), (Some("en"), WIDTH + CHIP_ROOM));
    h.app_mut().show(Show::Transcribing(None));
    h.wait(ms(500));
    assert_eq!(h.app().width.1, WIDTH + CHIP_ROOM, "the chip stays while transcribing");
    h.app_mut().show(Show::Done("420 ms".into()));
    h.wait(ms(500));
    assert_eq!((h.app().language.as_deref(), h.app().width.1), (None, WIDTH));
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
    let shown = Some(meter.clone());
    h.app_mut().show(Show::Recording {
      meter: shown,
      device,
      seconds: 1.0,
      warning,
      language: None,
    });
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

  /// Every state draws on the gradient or the grey, or in the plain style on the system's pill,
  /// transparent around the pill; with `WAVO_SHOTS` set (`make shots`), written there: `hud-*.png`
  /// and `hud-plain-*.png`.
  #[test]
  fn every_state_renders() {
    let states = || {
      [
        ("recording", vec![recording(None)]),
        ("recording-language", vec![speaking("ru")]),
        ("transcribing", vec![recording(None), Show::Transcribing(None)]),
        ("transcribing-language", vec![speaking("en"), Show::Transcribing(None)]),
        ("done", vec![Show::Done("420 ms".into())]),
        ("error", vec![Show::Error(refused(NotPasted::SecureField))]),
        ("error-short", vec![Show::Error(crashed())]),
        ("error-long", vec![Show::Error(too_busy())]),
        ("warning", vec![recording(Some(&no_model(false)))]),
      ]
    };
    let dir = std::env::var_os("WAVO_SHOTS").map(PathBuf::from);
    for (style, prefix) in [(HudStyle::Gradient, "hud"), (HudStyle::Plain, "hud-plain")] {
      for (name, shows) in states() {
        let (mut h, _) = styled(style);
        // A clock far enough on for the recording's seconds.
        h.advance(ms(60_000));
        for show in shows {
          h.app_mut().show(show);
          h.wait(ms(500));
        }

        h.app_mut().levels =
          std::array::from_fn(|i| 0.002 * (1.0 + (i as f32 * 1.7).sin()) * i as f32);
        let px = fill(&mut h);
        let img = h.render();
        let around = img.pixel(1.0, MARGIN + HEIGHT / 2.0)[3];
        assert!(px[3] == 255 && around == 0, "{prefix}-{name}: {px:?}");
        let still = style == HudStyle::Plain || name.starts_with("error") || name == "warning";
        assert_eq!(grey(px), still, "{prefix}-{name}: {px:?}");
        if let Some(dir) = &dir {
          std::fs::create_dir_all(dir).unwrap();
          h.shot(&dir.join(format!("{prefix}-{name}.png"))).unwrap();
        }
      }
    }
  }

  /// The setting's style: the gradient flowing, or the plain pill of the system's appearance,
  /// light or dark, which paints only at the meter's ticks too; the switch shows at once.
  #[test]
  fn the_style_is_the_settings() {
    let (mut h, _) = hud();
    h.app_mut().show(recording(None));
    h.wait(ms(500));
    let before = fill(&mut h);
    h.wait(ms(1000));
    let after = fill(&mut h);
    assert!(!grey(before) && before != after, "flowing: {before:?} {after:?}");

    h.app_mut().set_style(HudStyle::Plain);
    let dark = fill(&mut h);
    assert!(grey(dark) && dark[0] < 80, "the dark pill: {dark:?}");
    h.wait(ms(300));
    assert_eq!(fill(&mut h), dark, "still");
    assert!((28..=32).contains(&h.wait(ms(1000))));
    h.event(Event::Appearance(Appearance::LIGHT));
    let light = fill(&mut h);
    assert!(grey(light) && light[0] > 200, "the light pill: {light:?}");
  }

  /// White reads on every colour of the gradient, the dither's half step up included: WCAG AA's
  /// 4.5:1 for the time's text.
  #[test]
  fn white_text_reads_on_the_whole_gradient() {
    let worst = SHADES.iter().map(|c| contrast(c.map(|v| v + 0.5))).fold(f32::MAX, f32::min);
    assert!(worst >= 4.5, "{worst:.2}:1");
  }

  /// The gradient neither glares nor greys: every colour of it about as light and as colourful as
  /// the rest.
  #[test]
  fn the_gradient_is_calm_and_never_grey() {
    for k in 0..4096 {
      let [l, c, h] = brand(k as f32 / 4096.0);
      assert!((0.52..=0.56).contains(&l) && (0.12..=0.19).contains(&c), "{l} {c} {h}");
    }
  }

  /// The dimmed time while transcribing still reads, 3:1 on every colour, the deepest too.
  #[test]
  fn the_dimmed_time_reads_on_the_whole_gradient() {
    let worst = SHADES
      .iter()
      .flat_map(|&c| [1.0, 1.0 - DEEPEN].map(|deep| c.map(|v| v * deep + 0.5)))
      .map(|c| (luminance(c.map(|v| lerp(v, 255.0, DIM))) + 0.05) / (luminance(c) + 0.05))
      .fold(f32::MAX, f32::min);
    assert!(worst >= 3.0, "{worst:.2}:1");
  }

  /// Each time the pill appears the gradient starts somewhere new, and carries on from there as
  /// the dictation transcribes and is done: no jump.
  #[test]
  fn a_dictation_starts_the_gradient_anywhere_and_keeps_it() {
    let (mut h, _) = hud();
    let seeds = Rc::new(Cell::new(0));
    let count = seeds.clone();
    h.app_mut().seed = Box::new(move || {
      count.set(count.get() + 1);
      count.get() * 1_234_567
    });
    let step = |a: [u8; 4], b: [u8; 4]| (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap();
    h.app_mut().show(recording(None));
    h.wait(ms(500));
    let mut before = fill(&mut h);
    for show in [Show::Transcribing(None), Show::Done("420 ms".into())] {
      h.app_mut().show(show);
      h.wait(FLOW_TICK);
      let after = fill(&mut h);
      assert!(step(before, after) < 16, "{before:?} {after:?}");
      before = after;
    }

    assert_eq!(seeds.get(), 1);
    h.wait(DONE_HOLD + COLLAPSE + ms(100));
    assert_eq!(h.app().mode(), Mode::Hidden);
    h.app_mut().show(recording(None));
    h.wait(ms(500));
    assert_eq!(seeds.get(), 2, "a new start");
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
