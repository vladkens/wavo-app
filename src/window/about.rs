// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The About page: the app's icon, name and version, one line, View on GitHub and Buy Me a
//! Coffee, then the acknowledgements in a plate, each opening its page with the large ↗ in its row.

use nimbo::{
  Align, Button, ButtonStyle, ControlSize, Font, FormRow, FormStyle, Icon, Style, TextStyle, Ui,
  Variant, grow, px,
};

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

/// The gap between a form row's label and its caption, as nimbo lays them out.
const LABEL_GAP: f32 = 2.0;
/// The acknowledgements' ↗, in points, and its mark: nimbo's `EXTERNAL` drawn larger in its box.
const LINK_ICON: f32 = 22.0;
const LINK_MARK: Icon = Icon::new("M6.5 17.5 17.5 6.5M8 6.5h9.5V16");

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

  /// The acknowledgements, a name and a line each, with a large ↗ centred in the row (not on the
  /// name's line, where a row's controls go).
  pub(super) fn about(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let mut open = None;
    let fs = &FormStyle { caption: fs.caption.ellipsis(), ..*fs };
    // The row's height under its hairline: the name, its line, the padding around them.
    let row_h = 2.0 * fs.pad_y + fs.label.line_h + LABEL_GAP + fs.caption.line_h;
    // A large square ghost button; its ↗ is as big as its text and 2 more: [`LINK_ICON`].
    let t = ui.theme();
    let s = t.button_sized(Variant::Ghost, ControlSize::Large);
    let side = s.frame.min[0];
    let font = Font { size: LINK_ICON - 2.0, ..s.text.font };
    let text = s.text.color(t.muted).font(font);
    let link = ButtonStyle { frame: s.frame.w(px(side)).pad(0.0), text, ..s };
    ui.form_section(fs, "Acknowledgements", |ui| {
      for (id, name, what, url) in ACKS {
        ui.form_row(fs, FormRow::new(name).caption(what), |ui| {
          ui.col(Style::new().h(px(row_h)).align(Align::Center, Align::Center), |ui| {
            let r = Button::icon(LINK_MARK).style(link).show(ui.id((LINK, id)));
            ui.tooltip(r.id, url.trim_start_matches("https://"));
            if r.clicked {
              open = Some(url);
            }
          });
        });
      }
    });
    if let Some(url) = open {
      self.shell.open_url(url);
    }
  }
}
