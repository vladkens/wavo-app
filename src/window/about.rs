// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The About page: the app's icon, name and version, one line, View on GitHub and Buy Me a
//! Coffee, then the acknowledgements in a plate, each opening its page.

use nimbo::{Align, Font, FormRow, FormStyle, Icon, Style, TextStyle, Ui, grow, px};

use super::Prefs;

/// The head's buttons and the acknowledgements: `(LINK, name)`.
pub const LINK: &str = "link";

const SOURCE: &str = "https://github.com/vladkens/wavo-app";
const COFFEE: &str = "https://buymeacoffee.com/vladkens";

/// `(id, name, what, url)`.
const ACKS: [(&str, &str, &str, &str); 6] = [
  ("wavo", "wavo", "Speech recognition on this machine", "https://github.com/vladkens/wavo"),
  ("nimbo", "nimbo", "The UI toolkit Wavo is drawn with", "https://github.com/vladkens/nimbo"),
  ("handy", "Handy", "The app that inspired Wavo", "https://github.com/cjpais/Handy"),
  (
    "transcribe",
    "transcribe.cpp",
    "The models’ GGUF conversions",
    "https://github.com/handy-computer/transcribe.cpp",
  ),
  (
    "ggml",
    "ggml",
    "The tensor library and model format, by Georgi Gerganov and others",
    "https://github.com/ggml-org/ggml",
  ),
  (
    "models",
    "The models",
    "OpenAI Whisper, NVIDIA Parakeet, Sber GigaAM, Useful Sensors Moonshine",
    "https://huggingface.co/handy-computer",
  ),
];

/// `assets/icon.png` at 128 px (64 points at 2x) as RGBA, from build.rs.
const ICON_PX: usize = 128;
static ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/about-icon.rgba"));

impl Prefs {
  /// The head above the page: the icon, the name, the version, a line and two buttons.
  pub(super) fn about_head(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let head =
      Style::new().w(grow()).pads([22.0, 20.0, 0.0, 20.0]).align(Align::Center, Align::Start);
    let mut open = None;
    ui.col(head, |ui| {
      ui.rgba_image(ICON_PX, ICON_PX, ICON, Style::new().w(px(64.0)).h(px(64.0)));
      ui.el(Style::new().h(px(10.0)));
      ui.text("Wavo", TextStyle::new(Font { size: 20.0, ..t.bold }, t.text).ellipsis());
      ui.el(Style::new().h(px(2.0)));
      ui.text(concat!("Version ", env!("CARGO_PKG_VERSION")), fs.note);
      ui.el(Style::new().h(px(8.0)));
      ui.text("Local dictation: hold a key, speak, and the text lands where you type.", t.label());
      ui.el(Style::new().h(px(14.0)));
      ui.row(Style::new().gap(8.0), |ui| {
        if ui.id((LINK, "source")).button("View on GitHub").clicked {
          open = Some(SOURCE);
        }

        if ui.id((LINK, "coffee")).button("Buy Me a Coffee").clicked {
          open = Some(COFFEE);
        }
      });
    });
    if let Some(url) = open {
      self.shell.open_url(url);
    }
  }

  pub(super) fn about(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let mut open = None;
    ui.form_section(fs, "Acknowledgements", |ui| {
      for (id, name, what, url) in ACKS {
        ui.form_row(fs, FormRow::new(name).caption(what), |ui| {
          let r = ui.id((LINK, id)).icon_button(Icon::EXTERNAL);
          ui.tooltip(r.id, url.trim_start_matches("https://"));
          if r.clicked {
            open = Some(url);
          }
        });
      }
    });
    if let Some(url) = open {
      self.shell.open_url(url);
    }
  }
}
