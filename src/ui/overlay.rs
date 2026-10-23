// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD on nimbo: an overlay ([Config::overlay]) at the bottom or top of the screen with the
//! pointer, over every app, never taking the focus, that draws [hud::State]'s frames in the
//! theme's colours. The shell tells it what to show ([Overlay::show]); that applies at its next
//! frame, on its clock, where text can be measured. While it shows it draws every frame and reads
//! the recording's level and silence from the [Meter]; once the state has hidden, it calls
//! `hidden` and the shell hides the window.

use nimbo::ui::widgets::icon;
use nimbo::{
  Anchor, App, Color, Config, Cx, Event, Font, Frame, Icon, Quad, Rect, Theme, Weight, alpha,
};

use super::hud::{self, Mode, Shape, State};
use crate::audio::recorder::Meter;
use crate::settings::HudPosition;

const FONT: Font = Font::ui(13.0);
const TIME: Font = Font::mono(13.0).weight(Weight::Medium);
/// A warning: a triangle with an exclamation mark cut out of it.
const TRIANGLE: Icon =
  Icon::filled("M10.3 3.9a2 2 0 0 1 3.4 0l8 13.6a2 2 0 0 1-1.7 3H4a2 2 0 0 1-1.7-3Z");
const MARK: Icon = Icon::new("M12 9v4.5M12 16.8v.1");

/// The overlay at `position`: the widest pill and room for its shadow, shown on demand.
pub fn config(position: HudPosition) -> Config {
  let anchor = match position {
    HudPosition::Bottom => Anchor::Bottom(44.0),
    HudPosition::Top => Anchor::Top(0.0),
  };

  let (w, h) = (hud::MAX_WIDTH + 2.0 * hud::MARGIN, hud::HEIGHT + 2.0 * hud::MARGIN);
  Config::new("Wavo", w, h).overlay(anchor).hidden()
}

/// What the shell asks the HUD to show.
pub enum Show {
  /// Recording for `seconds` so far, its level and silence from `meter`, from `device`; with
  /// `no_model` the warning while the microphone sends sound.
  Recording {
    meter: Option<Meter>,
    device: String,
    seconds: f64,
    no_model: Option<String>,
  },
  Transcribing(Option<(usize, usize)>),
  Error(String),
  Collapse,
}

pub struct Overlay {
  state: State,
  shows: Vec<Show>,
  meter: Option<Meter>,
  device: String,
  no_model: Option<String>,
  theme: Theme,
  hidden: Box<dyn FnMut()>,
}

impl Overlay {
  /// `hidden` runs as the state hides (the collapse ended).
  pub fn new(hidden: impl FnMut() + 'static) -> Self {
    Self {
      state: State::default(),
      shows: Vec::new(),
      meter: None,
      device: String::new(),
      no_model: None,
      theme: Theme::PLATES_LIGHT,
      hidden: Box::new(hidden),
    }
  }

  /// Shows `show` from the next frame on.
  pub fn show(&mut self, show: Show) {
    self.shows.push(show);
  }

  /// What it shows, or will at its next frame.
  pub fn mode(&self) -> Mode {
    match self.shows.last() {
      Some(Show::Recording { .. }) => Mode::Recording,
      Some(Show::Error(_)) => Mode::Error,
      _ => self.state.mode(),
    }
  }

  /// See [State::text].
  pub fn text(&self) -> Option<String> {
    self.state.text()
  }

  fn apply(&mut self, show: Show, f: &mut Frame) {
    let now = f.now;
    if !matches!(show, Show::Recording { .. }) {
      self.meter = None;
    }

    match show {
      Show::Recording { meter, device, seconds, no_model } => {
        (self.meter, self.device, self.no_model) = (meter, device, no_model);
        let seconds = std::time::Duration::from_secs_f64(seconds);
        self.state.recording(now.saturating_sub(seconds), now);
      }
      Show::Transcribing(progress) => self.state.transcribing(progress, now),
      Show::Error(reason) => self.state.error(&reason, f.measure(&reason, FONT), now),
      Show::Collapse => self.state.collapse(now),
    }
  }

  fn draw(&self, f: &mut Frame) {
    let t = &self.theme;
    let hud::Frame { width, alpha: pill_alpha, shapes } = self.state.frame(f.now);
    let fade = |c: Color, a: f32| alpha(c, c[3] * pill_alpha * a);
    let pill = Rect::new((f.size.w - width) / 2.0, hud::MARGIN, width, hud::HEIGHT);
    let shadow = Rect::new(pill.x, pill.y + 2.0, pill.w, pill.h);
    f.shadow(shadow, hud::RADIUS, 6.0, fade(t.shadow, 1.0));
    f.quad(Quad::new(pill, fade(t.menu, 1.0)).radius(hud::RADIUS).border(1.0, fade(t.line, 1.0)));
    f.layer();
    f.push_clip(pill);

    // What is in the pill keeps its place as the pill narrows: centred at the recording's width.
    let x = (f.size.w - width.max(hud::WIDTH)) / 2.0;
    let mid = pill.y + hud::HEIGHT / 2.0;
    let baseline = |f: &mut Frame, font| mid + (f.text.metrics(font).cap_height / 2.0).round();
    for shape in shapes {
      match shape {
        Shape::Dot { alpha } => {
          let dot = Rect::new(x + hud::PAD, mid - hud::DOT / 2.0, hud::DOT, hud::DOT);
          f.rect(dot, fade(t.danger, alpha), hud::DOT / 2.0);
        }
        Shape::Bar { x: at, height, alpha } => {
          let bar = Rect::new(x + at, mid - height / 2.0, hud::BAR_WIDTH, height);
          f.rect(bar, fade(t.text, alpha), hud::BAR_WIDTH / 2.0);
        }
        Shape::Time { text, alpha } => {
          let y = baseline(f, TIME);
          f.text(x + hud::TIME_X, y, &text, TIME, fade(t.text, alpha));
        }
        Shape::Warning { text, alpha } => {
          let glyph = Rect::new(x + hud::PAD, mid - hud::GLYPH / 2.0, hud::GLYPH, hud::GLYPH);
          icon::draw(f, TRIANGLE, glyph, fade(t.warning, alpha), 0.0);
          icon::draw(f, MARK, glyph, fade(t.menu, alpha), 2.2);
          let y = baseline(f, FONT);
          f.text(x + hud::REASON_X, y, &text, FONT, fade(t.text, alpha));
        }
      }
    }

    f.pop_clip();
  }
}

impl App for Overlay {
  fn event(&mut self, _cx: &mut Cx, ev: &Event) -> bool {
    let Event::Appearance(a) = ev else { return false };
    self.theme = Theme::system(*a);
    true
  }

  fn paint(&mut self, f: &mut Frame) {
    let shown = self.state.mode() != Mode::Hidden;
    for show in std::mem::take(&mut self.shows) {
      self.apply(show, f);
    }

    // No sound from the microphone first: it is the recording's own; then no model.
    let warning = match &self.meter {
      Some(m) if m.silent() => Some(format!("no sound from {}", self.device)),
      _ => self.no_model.clone(),
    };

    let warning = warning.map(|w| (f.measure(&w, FONT), w));
    self.state.warning(warning.as_ref().map(|(width, w)| (w.as_str(), *width)), f.now);
    self.state.tick(self.meter.as_ref().map_or(0.0, Meter::level), f.now);
    if self.state.mode() == Mode::Hidden {
      if shown {
        (self.hidden)();
      }

      return;
    }

    self.draw(f);
    f.animate();
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::rc::Rc;
  use std::time::Duration;

  use nimbo::{Appearance, Harness};

  use super::*;

  /// The overlay in the harness, transparent as an overlay's frame is, and how often it asked to
  /// be hidden.
  fn overlay() -> (Harness<Overlay>, Rc<Cell<u32>>) {
    let hidden = Rc::new(Cell::new(0));
    let count = hidden.clone();
    let config = config(HudPosition::Bottom);
    let mut h = Harness::new(Overlay::new(move || count.set(count.get() + 1)), config.size, 2.0);
    h.cpu.transparent = true;
    h.event(Event::Appearance(Appearance::DARK));
    (h, hidden)
  }

  fn recording(seconds: f64) -> Show {
    Show::Recording { meter: None, device: "Mic".into(), seconds, no_model: None }
  }

  #[test]
  fn an_overlay_at_the_bottom_or_top_shown_on_demand() {
    let shown = |p| {
      let c = config(p);
      (c.overlay, c.hidden, c.resizable)
    };
    assert_eq!(shown(HudPosition::Bottom), (Some(Anchor::Bottom(44.0)), true, false));
    assert_eq!(shown(HudPosition::Top), (Some(Anchor::Top(0.0)), true, false));
  }

  #[test]
  fn the_pill_records_in_the_middle_transparent_around_it() {
    let (mut h, _) = overlay();
    h.app_mut().show(recording(3.0));
    assert_eq!(h.app().mode(), Mode::Recording, "as soon as it is asked");
    h.wait(Duration::from_millis(500));
    assert_eq!(h.app().mode(), Mode::Recording);
    let img = h.render();
    let size = config(HudPosition::Bottom).size;
    for (x, y) in [(1.0, 1.0), (size.w - 1.0, 1.0), (1.0, size.h - 1.0)] {
      assert_eq!(img.pixel(x, y), [0; 4], "({x}, {y})");
    }

    let left = (size.w - hud::WIDTH) / 2.0;
    let mid = hud::MARGIN + hud::HEIGHT / 2.0;
    let [r, g, _, a] = img.pixel(left + hud::PAD + hud::DOT / 2.0, mid);
    assert!(r > 200 && g < 100 && a == 255, "the red dot: {r} {g} {a}");
    let [r, ..] = img.pixel(left + 2.0, hud::MARGIN + 1.0);
    assert_eq!(r, 0, "the pill's rounded corner");
    assert!(img.pixel(left + 30.0, mid)[3] == 255, "the pill");
    assert!(h.rt.needs_frame(), "it animates");
  }

  #[test]
  fn an_error_widens_the_pill_then_it_hides() {
    let (mut h, hidden) = overlay();
    h.app_mut().show(Show::Error("no model · open models".into()));
    h.wait(Duration::from_millis(300));
    assert_eq!(h.app().text().as_deref(), Some("warning: no model · open models"));
    let img = h.render();
    let mid = hud::MARGIN + hud::HEIGHT / 2.0;
    let left = (0..400).map(|x| x as f32).find(|&x| img.pixel(x, mid)[3] == 255).unwrap();
    let recording = (config(HudPosition::Bottom).size.w - hud::WIDTH) / 2.0;
    assert!(left < recording - 10.0, "wider than the recording's pill: {left}");
    let [r, g, b, _] = img.pixel(left + hud::PAD + hud::GLYPH / 2.0 - 4.0, mid + 5.0);
    assert!(r > 200 && g > 100 && b < 100, "the orange glyph: {r} {g} {b}");

    h.wait(Duration::from_secs(5));
    assert_eq!((h.app().mode(), hidden.get()), (Mode::Hidden, 1), "held, collapsed, hidden");
    assert!(!h.rt.needs_frame(), "no frames once hidden");
  }

  #[test]
  fn a_transcription_collapses_when_the_text_is_out() {
    let (mut h, hidden) = overlay();
    h.app_mut().show(recording(0.0));
    h.wait(Duration::from_millis(200));
    h.app_mut().show(Show::Transcribing(Some((0, 2))));
    h.wait(Duration::from_millis(200));
    assert_eq!((h.app().mode(), h.app().text().as_deref()), (Mode::Transcribing, Some("1/2")));
    h.app_mut().show(Show::Collapse);
    h.wait(Duration::from_millis(400));
    assert_eq!((h.app().mode(), hidden.get()), (Mode::Hidden, 1));
  }
}
