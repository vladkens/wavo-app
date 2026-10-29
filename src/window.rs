// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The settings window, as macOS System Settings looks: toolbar tabs over pages of grey plates.
//! General, Advanced and About are as high as their content; Models and History scroll under
//! their search row. The window shows a copy of the settings and the session's status, which the
//! shell keeps current ([Prefs::update]), and reaches the app only through [Shell]: tests run it
//! in nimbo's harness with a fake one.

use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use nimbo::autostart::State as Autostart;
use nimbo::permissions::{Access as Grant, Pane};
use nimbo::update::Release;
use nimbo::{
  Align, Answer, App, Button, Config, Cx, Dialog, Event, FileDialog, Float, Font, FormRow,
  FormStyle, Hotkey, Icon, Id, Key, NamedKey, PopUp, Select, Style, TextStyle, Tile, Ui, Weight,
  grow, open, paths, px,
};
use tracing::{info, warn};
use wavo::hub::{self, Hub, ModelInfo};

use crate::audio::{self, Auto, Choice, Microphone, Playback, Transport};
use crate::history::{Entry, History, Local, RECORDINGS_DIR, Retention, fold, local_time, now_ms};
use crate::session::{NoModel, Request, Status, label};
use crate::settings::{Activation, HudPosition, Settings, language_name};
use crate::transfer::Imported;
use crate::updater::{self, Act as UpdateAct, State};

// MARK: Window

pub const WIDTH: f32 = 560.0;
/// The model to start with, wavo's own default: first in Available, its Download the blue one.
const RECOMMENDED: &str = "parakeet-ultra";
/// The height of the pages that scroll.
pub const SCROLLING: f32 = 600.0;

const TABS: &str = "tabs";
const CONFIRM: &str = "confirm";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
  General,
  Models,
  History,
  Advanced,
  About,
}

const PAGES: [(Page, Icon, &str); 5] = [
  (Page::General, Icon::GEAR, "General"),
  (Page::Models, Icon::DOWNLOAD, "Models"),
  (Page::History, Icon::HISTORY, "History"),
  (Page::Advanced, Icon::SLIDERS, "Advanced"),
  (Page::About, Icon::INFO, "About"),
];

impl Page {
  fn scrolls(self) -> bool {
    matches!(self, Self::Models | Self::History)
  }
}

/// A macOS preferences window: the title over the tabs, under one background.
pub fn config(page: Page) -> Config {
  let h = if page.scrolls() { SCROLLING } else { 500.0 };
  crate::with_icon(Config::new("Wavo", WIDTH, h).fixed().unified())
}

/// What the system allows the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
  pub accessibility: bool,
  pub microphone: Grant,
  /// Another app holds Secure Input: the hotkey goes through its backup path.
  pub secure_input: bool,
  /// Paste from Other Apps is not allowed: the clipboard can't be put back after a paste.
  pub clipboard_unread: bool,
}

/// What the window asks of the app. What takes a while runs off the main thread and comes back
/// through the `Prefs` method named.
pub trait Shell {
  fn edit(&self, _edit: Box<dyn FnOnce(&mut Settings) + Send>) {}
  /// The shortcut recorder records: the current hotkey is not matched meanwhile.
  fn pause_hotkey(&self, _paused: bool) {}
  fn devices(&self) -> Vec<audio::Device>;
  /// [audio::lid_closed], for Automatic's microphone.
  fn lid_closed(&self) -> bool {
    false
  }

  fn access(&self) -> Access;
  /// The system's prompt while it can still ask ([Prefs::access_changed]), else its pane.
  fn allow(&self, _pane: Pane) {}
  /// A download that ends calls [Prefs::models_changed].
  fn download(&self, _model: &'static ModelInfo) {}
  fn cancel_download(&self, _name: &str) {}
  fn download_state(&self, name: &str) -> Option<Download>;
  /// Then [Prefs::models_changed].
  fn delete_model(&self, _model: &'static ModelInfo) {}
  fn history(&self, _req: Request) {}
  fn autostart(&self) -> Result<Autostart, String>;
  /// Then [Prefs::autostart_set].
  fn set_autostart(&self, _on: bool) {}
  /// Writes the history into the archive `file`, replacing it (the save panel asked):
  /// [Shell::export_state].
  fn export(&self, _file: PathBuf) {}
  fn export_state(&self) -> Option<Export>;
  /// Adds the dictations of the archive `file` the history does not have: [Shell::import_state].
  fn import(&self, _file: PathBuf) {}
  fn import_state(&self) -> Option<Import>;
  fn open_url(&self, _url: &str) {}
  fn reveal(&self, _dir: &Path) {}
  /// This Mac has a Neural Engine wavo can use.
  fn has_neural_engine(&self) -> bool {
    false
  }
  /// What the updater does; `None` where the app can't update (nimbo's `update::supported`, a dev
  /// build, a run with its own `WAVO_HOME`).
  fn update_state(&self) -> Option<State> {
    None
  }
  /// The window is redrawn as the state changes.
  fn update_act(&self, _act: UpdateAct) {}
}

enum Delete {
  Model(&'static ModelInfo),
  Entry(i64),
}

/// A delete the dialog asks about.
struct Ask {
  title: String,
  message: String,
  delete: Delete,
}

struct Login {
  /// `Err` with the reason when unavailable.
  state: Result<Autostart, String>,
  /// Being set, to on or off: the switch shows it and waits.
  busy: Option<bool>,
  error: Option<String>,
}

pub struct Prefs {
  pub page: Page,
  settings: Settings,
  status: Status,
  shell: Box<dyn Shell>,
  hub: Hub,
  home: PathBuf,
  devices: Vec<audio::Device>,
  /// A laptop's lid is closed: its built-in microphone hears nothing ([audio::automatic]).
  lid_closed: bool,
  found: Vec<Found>,
  pick: Pick,
  access: Access,
  login: Login,
  /// The shortcut recorder records: the hotkey is paused.
  recording: bool,
  focused: bool,
  ask: Option<Ask>,
  /// The file panel open: its answer goes to it.
  panel: Option<Panel>,
  model_search: String,
  /// An index of [LANGUAGES].
  model_lang: usize,
  history: HistoryPage,
}

impl Prefs {
  /// `history`: the app's connection, `None` when it could not open.
  pub fn new(
    shell: Box<dyn Shell>,
    settings: Settings,
    hub: Hub,
    home: &Path,
    history: Option<Rc<History>>,
  ) -> Self {
    let access = shell.access();
    let mut prefs = Self {
      page: Page::General,
      settings,
      status: Status::default(),
      shell,
      hub,
      home: home.to_owned(),
      devices: Vec::new(),
      lid_closed: false,
      found: Vec::new(),
      pick: Pick::default(),
      access,
      login: Login { state: Err(String::new()), busy: None, error: None },
      recording: false,
      focused: false,
      ask: None,
      panel: None,
      model_search: String::new(),
      model_lang: 0,
      history: HistoryPage { db: history, ..HistoryPage::default() },
    };
    prefs.refresh();
    prefs
  }

  pub fn update(&mut self, settings: &Settings, status: &Status, history: u64) {
    let model = std::mem::replace(&mut self.settings, settings.clone()).model;
    if model != self.settings.model {
      self.repick();
    }

    self.status = status.clone();
    self.history.revision = history;
  }

  /// Looks again at what the session does not push: when the window opens or comes to the front.
  pub fn refresh(&mut self) {
    self.devices = self.shell.devices();
    self.lid_closed = self.shell.lid_closed();
    self.access = self.shell.access();
    self.login.state = self.shell.autostart();
    self.models_changed();
  }

  pub fn access_changed(&mut self) {
    self.access = self.shell.access();
  }

  pub fn models_changed(&mut self) {
    self.found = ready(self.hub.downloaded(), |name| self.shell.download_state(name));
    self.repick();
  }

  fn repick(&mut self) {
    self.pick = Pick::new(&self.hub, self.settings.model.as_deref(), &self.found);
  }

  /// Changes the settings here at once and in the session, which saves them.
  fn edit(&mut self, edit: impl Fn(&mut Settings) + Send + 'static) {
    edit(&mut self.settings);
    self.repick();
    self.shell.edit(Box::new(move |s| edit(s)));
  }

  fn active(&self) -> Option<&Found> {
    self.found.get(self.pick.active?)
  }

  /// Some model transcribes: the chosen one, or the first downloaded in its place.
  fn transcribes(&self) -> bool {
    self.pick.active.is_some() || self.pick.other.as_ref().is_some_and(|o| o.1)
  }

  /// Why the model does not transcribe as chosen.
  fn model_note(&self) -> Option<String> {
    if let Some(chosen) = self.pick.gone() {
      return Some(match self.active() {
        Some(f) => format!("{chosen} is not on disk: {} transcribes instead.", f.0.full_name),
        None => format!("{chosen} is not on disk: download it again in Models, or pick another."),
      });
    }

    match &self.status.no_model {
      Some(e @ NoModel::LoadFailed { .. }) => Some(format!("{e}.")),
      _ => None,
    }
  }

  fn confirm(&mut self, ui: &mut Ui) {
    let Some(ask) = &self.ask else { return };
    let mut open = true;
    let d = Dialog::new(&ask.title).message(&ask.message).destructive("Delete");
    let answer = d.show(ui.id(CONFIRM), &mut open);
    if open {
      return;
    }

    let ask = self.ask.take().unwrap();
    if !matches!(answer, Some(Answer::Action)) {
      return;
    }

    match ask.delete {
      Delete::Model(m) => {
        info!("settings: delete model {}", m.name);
        self.shell.delete_model(m);
      }
      Delete::Entry(id) => self.shell.history(Request::Delete(id)),
    }
  }
}

impl App for Prefs {
  fn view(&mut self, ui: &mut Ui) {
    // Back from another app (System Settings, where a permission was given).
    let focused = ui.window_focused();
    if focused && !self.focused {
      self.refresh();
    }

    self.focused = focused;
    // A recording played to its end lets the speakers go, whatever page shows.
    let h = &mut self.history;
    if let Some((_, p)) = h.playing.take_if(|(_, p)| !p.playing())
      && let Some(e) = p.error()
    {
      h.note = Some((format!("Could not play the recording: {e}"), ui.now()));
    }

    // The title bar is part of the window's content.
    let bar = ui.titlebar().map_or(0.0, |(h, _)| h);
    match self.page.scrolls() {
      true => ui.set_window_size(WIDTH, SCROLLING + bar),
      false => ui.fit_window_height(WIDTH, 200.0, 800.0),
    }

    let fs = FormStyle::plates(&ui.theme());
    ui.col(Style::new().grow().bg(fs.bg), |ui| {
      ui.id(TABS).toolbar_tabs(&fs, &mut self.page, &PAGES);
      let head = Style::new().w(grow()).pads([12.0, 20.0, 2.0, 20.0]).gap(10.0);
      let head = head.align(Align::Start, Align::Center);
      match self.page {
        Page::Models => self.models_head(ui, head),
        Page::History => self.history_head(ui, head),
        _ => {}
      }

      ui.id(("page", self.page as u8)).form_scroll(&fs, Style::new().grow(), |ui| {
        ui.col(fs.page.pads([12.0, 20.0, 20.0, 20.0]), |ui| match self.page {
          Page::General => self.general(ui, &fs),
          Page::Models => self.models(ui, &fs),
          Page::History => self.history(ui, &fs),
          Page::Advanced => self.advanced(ui, &fs),
          Page::About => self.about(ui, &fs),
        });
      });

      if self.page == Page::History {
        self.history_foot(ui, &fs);
        self.history_total(ui, &fs);
      }
    });
    self.confirm(ui);

    // Also when the recorder lost its focus with the page, or the window lost the keys.
    let recording = ui.focused(Id::of("hotkey")) && focused;
    if recording != self.recording {
      self.recording = recording;
      self.shell.pause_hotkey(recording);
    }
  }

  /// The file Export's or Import's panel picked; Esc, or a click on nothing above the panel
  /// under the history, unselects its dictation.
  fn event(&mut self, cx: &mut Cx, ev: &Event) -> bool {
    let history = self.page == Page::History;
    match ev {
      Event::FileDialog(answer) => {
        match (self.panel.take(), answer) {
          // GNOME's save panel adds no extension: Import's *.zip would not show it.
          (Some(Panel::Export), Some(file)) => self.shell.export(with_zip(file)),
          (Some(Panel::Import), Some(file)) => self.shell.import(file.clone()),
          _ => {}
        }

        true
      }
      Event::KeyDown(k) if history && k.key == Key::Named(NamedKey::Escape) => {
        self.history.selected.take().is_some()
      }
      Event::MouseDown { pos, .. } if history && cx.target().is_none() => {
        let above = pos.y < cx.size.h - FOOT_H - TOTAL_H;
        above && self.history.selected.take().is_some()
      }
      _ => false,
    }
  }
}

// The microphone's pop-up at its widest, in points.
const MIC_WIDTH: f32 = 320.0;

/// A pop-up of `labels`, item `at` the current one: the item picked, when it is another.
fn pick(ui: &mut Ui, id: &str, labels: &[String], at: usize, lines: &[usize]) -> Option<usize> {
  let items: Vec<(usize, &str)> = labels.iter().map(String::as_str).enumerate().collect();
  let mut cur = at;
  Select::new(&mut cur, &items).separators(lines).show(ui.id(id)).changed.then_some(cur)
}

// MARK: General

const FN_NOTE: &str =
  "Set System Settings › Keyboard › “Press 🌐 key to” to “Do Nothing”, or macOS acts on it too.";
const SECURE_INPUT_NOTE: &str = "Secure Input is on (a password field, or a terminal’s Secure \
  Keyboard Entry): the shortcut works through a backup path, and Esc may not cancel.";
const ACTIVATION_HELP: &str = "How the hotkey records: while you hold it, from one press to the \
  next, or both (a short tap latches the recording on).";
// Where the person turns launch at login back on, which the system turned off.
const LOGIN_APPROVAL: &str = if cfg!(windows) {
  "Turn Wavo on in Task Manager › Startup apps."
} else {
  "Allow Wavo in System Settings › General › Login Items."
};
const KEEP_HELP: &str = "Off: after the paste the clipboard gets back what it held. On: the \
  dictated text stays on it.";
const CLIPBOARD_NOTE: &str = "Wavo can’t put the clipboard back after a paste: allow it in System \
  Settings › Privacy & Security › Paste from Other Apps.";
const MICROPHONE_WHY: &str = "Wavo records your voice through it while you hold the shortcut; \
  the recording stays on this Mac.";
const ACCESSIBILITY_WHY: &str = "Wavo hears the shortcut in every app and pastes the text where \
  you type through it.";

/// A triangle with "!" in it, for a permission missing.
const WARNING: Icon = Icon::new("M12 4.5 3 19.5h18ZM12 10v4.5M12 17v.2");

const ACTIVATIONS: [(Activation, &str); 3] = [
  (Activation::HoldOrToggle, "Hold to talk, or tap to start and stop"),
  (Activation::PushToTalk, "Hold to talk"),
  (Activation::Toggle, "Tap to start and stop"),
];

impl Prefs {
  fn general(&mut self, ui: &mut Ui, fs: &FormStyle) {
    self.permissions(ui, fs);
    // By what matters most: the microphone, the model and its language, then the shortcut.
    ui.form_section(fs, "Dictation", |ui| {
      let (devices, saved) = (&self.devices, &self.settings.microphone);
      let (mics, at) = microphones(devices, saved, self.lid_closed);
      let labels: Vec<&str> = mics.iter().map(|m| m.0.as_str()).collect();
      let mut row = FormRow::new("Microphone");
      if let Some(note) = bluetooth_only(devices, saved, self.lid_closed) {
        row = row.caption(note);
      }

      ui.form_row(fs, row, |ui| {
        // ALSA's names run past 60 characters: a longer one ends in "…" beside its label.
        let mut cur = at;
        let pop = PopUp::new(&mut cur, &labels).separators(&[1]).max_width(MIC_WIDTH);
        if pop.show(ui.id("mic")).changed {
          let choice = mics[cur].1.clone();
          self.edit(move |s| s.microphone = choice.clone());
        }
      });

      self.model_rows(ui, fs);
      let note = match (self.access.secure_input, self.settings.hotkey == "fn") {
        (true, _) => Some(SECURE_INPUT_NOTE),
        (false, true) => Some(FN_NOTE),
        (false, false) => None,
      };
      let mut row = FormRow::new("Shortcut");
      if let Some(note) = note {
        row = row.info(note);
      }

      ui.form_row(fs, row, |ui| self.shortcut(ui));
      ui.form_row(fs, FormRow::new("Activation").info(ACTIVATION_HELP), |ui| {
        let mut cur = self.settings.activation;
        if ui.id("activation").select(&mut cur, &ACTIVATIONS).changed {
          self.edit(move |s| s.activation = cur);
        }
      });
    });

    ui.form_section(fs, "Output", |ui| {
      ui.form_row(fs, FormRow::new("Paste the text into the app you dictate in"), |ui| {
        let mut on = self.settings.auto_paste;
        if ui.id("paste").toggle(&mut on, "").changed {
          self.edit(move |s| s.auto_paste = on);
        }
      });

      // Without pasting the text only goes to the clipboard, and stays there.
      let help = match self.access.clipboard_unread && self.settings.restores_clipboard() {
        true => format!("{KEEP_HELP}\n\n{CLIPBOARD_NOTE}"),
        false => KEEP_HELP.to_owned(),
      };
      let row = FormRow::new("Keep the text on the clipboard").info(&help);
      let row = row.disabled(!self.settings.auto_paste);
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.keep_in_clipboard;
        if ui.id("keep").toggle(&mut on, "").changed {
          self.edit(move |s| s.keep_in_clipboard = on);
        }
      });
    });

    ui.form_section(fs, "Startup", |ui| self.login(ui, fs));
  }

  /// While the system does not allow them, a row for the microphone and one for Accessibility.
  fn permissions(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let (mic, ax) = (self.access.microphone != Grant::Granted, !self.access.accessibility);
    let title = match (mic, ax) {
      (false, false) => return,
      (true, true) => "Wavo needs two permissions",
      _ => "Wavo needs a permission",
    };

    // A denied microphone is not asked again: only System Settings changes it.
    let ask = self.access.microphone == Grant::NotDetermined;
    let rows = [
      (mic, "Microphone", MICROPHONE_WHY, "open-microphone", Pane::Microphone, ask),
      (ax, "Accessibility", ACCESSIBILITY_WHY, "open-accessibility", Pane::Accessibility, true),
    ];
    let fs = &FormStyle { tile_icon: ui.theme().warning, ..*fs };
    ui.form_section(fs, title, |ui| {
      for (_, name, why, id, pane, ask) in rows.into_iter().filter(|r| r.0) {
        let row = FormRow::new(name).info(why).tile(Tile::Icon(WARNING));
        ui.form_row(fs, row, |ui| {
          let button =
            if ask { Button::new("Allow").primary() } else { Button::new("Open Settings") };
          if button.show(ui.id(id)).clicked {
            self.shell.allow(pane);
          }
        });
      }
    });
  }

  /// The recorder and its reset; a cleared value keeps the hotkey: the app needs one.
  fn shortcut(&mut self, ui: &mut Ui) {
    let mut hotkey = Hotkey::parse(&self.settings.hotkey).ok();
    // Where hotkeys are chords only (Linux), a modifier key alone would leave no hotkey at all.
    let alone = |h: &Hotkey| !nimbo::hotkey::modifier_alone() && matches!(h, Hotkey::Modifier(_));
    if ui.id("hotkey").shortcut(&mut hotkey).changed
      && let Some(hotkey) = hotkey.filter(|h| !alone(h))
    {
      let hotkey = hotkey.to_string();
      info!("settings: hotkey {hotkey}");
      self.edit(move |s| s.hotkey = hotkey.clone());
    }

    let default = Settings::default().hotkey;
    ui.disabled(Hotkey::parse(&self.settings.hotkey).ok() == Hotkey::parse(&default).ok());
    let r = ui.id("reset").icon_button(Icon::RESET);
    ui.tooltip(r.id, "Back to the default shortcut");
    if r.clicked {
      self.edit(move |s| s.hotkey = default.clone());
    }
  }

  /// The downloaded models, the chosen one when it is none of them, then "Download more…".
  fn model_choices(&self) -> (Vec<String>, usize) {
    let mut items: Vec<String> = self.found.iter().map(|f| f.0.full_name.to_owned()).collect();
    let mut at = self.pick.active.unwrap_or_default();
    let other = match &self.pick.other {
      Some((name, true)) => Some(name.clone()),
      Some((name, false)) => Some(format!("{name} (not on disk)")),
      None => None,
    };
    if let Some(other) = other {
      items.push(other);
      at = items.len() - 1;
    }

    items.push("Download more…".into());
    (items, at)
  }

  /// The language choices: auto, then the languages the model in use is meant for.
  fn languages(&self) -> (Vec<(String, String)>, usize) {
    let codes = self.active().map_or(&[][..], |f| f.0.langs);
    let mut langs: Vec<(String, String)> =
      codes.iter().map(|c| (language_name(c), (*c).to_owned())).collect();
    langs.sort();
    langs.insert(0, ("Auto-detect".to_owned(), "auto".to_owned()));
    let Some(current) = self.settings.engine_language() else { return (langs, 0) };

    if let Some(i) = langs.iter().position(|(_, c)| c.eq_ignore_ascii_case(&current)) {
      return (langs, i);
    }

    langs.push((format!("{} (not one the model is meant for)", language_name(&current)), current));
    let at = langs.len() - 1;
    (langs, at)
  }

  /// The model and language rows; with no model downloaded, the way to download one.
  fn model_rows(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let (labels, at) = self.model_choices();
    let note = self.model_note();
    let mut row = FormRow::new("Model");
    if let Some(note) = &note {
      row = row.info(note);
    }

    let none = self.found.is_empty() && self.pick.other.is_none();
    let mut picked = None;
    ui.form_row(fs, row, |ui| {
      if !none {
        picked = pick(ui, "model", &labels, at, &[labels.len() - 2]);
      } else if Button::new("Download a Model…").primary().show(ui.id("download-model")).clicked {
        self.page = Page::Models;
      }
    });

    match picked {
      Some(i) if i + 1 == labels.len() => self.page = Page::Models,
      Some(i) if i < self.found.len() => self.use_model(self.found[i].0.name),
      _ => {}
    }

    let (langs, at) = self.languages();
    let labels: Vec<String> = langs.iter().map(|l| l.0.clone()).collect();
    ui.form_row(fs, FormRow::new("Language"), |ui| {
      if let Some(i) = pick(ui, "lang", &labels, at, &[0]) {
        let code = langs[i].1.clone();
        self.edit(move |s| s.language = code.clone());
      }
    });
  }
}

// MARK: Advanced

const TOOK_HELP: &str = "After each dictation the HUD shows for a second how long the text took after \
  the stop.";
const UNLOAD_HELP: &str = "Frees the model's memory after Wavo has been idle this long. The next \
  dictation loads it again, which takes a moment.";
const ANE_HELP: &str = "Parakeet and GigaAM run on the Neural Engine: as fast as on the GPU at a \
  fraction of the energy. Each new version of Wavo prepares the model once, in the background \
  (10–30 s); meanwhile it runs on the GPU. Whisper always runs on the GPU.";
const IMPORT_HELP: &str = "A .zip that Wavo exported, here or on another computer: the dictations this \
  history does not have are added with their recordings; the settings stay as they are.";
const EXPORT_HELP: &str = "The history, its recordings and the settings, into a .zip archive: \
  Import brings it back, here or on another computer.";
const UPDATE_HELP: &str = "Wavo looks for a new version at launch and once a day and offers it in \
  the menu: nothing installs until you choose Install and Relaunch.";

const HUDS: [(HudPosition, &str); 2] =
  [(HudPosition::Bottom, "Bottom of the screen"), (HudPosition::Top, "Top of the screen")];

const UNLOADS: [(Option<u32>, &str); 7] = [
  (None, "Never"),
  (Some(0), "Right after each dictation"),
  (Some(2), "After 2 minutes"),
  (Some(5), "After 5 minutes"),
  (Some(10), "After 10 minutes"),
  (Some(15), "After 15 minutes"),
  (Some(60), "After 60 minutes"),
];

const RETENTIONS: [(Retention, &str); 7] = [
  (Retention::Unlimited, "Forever"),
  (Retention::Count(100), "The last 100 dictations"),
  (Retention::Count(1000), "The last 1,000 dictations"),
  (Retention::Count(10_000), "The last 10,000 dictations"),
  (Retention::Days3, "3 days"),
  (Retention::Weeks2, "2 weeks"),
  (Retention::Months3, "3 months"),
];

impl Prefs {
  fn advanced(&mut self, ui: &mut Ui, fs: &FormStyle) {
    ui.form_section(fs, "App", |ui| {
      ui.form_row(fs, FormRow::new("Show the HUD at"), |ui| {
        let mut cur = self.settings.hud_position;
        if ui.id("hud").select(&mut cur, &HUDS).changed {
          self.edit(move |s| s.hud_position = cur);
        }
      });

      ui.form_row(fs, FormRow::new("Show the transcription time").info(TOOK_HELP), |ui| {
        let mut on = self.settings.show_transcription_time;
        if ui.id("took").toggle(&mut on, "").changed {
          self.edit(move |s| s.show_transcription_time = on);
        }
      });

      ui.form_row(fs, FormRow::new("Unload the model").info(UNLOAD_HELP), |ui| {
        let mut cur = self.settings.unload_after_minutes;
        if Select::new(&mut cur, &UNLOADS).separators(&[0]).show(ui.id("unload")).changed {
          self.edit(move |s| s.unload_after_minutes = cur);
        }
      });

      // Only where there is one: Apple silicon with macOS 15 or later.
      if self.shell.has_neural_engine() {
        ui.form_row(fs, FormRow::new("Use the Neural Engine").info(ANE_HELP), |ui| {
          let mut on = self.settings.neural_engine;
          if ui.id("ane").toggle(&mut on, "").changed {
            self.edit(move |s| s.neural_engine = on);
          }
        });
      }

      ui.form_row(fs, FormRow::new("Keep the history"), |ui| {
        let mut cur = self.settings.history_retention;
        if ui.id("retention").select(&mut cur, &RETENTIONS).changed {
          self.edit(move |s| s.history_retention = cur);
        }
      });

      // Only where the app can update.
      if self.shell.update_state().is_some() {
        ui.form_row(fs, FormRow::new("Automatic updates").info(UPDATE_HELP), |ui| {
          let mut on = self.settings.auto_update;
          if ui.id("auto-update").toggle(&mut on, "").changed {
            self.edit(move |s| s.auto_update = on);
          }
        });
      }
    });

    let models = self.hub.root().to_owned();
    let recordings = self.home.join(RECORDINGS_DIR);
    let folders = [
      (
        "Model folder",
        &models,
        "The downloaded models, in the Hugging Face cache, which other apps \
        that use Hugging Face models share. Deleting a model on Models frees its space.",
      ),
      (
        "Recordings",
        &recordings,
        "The audio of every dictation in the history, a folder a day. \
        A dictation deleted from the history takes its recording with it.",
      ),
    ];
    let show = format!("Show in {}", open::file_manager());
    ui.form_section(fs, "Files", |ui| {
      for (name, dir, what) in folders {
        let help = format!("{what}\n\n{}", paths::display(dir));
        let row = FormRow::new(name).info(&help).tile(Tile::Icon(Icon::FOLDER));
        ui.form_row(fs, row, |ui| {
          let r = ui.disabled(!dir.is_dir()).id(("reveal", name)).button(&show);
          ui.tooltip(r.id, &paths::display(dir));
          if r.clicked {
            self.shell.reveal(dir);
          }
        });
      }
    });

    ui.form_section(fs, "Data", |ui| {
      self.export_row(ui, fs, &show);
      self.import_row(ui, fs);
    });
  }

  /// Export, the system's save panel in Downloads ([Prefs::event]); then the archive's size with
  /// Show in Finder.
  fn export_row(&mut self, ui: &mut Ui, fs: &FormStyle, show: &str) {
    let state = self.shell.export_state();
    let ended = |(_, size): &(PathBuf, u64)| format!("Exported · {}", size_text(*size));
    let shown = shown(&state, "export", "Copying the history…", ended);
    let running = shown.progress.is_some();
    let row = FormRow::new("Export history").info(EXPORT_HELP).tile(Tile::Icon(Icon::DOC));
    data_row(ui, fs, Id::of("export-row"), row, &shown, |ui| {
      if let Some(Transfer::Done((file, _))) = &state
        && ui.disabled(!file.is_file()).id("export-show").button(show).clicked
      {
        self.shell.reveal(file);
      }

      if ui.disabled(running).id("export").button("Export…").clicked {
        let name = export_name(now_ms());
        let panel = FileDialog::save("Export").name(&name).types(&["zip"]);
        ui.file_dialog(FileDialog { dir: nimbo::paths::downloads(), ..panel });
        self.panel = Some(Panel::Export);
      }
    });
  }

  /// Import…, the system's open panel in Downloads ([Prefs::event]); then what it added.
  fn import_row(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let state = self.shell.import_state();
    let shown = shown(&state, "import", "Reading the archive…", imported_text);
    let running = shown.progress.is_some();
    let row = FormRow::new("Import history").info(IMPORT_HELP).tile(Tile::Icon(Icon::DOWNLOAD));
    data_row(ui, fs, Id::of("import-row"), row, &shown, |ui| {
      if ui.disabled(running).id("import").button("Import…").clicked {
        let panel = FileDialog::open("Import").types(&["zip"]);
        ui.file_dialog(FileDialog { dir: nimbo::paths::downloads(), ..panel });
        self.panel = Some(Panel::Import);
      }
    });
  }

  /// The switch shows the change at once and stays disabled until the system answers
  /// ([Prefs::autostart_set]).
  fn login(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let login = &self.login;
    let note = match (&login.error, &login.state) {
      (Some(e), _) => Some(e.clone()),
      (None, Ok(Autostart::RequiresApproval)) if login.busy.is_none() => {
        Some(LOGIN_APPROVAL.to_owned())
      }
      (None, Err(why)) if !why.is_empty() => Some(why.clone()),
      _ => None,
    };
    let state = login.state.as_ref().map_or(self.settings.autostart, |s| s.wanted());
    let mut on = login.busy.unwrap_or(state);
    let mut row =
      FormRow::new("Launch at login").disabled(login.state.is_err() || login.busy.is_some());
    if let Some(note) = &note {
      row = row.info(note);
    }

    ui.form_row(fs, row, |ui| {
      if ui.id("login").toggle(&mut on, "").changed {
        self.login.busy = Some(on);
        self.login.error = None;
        self.shell.set_autostart(on);
      }
    });
  }

  /// [Shell::set_autostart] to `on` ended: the system's state after it, or why not.
  /// A history request the session refused: said under the panel.
  pub fn history_failed(&mut self, req: Request, error: &str) {
    let what = match req {
      Request::Delete(_) => "Could not delete the dictation",
      Request::Retranscribe(_) => "Could not transcribe it again",
      Request::Restore(_) => "Could not put the previous text back",
      Request::Unload | Request::Imported => return,
    };
    self.history.said = Some(format!("{what}: {error}"));
  }

  pub fn autostart_set(&mut self, on: bool, result: Result<Autostart, String>) {
    self.login.busy = None;
    match result {
      Ok(state) => {
        self.login.state = Ok(state);
        self.settings.autostart = on;
      }
      Err(e) => {
        let what = if on { "on" } else { "off" };
        warn!("launch at login {what}: {e}");
        self.login.error = Some(format!("Couldn't turn it {what}: {e}"));
      }
    }
  }
}

// MARK: Models

/// A downloaded model: wavo's, and its file.
pub type Found = (&'static ModelInfo, PathBuf);

/// Automatic with the device it picks, the system default, the `devices`, and the `saved` one when
/// it is not connected; the index of the current one. Here and in the menu.
pub fn microphones(
  devices: &[audio::Device],
  saved: &Microphone,
  lid_closed: bool,
) -> (Vec<(String, Microphone)>, usize) {
  let automatic = match audio::automatic(devices, lid_closed) {
    Auto::Device(i) => format!("Automatic ({})", devices[i].name),
    Auto::System => "Automatic".to_owned(),
    Auto::Nothing => "Automatic (none: the lid is closed)".to_owned(),
  };
  let default = match devices.iter().find(|d| d.default) {
    Some(d) => format!("System Default ({})", d.name),
    None => "System Default".to_owned(),
  };
  let mut items = vec![(automatic, Microphone::Automatic), (default, Microphone::System)];
  for d in devices {
    let choice = Choice { id: d.id.clone(), name: d.name.clone() };
    items.push((d.name.clone(), Microphone::Device(choice)));
  }

  let saved = match saved {
    Microphone::Automatic => return (items, 0),
    Microphone::System => return (items, 1),
    Microphone::Device(choice) => choice,
  };
  let by_id = devices.iter().position(|d| saved.id.is_some() && d.id == saved.id);
  if let Some(i) = by_id.or_else(|| devices.iter().position(|d| d.name == saved.name)) {
    return (items, i + 2);
  }

  items.push((format!("{} (not connected)", saved.name), Microphone::Device(saved.clone())));
  let at = items.len() - 1;
  (items, at)
}

/// Under Automatic when the device it picks is a Bluetooth one: there is no other.
pub fn bluetooth_only(
  devices: &[audio::Device],
  saved: &Microphone,
  lid_closed: bool,
) -> Option<&'static str> {
  let auto = (*saved == Microphone::Automatic).then(|| audio::automatic(devices, lid_closed))?;
  match auto {
    Auto::Device(i) if devices[i].transport == Transport::Bluetooth => {
      Some("Only a Bluetooth mic: the sound is worse")
    }
    _ => None,
  }
}

/// `740 MB`, `16.0 GB`; `48 KB` under a megabyte (an empty history's archive).
pub fn size_text(bytes: u64) -> String {
  let mb = bytes as f64 / 1e6;
  match mb {
    _ if mb >= 1000.0 => format!("{:.1} GB", mb / 1000.0),
    _ if mb >= 1.0 => format!("{mb:.0} MB"),
    _ => format!("{} KB", bytes.div_ceil(1000)),
  }
}

/// The share done and `42% · 114 of 272 MB`.
fn share(done: u64, total: u64) -> (f32, String) {
  let k = done as f32 / total.max(1) as f32;
  (k, format!("{:.0}% · {}", k * 100.0, done_text(done, total)))
}

/// A progress bar, moving without a share, and its text, as high as a caption's line.
fn progress_line(ui: &mut Ui, fs: &FormStyle, k: Option<f32>, text: &str) {
  let line = Style::new().w(grow()).h(px(fs.caption.line_h)).gap(8.0);
  ui.row(line.align(Align::Start, Align::Center), |ui| {
    ui.col(Style::new().w(px(120.0)), |ui| ui.progress(k));
    ui.text(text, fs.caption.nowrap());
  });
}

/// `86,286`.
fn thousands(n: usize) -> String {
  let digits = n.to_string();
  let mut s = String::new();
  for (i, c) in digits.chars().enumerate() {
    if i > 0 && (digits.len() - i).is_multiple_of(3) {
      s.push(',');
    }

    s.push(c);
  }

  s
}

/// `114 of 272 MB`, `0.4 of 1.7 GB`.
fn done_text(done: u64, total: u64) -> String {
  let (d, t) = (done as f64, total as f64);
  match total >= 1_000_000_000 {
    true => format!("{:.1} of {:.1} GB", d / 1e9, t / 1e9),
    false => format!("{:.0} of {:.0} MB", d / 1e6, t / 1e6),
  }
}

/// The models `found` on disk that are ready: not those still downloading or being prepared for
/// the Neural Engine, whose file is there already.
pub fn ready(mut found: Vec<Found>, state: impl Fn(&str) -> Option<Download>) -> Vec<Found> {
  found
    .retain(|f| !matches!(state(f.0.name), Some(Download::Running { .. } | Download::Preparing)));
  found
}

/// Which model transcribes, as the session picks it: the chosen one when it is on disk, else the
/// first downloaded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pick {
  /// Its index among the downloaded; `None` for a file of its own outside the cache, or none.
  pub active: Option<usize>,
  /// The chosen model when it is none of the downloaded: its name, and whether its file is there
  /// (outside the cache) or gone.
  pub other: Option<(String, bool)>,
}

impl Pick {
  pub fn new(hub: &Hub, query: Option<&str>, found: &[Found]) -> Self {
    let first = (!found.is_empty()).then_some(0);
    let Some(query) = query else { return Self { active: first, other: None } };
    match hub.resolve(query) {
      Ok(path) => match found.iter().position(|f| f.1 == path) {
        Some(i) => Self { active: Some(i), other: None },
        None => Self { active: None, other: Some((label(query), true)) },
      },
      Err(_) => Self { active: first, other: Some((label(query), false)) },
    }
  }

  /// The chosen model when it is not on disk.
  pub fn gone(&self) -> Option<&str> {
    self.other.as_ref().filter(|(_, there)| !there).map(|(name, _)| name.as_str())
  }
}

/// Every language of wavo's models by name, after "All languages": `(label, code)`.
static LANGUAGES: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
  let mut codes: Vec<&str> = hub::MODELS.iter().flat_map(|m| m.langs.iter().copied()).collect();
  codes.sort_unstable();
  codes.dedup();
  let mut all: Vec<(String, &str)> = codes.into_iter().map(|c| (language_name(c), c)).collect();
  all.sort();
  all.insert(0, ("All languages".to_owned(), ""));
  all
});

/// Every word of `search` is in the model's names, maker or languages, in any case.
fn finds(search: &str, m: &ModelInfo) -> bool {
  let search = fold(search);
  if search.trim().is_empty() {
    return true;
  }

  let langs: Vec<String> = m.langs.iter().map(|c| language_name(c)).collect();
  let text =
    [m.full_name, m.name, m.family, m.maker, m.languages].join(" ") + " " + &langs.join(" ");
  let text = fold(&text);
  search.split_whitespace().all(|w| text.contains(w))
}

impl Prefs {
  fn models_head(&mut self, ui: &mut Ui, s: Style) {
    let items: Vec<(usize, &str)> = LANGUAGES.iter().map(|l| l.0.as_str()).enumerate().collect();
    ui.row(s, |ui| {
      ui.id("model-search").search(&mut self.model_search, "Search models");
      ui.id("model-lang").select(&mut self.model_lang, &items);
    });
  }

  /// wavo's models in two plates, in wavo's order, the one in use shown whatever the search.
  /// Every row is one line of name and one of caption in every state: the list never moves
  /// under the pointer.
  fn models(&mut self, ui: &mut Ui, fs: &FormStyle) {
    if let Some(note) = self.model_note() {
      ui.text(note, fs.caption.color(ui.theme().warning));
    }

    let fs = &FormStyle { caption: fs.caption.ellipsis(), ..*fs };
    let lang = LANGUAGES[self.model_lang].1;
    let active = self.active().map(|f| f.0.name);
    let shown = |m: &ModelInfo| {
      let lang = lang.is_empty() || m.langs.contains(&lang);
      active == Some(m.name) || (lang && finds(&self.model_search, m))
    };
    let on_disk = |m: &ModelInfo| self.found.iter().any(|f| f.0.name == m.name);
    let downloaded: Vec<&'static ModelInfo> =
      hub::MODELS.iter().filter(|m| on_disk(m) && shown(m)).collect();
    // The recommended first.
    let mut remote: Vec<&'static ModelInfo> =
      hub::MODELS.iter().filter(|m| !on_disk(m) && shown(m)).collect();
    remote.sort_by_key(|m| m.name != RECOMMENDED);
    let count = |n: usize| format!("{n} model{}", if n == 1 { "" } else { "s" });
    let mut act = None;
    if self.found.is_empty() {
      ui.form_section(fs, "Downloaded", |ui| {
        let row = FormRow::new("No model yet")
          .caption("Download the recommended one below to start dictating.")
          .tile(Tile::Icon(Icon::DOWNLOAD));
        ui.form_row(fs, row, |_| {});
      });
    } else if !downloaded.is_empty() {
      let size: u64 = downloaded.iter().map(|m| m.size).sum();
      let note = format!("{} · {}", count(downloaded.len()), size_text(size));
      ui.form_section_with(
        fs,
        "Downloaded",
        |ui| ui.text(&note, fs.note),
        |ui| {
          for &m in &downloaded {
            act = self.downloaded_row(ui, fs, m, active == Some(m.name)).or(act);
          }
        },
      );
    }

    if !remote.is_empty() {
      let note = count(remote.len());
      ui.form_section_with(
        fs,
        "Available",
        |ui| ui.text(&note, fs.note),
        |ui| {
          for &m in &remote {
            self.remote_row(ui, fs, m);
          }
        },
      );
    }

    if downloaded.is_empty() && remote.is_empty() {
      ui.text("No model matches.", fs.caption);
    }

    if let Some(m) = act {
      let size = size_text(m.size);
      let message =
        format!("It frees {size} in the Hugging Face cache. You can download it again.");
      let title = format!("Delete {}?", m.full_name);
      self.ask = Some(Ask { title, message, delete: Delete::Model(m) });
    }
  }

  fn use_model(&mut self, name: &'static str) {
    info!("settings: use model {name}");
    self.edit(move |s| s.model = Some(name.to_owned()));
  }

  /// Use (or "✓ In use"), the bin and ↗: the model to ask about deleting.
  fn downloaded_row(
    &mut self,
    ui: &mut Ui,
    fs: &FormStyle,
    m: &'static ModelInfo,
    in_use: bool,
  ) -> Option<&'static ModelInfo> {
    let t = ui.theme();
    let caption = format!("{} · {}", m.languages, size_text(m.size));
    let (mut used, mut act) = (false, None);
    ui.form_row(fs, model_row(m, Some(&caption)), |ui| {
      if in_use {
        ui.row(Style::new().gap(4.0).align(Align::Start, Align::Center), |ui| {
          ui.icon_stroke(Icon::CHECK, 14.0, t.accent, 2.2);
          let s = t.label().color(t.accent).font(t.font.weight(Weight::Medium)).nowrap();
          ui.text("In use", s);
        });
      } else {
        used = ui.id(("use", m.name)).button("Use").clicked;
      }

      let r = ui.id(("delete", m.name)).icon_button(Icon::TRASH);
      ui.tooltip(r.id, "Delete…");
      if r.clicked {
        act = Some(m);
      }

      self.card(ui, m);
    });

    if used {
      self.use_model(m.name);
    }

    act
  }

  /// Download (Retry after a failure), or its progress in place of the caption with ✕.
  fn remote_row(&self, ui: &mut Ui, fs: &FormStyle, m: &'static ModelInfo) {
    let state = self.shell.download_state(m.name);
    let (caption, progress) = match &state {
      Some(Download::Running { done, total, rate }) => {
        let (k, mut s) = share(*done, *total);
        if let Some(rate) = rate {
          s += &format!(" · {:.1} MB/s", rate / 1e6);
        }

        (None, Some((Some(k), s)))
      }
      Some(Download::Preparing) => (None, Some((None, "Preparing for the Neural Engine…".into()))),
      Some(Download::Failed(e)) => (Some(format!("Could not download: {e}")), None),
      None => {
        let first = if m.name == RECOMMENDED { "Recommended · " } else { "" };
        (Some(format!("{first}{} · {}", m.languages, size_text(m.size))), None)
      }
    };

    let failed = matches!(state, Some(Download::Failed(_)));
    let shown = Shown { caption, progress, failed };
    let row = model_row(m, None);
    data_row(ui, fs, Id::of(("model", m.name)), row, &shown, |ui| {
      if shown.progress.is_none() {
        let label = if failed { "Retry" } else { "Download" };
        let mut button = Button::new(label);
        if m.name == RECOMMENDED {
          button = button.primary();
        }

        if button.show(ui.id(("download", m.name))).clicked {
          self.shell.download(m);
        }
      } else {
        let r = ui.id(("cancel", m.name)).icon_button(Icon::X);
        let preparing = matches!(state, Some(Download::Preparing));
        ui.tooltip(r.id, if preparing { "Cancel" } else { "Cancel the download" });
        if r.clicked {
          self.shell.cancel_download(m.name);
        }
      }

      self.card(ui, m);
    });
  }

  /// A model's ↗, last in its row: its card (license, languages, benchmarks) in the browser, the
  /// sign About's links have too.
  fn card(&self, ui: &mut Ui, m: &ModelInfo) {
    let r = ui.id(("card", m.name)).icon_button(Icon::EXTERNAL);
    ui.tooltip(r.id, "Open the model’s page on Hugging Face");
    if r.clicked {
      self.shell.open_url(m.card);
    }
  }
}

/// A model's row: the letter of its family on its tile, its name on one line, `caption` under it.
fn model_row<'a>(m: &'a ModelInfo, caption: Option<&'a str>) -> FormRow<'a> {
  let row = FormRow::new(m.full_name).one_line().tile(Tile::Text(m.family.get(..1).unwrap_or("·")));
  match caption {
    Some(c) => row.caption(c),
    None => row,
  }
}

// MARK: Downloads

/// How a model's download goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Download {
  /// Bytes on disk of `total` (a part an earlier pull left counts), and bytes a second once it
  /// has run a while.
  Running { done: u64, total: u64, rate: Option<f64> },
  /// Prepared for the Neural Engine after the download.
  Preparing,
  /// Why it stopped; the next one resumes it.
  Failed(String),
}

struct Job {
  state: Download,
  cancel: Arc<AtomicBool>,
}

/// The app's downloads by model name, each on a thread of its own through [Hub::pull], which
/// resumes a broken download; they outlive the window.
#[derive(Clone, Default)]
pub struct Downloads(Arc<Mutex<HashMap<&'static str, Job>>>);

impl Downloads {
  pub fn get(&self, name: &str) -> Option<Download> {
    self.0.lock().unwrap().get(name).map(|j| j.state.clone())
  }

  /// The download whose cancel is `cancel` goes on as `state` (`None` ends it), unless a cancel
  /// forgot it and another may have started.
  fn set(&self, name: &str, cancel: &Arc<AtomicBool>, state: Option<Download>) {
    let mut jobs = self.0.lock().unwrap();
    if !jobs.get(name).is_some_and(|j| Arc::ptr_eq(&j.cancel, cancel)) {
      return;
    }

    match state {
      Some(state) => jobs.get_mut(name).unwrap().state = state,
      None => _ = jobs.remove(name),
    }
  }

  /// Forgets the download or its failure at once; its thread stops before its next chunk and
  /// leaves the part for the next one to resume.
  pub fn cancel(&self, name: &str) {
    if let Some(job) = self.0.lock().unwrap().remove(name) {
      job.cancel.store(true, Ordering::Relaxed);
    }
  }

  /// Downloads `model` into `hub`, unless it downloads already. `changed` runs on its thread as
  /// it goes, at most every 100 ms, and with `true` once it has ended.
  pub fn pull(&self, hub: Hub, model: &'static ModelInfo, changed: impl Fn(bool) + Send + 'static) {
    let cancel = Arc::new(AtomicBool::new(false));
    {
      let mut jobs = self.0.lock().unwrap();
      if jobs.get(model.name).is_some_and(|j| !matches!(j.state, Download::Failed(_))) {
        return;
      }

      let state = Download::Running { done: 0, total: model.size, rate: None };
      jobs.insert(model.name, Job { state, cancel: cancel.clone() });
    }

    let downloads = self.clone();
    let work = move || {
      let (started, mut told, mut from) = (Instant::now(), None::<Instant>, None);
      let progress = |p: wavo::hub::Progress| {
        let state = match p.preparing {
          Some(_) => Download::Preparing,
          None => {
            // This run's speed, once it has run a second: a resumed part does not count.
            let from = *from.get_or_insert(p.done);
            let secs = started.elapsed().as_secs_f64();
            let rate = (secs >= 1.0 && p.done > from).then(|| (p.done - from) as f64 / secs);
            Download::Running { done: p.done, total: p.total, rate }
          }
        };
        let tell = state == Download::Preparing
          || told.is_none_or(|t: Instant| t.elapsed() >= Duration::from_millis(100));
        downloads.set(model.name, &cancel, Some(state));
        if tell {
          told = Some(Instant::now());
          changed(false);
        }

        match cancel.load(Ordering::Relaxed) {
          true => ControlFlow::Break(()),
          false => ControlFlow::Continue(()),
        }
      };

      match hub.pull(model.name, progress) {
        Ok(path) => {
          info!("model {} downloaded: {}", model.name, path.display());
          downloads.set(model.name, &cancel, None);
        }
        Err(_) if cancel.load(Ordering::Relaxed) => {}
        Err(e) => {
          warn!("download {}: {e}", model.name);
          downloads.set(model.name, &cancel, Some(Download::Failed(e.to_string())));
        }
      }

      changed(true);
    };
    std::thread::Builder::new().name("download".into()).spawn(work).expect("spawn a download");
  }
}

// MARK: Transfers

/// How an export or an import goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Transfer<T> {
  /// Bytes done of `total`, 0 while it starts (the history is copied or read first).
  Running {
    done: u64,
    total: u64,
  },
  Done(T),
  Failed(String),
}

/// The archive and its size.
pub type Export = Transfer<(PathBuf, u64)>;
pub type Import = Transfer<Imported>;

/// Which file panel is open.
#[derive(Clone, Copy)]
enum Panel {
  Export,
  Import,
}

/// The archive Export suggests, to the second, so another one the same day keeps its own:
/// `wavo-export-2026-10-26-03-15-22.zip`.
fn export_name(ms: i64) -> String {
  let Some(t) = local_time(ms) else { return "wavo-export.zip".into() };
  let (y, mo, d, h, mi, s) = (t.year, t.month, t.day, t.hour, t.minute, ms / 1000 % 60);
  format!("wavo-export-{y:04}-{mo:02}-{d:02}-{h:02}-{mi:02}-{s:02}.zip")
}

/// What a Data row shows of a transfer: its caption, else its progress while it runs.
struct Shown {
  caption: Option<String>,
  progress: Option<(Option<f32>, String)>,
  failed: bool,
}

/// `starting` until it knows its total, `ended` once done; a failure says it could not `what`.
fn shown<T>(
  state: &Option<Transfer<T>>,
  what: &str,
  starting: &str,
  ended: impl Fn(&T) -> String,
) -> Shown {
  let (caption, progress) = match state {
    Some(Transfer::Running { total: 0, .. }) => (None, Some((None, starting.to_owned()))),
    Some(Transfer::Running { done, total }) => {
      let (k, text) = share(*done, *total);
      (None, Some((Some(k), text)))
    }
    Some(Transfer::Done(t)) => (Some(ended(t)), None),
    Some(Transfer::Failed(e)) => (Some(format!("Could not {what}: {e}")), None),
    None => (None, None),
  };
  Shown { caption, progress, failed: matches!(state, Some(Transfer::Failed(_))) }
}

/// A Data row with what it `shown`s: the progress under the title, a failure in red.
fn data_row<'a>(
  ui: &mut Ui,
  fs: &FormStyle,
  id: Id,
  row: FormRow<'a>,
  shown: &'a Shown,
  controls: impl FnOnce(&mut Ui),
) {
  let danger = FormStyle { caption: fs.caption.color(ui.theme().danger), ..*fs };
  let row = match &shown.caption {
    Some(caption) => row.caption(caption),
    None => row,
  };
  let below = |ui: &mut Ui| {
    if let Some((k, text)) = &shown.progress {
      progress_line(ui, fs, *k, text);
    }
  };
  ui.with_id(id).form_row_with(if shown.failed { &danger } else { fs }, row, below, controls);

  // An error cut short shows whole in its tooltip.
  if let Some(caption) = shown.caption.as_ref().filter(|_| shown.failed) {
    ui.tooltip(id.key("caption"), caption);
  }
}

/// `Added 120 dictations`, and the recordings that did not come.
fn imported_text(r: &Imported) -> String {
  let mut s = match r.added {
    0 => "Nothing new: all of its dictations are here".to_owned(),
    1 => "Added 1 dictation".to_owned(),
    n => format!("Added {n} dictations"),
  };
  if r.failed > 0 {
    s += &format!(" · {} recordings not copied", r.failed);
  }

  s
}

// MARK: History

/// How long a search stays the same before the history is read for it: a whole read takes 0.1 s,
/// too long for each key typed.
const SEARCH_WAIT: Duration = Duration::from_millis(150);
/// How long a note stands in the line under the panel, in place of the totals.
const NOTE_TIME: Duration = Duration::from_secs(4);

/// A calendar day: year, month, day.
type Day = (i32, u32, u32);

#[derive(Default)]
struct HistoryPage {
  db: Option<Rc<History>>,
  /// The session's history changes: a move reloads.
  revision: u64,
  search: String,
  /// All of them the search finds, newest first: the list builds only the rows it shows.
  entries: Vec<Entry>,
  /// Their days: each one's entries and duration.
  days: Vec<(Day, Range<usize>, i64)>,
  /// The whole history's ([Db::get_total]), read again when it changes.
  total: Option<(usize, i64, Option<i64>)>,
  /// What the entries were loaded for: the search and the revision.
  loaded: Option<(String, u64)>,
  /// The search as typed last, and when ([Ui::now]).
  typed: Option<(String, Duration)>,
  playing: Option<(i64, Playback)>,
  /// The entry the panel under the list shows.
  selected: Option<i64>,
  /// What the line under the panel says for [NOTE_TIME], and since when ([Ui::now]): an error,
  /// "Copied".
  note: Option<(String, Duration)>,
  /// A note from outside a frame (a request that failed on its thread), stamped at the next one.
  said: Option<String>,
}

impl HistoryPage {
  /// Reads the history again when it or the search changed; a search being typed keeps the last
  /// result until it stays the same for [SEARCH_WAIT]. Returns how long it still waits.
  fn load(&mut self, now: Duration) -> Option<Duration> {
    let search = self.search.trim().to_owned();
    if self.typed.as_ref().is_none_or(|(s, _)| *s != search) {
      self.typed = Some((search.clone(), now));
    }

    let since = self.typed.as_ref().map_or(SEARCH_WAIT, |(_, at)| now.saturating_sub(*at));
    let other = self.loaded.as_ref().is_some_and(|(s, _)| *s != search);
    if other && since < SEARCH_WAIT {
      return Some(SEARCH_WAIT - since);
    }

    let key = (search, self.revision);
    if self.loaded.as_ref() == Some(&key) {
      return None;
    }

    // The history changed, not only the search.
    let moved = self.loaded.as_ref().is_some_and(|k| k.1 != key.1);
    self.loaded = Some(key.clone());
    let Some(history) = &self.db else {
      self.note = Some(("Could not open the history: see the log".into(), now));
      return None;
    };

    let db = &history.db;
    if self.total.is_none() || moved {
      self.total = db.get_total().inspect_err(|e| warn!("history: {e:#}")).ok();
    }

    // All of them: 86k entries read in about 0.1 s.
    match db.get_page(&key.0, i64::MAX as usize) {
      Ok(entries) => (self.days, self.entries) = (days(&entries), entries),
      Err(e) => self.note = Some((format!("Could not read the history: {e:#}"), now)),
    }

    None
  }
}

/// `file`, with `.zip` added unless it ends in it; added, to a name not taken, as the panel asked
/// about replacing `file` only: `backup.zip`, else `backup 2.zip`.
fn with_zip(file: &Path) -> PathBuf {
  if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
    return file.to_owned();
  }

  let name = file.file_name().unwrap_or_default().to_string_lossy();
  let numbered = |n| if n == 1 { format!("{name}.zip") } else { format!("{name} {n}.zip") };
  (1..).map(|n| file.with_file_name(numbered(n))).find(|p| !p.exists()).unwrap()
}

/// The days of `entries`, newest first: each one's entries and their duration in ms.
fn days(entries: &[Entry]) -> Vec<(Day, Range<usize>, i64)> {
  let mut days: Vec<(Day, Range<usize>, i64)> = Vec::new();
  for (i, e) in entries.iter().enumerate() {
    let day = date_of(e.created_at);
    match days.last_mut() {
      Some((d, r, ms)) if *d == day => (r.end, *ms) = (i + 1, *ms + e.duration_ms),
      _ => days.push((day, i..i + 1, e.duration_ms)),
    }
  }

  days
}

/// A row of a name and a line under it: the padding, the two lines and nimbo's 2 points between.
fn two_line_h(fs: &FormStyle) -> f32 {
  2.0 * fs.pad_y + fs.label.line_h + 2.0 + fs.caption.line_h
}

fn date_of(ms: i64) -> Day {
  local_time(ms).map(|t| t.date()).unwrap_or_default()
}

/// A day's plate title: `Today`, `Yesterday, October 17`, `October 3`, `March 3, 2025`.
fn day_title(day: Day, now: i64) -> String {
  let (year, month, d) = day;
  let date = format!("{} {d}", Local { year, month, day: d, hour: 0, minute: 0 }.month_name());
  if day == date_of(now) {
    return "Today".into();
  }

  if day == date_of(now - 86_400_000) {
    return format!("Yesterday, {date}");
  }

  match year == date_of(now).0 {
    true => date,
    false => format!("{date}, {year}"),
  }
}

fn duration_text(ms: i64) -> String {
  if ms < 60_000 {
    return format!("{:.1} s", ms as f64 / 1000.0);
  }

  let s = ms / 1000;
  format!("{}:{:02}", s / 60, s % 60)
}

/// What an entry without text shows, here and in the menu's Recent: `no text · whisper-tiny`, the
/// model that tried it last.
pub fn no_text(tried_model: Option<&str>) -> String {
  match tried_model {
    Some(model) => format!("no text · {}", label(model)),
    None => "no text".into(),
  }
}

/// A row's line ([first_line]); one being recorded or transcribed has no text yet, not "no text".
fn row_line(e: &Entry, working: bool) -> String {
  if working && e.text.is_empty() { "Transcribing…".into() } else { first_line(e) }
}

/// A row's line: the text on one line, else [no_text].
fn first_line(e: &Entry) -> String {
  if e.text.is_empty() {
    return no_text(e.tried_model.as_deref());
  }

  e.text.chars().take(300).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Under the text: when, how long, the model.
fn details(e: &Entry) -> String {
  let mut parts = vec![local_time(e.created_at).map(Local::hm).unwrap_or_default()];
  parts.push(duration_text(e.duration_ms));
  parts.extend(e.model.as_deref().map(label));
  parts.retain(|p| !p.is_empty());
  parts.join(" · ")
}

/// `3 dictations · 1:15`: of the whole day, `(count, ms)`.
fn day_note((n, ms): (usize, i64)) -> String {
  let s = ms / 1000;
  let what = if n == 1 { "dictation" } else { "dictations" };
  format!("{n} {what} · {}:{:02}", s / 60, s % 60)
}

enum Act {
  Select(i64),
  Copy(String),
  Play(i64),
  Stop,
  Request(Request),
  Delete { id: i64, start: String, audio: bool },
}

/// The panel under the list, with the selected dictation's whole text and its actions.
const FOOT_H: f32 = 150.0;
/// The line under it with the whole history.
const TOTAL_H: f32 = 30.0;

impl Prefs {
  fn history_head(&mut self, ui: &mut Ui, s: Style) {
    let search = &mut self.history.search;
    ui.row(s, |ui| _ = ui.id("history-search").search(search, "Search dictations"));
  }

  /// The dictations, newest first, in a plate a day, a page at a time.
  fn history(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let h = &mut self.history;
    if let Some(said) = h.said.take() {
      h.note = Some((said, ui.now()));
    }

    if let Some(wait) = h.load(ui.now()) {
      ui.request_frame_in(wait);
    }
    // Deleted, or out of the search.
    if h.selected.is_some_and(|id| !h.entries.iter().any(|e| e.id == id)) {
      h.selected = None;
    }

    if h.entries.is_empty() {
      let empty = match h.search.trim().is_empty() {
        true => "No dictations yet: hold the shortcut and speak.",
        false => "Nothing found",
      };
      ui.form_section(fs, "", |ui| ui.form_row(fs, FormRow::new(empty), |_| {}));
    }

    // A plate a day, built only where it shows (nimbo's form list).
    let (now, mut act) = (now_ms(), None);
    let fs = &FormStyle { caption: fs.caption.ellipsis(), pad_y: fs.pad_y + 4.0, ..*fs };
    let titles: Vec<String> = h.days.iter().map(|(day, ..)| day_title(*day, now)).collect();
    let groups: Vec<(&str, usize)> =
      titles.iter().zip(&h.days).map(|(t, (_, r, _))| (t.as_str(), r.len())).collect();
    let (entries, days, status) = (&h.entries, &h.days, &self.status);
    let entry = |g: usize, i: usize| &entries[days[g].1.start + i];
    let key = |g, i| entry(g, i).id as u64;
    let note = |ui: &mut Ui, g: usize| {
      let (_, r, ms) = &days[g];
      ui.text(day_note((r.len(), *ms)), fs.note);
    };
    let row = |ui: &mut Ui, g, i| {
      let e = entry(g, i);
      let selected = h.selected == Some(e.id);
      let playing = h.playing.as_ref().is_some_and(|(id, _)| *id == e.id);
      let working = status.current == Some(e.id) || status.retranscribing.contains(&e.id);
      if let Some(a) = entry_row(ui, fs, e, selected, playing, working) {
        act = Some(a);
      }
    };
    ui.id("history-list").form_list(fs, &groups, two_line_h(fs), &key, note, row);

    self.history_act(ui, act);
  }

  /// Under the list, where it stays: the selected dictation's whole text, Transcribe Again,
  /// Restore the Previous Text and Delete.
  fn history_foot(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let transcribes = self.transcribes();
    let h = &self.history;
    let e = h.selected.and_then(|id| h.entries.iter().find(|e| e.id == id));
    let mut act = None;
    ui.el(Style::new().w(grow()).h(px(fs.hairline)).bg(fs.divider));
    let s = Style::new().w(grow()).h(px(FOOT_H)).pads([12.0, 20.0, 16.0, 20.0]).gap(10.0);
    ui.col(s, |ui| {
      let Some(e) = e else {
        let muted = t.label().color(t.muted);
        ui.text("Select a dictation to see all of its text.", muted);
        return;
      };

      let text = if e.text.is_empty() { no_text(e.tried_model.as_deref()) } else { e.text.clone() };
      let area = Style::new().w(grow()).h(grow());
      ui.id("history-text").scroll_area(area, |ui| {
        ui.col(Style::new().w(grow()), |ui| ui.text(&text, t.body()));
      });
      let busy = self.status.retranscribing.contains(&e.id);
      let audio = e.audio_path.is_some();
      ui.row(Style::new().w(grow()).gap(8.0), |ui| {
        let off = busy || !transcribes || !audio;
        if ui.disabled(off).id(("retranscribe", e.id)).button("Transcribe Again").clicked {
          act = Some(Act::Request(Request::Retranscribe(e.id)));
        }

        if self.status.restorable.contains(&e.id)
          && ui.disabled(busy).id(("restore", e.id)).button("Restore the Previous Text").clicked
        {
          act = Some(Act::Request(Request::Restore(e.id)));
        }

        ui.el(Style::new().w(grow()));
        // Not while it is transcribed again: the session refuses then.
        if ui.disabled(busy).id(("delete", e.id)).button("Delete…").clicked {
          let start = crate::history::shorten(&first_line(e), 60);
          act = Some(Act::Delete { id: e.id, start, audio });
        }
      });
    });

    self.history_act(ui, act);
  }

  /// Under the panel, the whole history: `86,286 dictations · 312 h · since March 14, 2026`; for
  /// [NOTE_TIME] after a note, the note.
  fn history_total(&self, ui: &mut Ui, fs: &FormStyle) {
    let now = ui.now();
    let note = self.history.note.as_ref().filter(|(_, at)| now < *at + NOTE_TIME);
    let text = match note {
      Some((note, at)) => {
        ui.request_frame_in(*at + NOTE_TIME - now);
        note.clone()
      }
      None => match self.total_text() {
        Some(text) => text,
        None => return,
      },
    };

    ui.el(Style::new().w(grow()).h(px(fs.hairline)).bg(fs.divider));
    ui.row(Style::new().w(grow()).h(px(TOTAL_H)).align(Align::Center, Align::Center), |ui| {
      ui.id("history-total").text(&text, fs.note.nowrap());
    });
  }

  fn total_text(&self) -> Option<String> {
    let (n, ms, Some(first)) = self.history.total.filter(|t| t.0 > 0)? else { return None };
    let what = if n == 1 { "dictation" } else { "dictations" };
    let spoken = match ms / 1000 {
      s if s < 60 => format!("{s} s"),
      s if s < 3600 => format!("{} min", s / 60),
      s => format!("{} h", s / 3600),
    };
    let since = local_time(first).map(|t| format!("{} {}, {}", t.month_name(), t.day, t.year));
    let mut text = format!("{} {what} · {spoken}", thousands(n));
    if let Some(since) = since {
      text += &format!(" · since {since}");
    }

    Some(text)
  }

  fn history_act(&mut self, ui: &mut Ui, act: Option<Act>) {
    let h = &mut self.history;
    match act {
      // Clicked again, it is unselected.
      Some(Act::Select(id)) => h.selected = (h.selected != Some(id)).then_some(id),
      Some(Act::Copy(text)) => {
        ui.clipboard().write(&text);
        h.note = Some(("Copied to the clipboard".into(), ui.now()));
      }
      Some(Act::Play(id)) => {
        let e = h.entries.iter().find(|e| e.id == id).unwrap();
        let Some(path) = h.db.as_ref().unwrap().audio_file(e) else {
          h.note = Some(("The recording is gone".into(), ui.now()));
          return;
        };

        let waker = ui.waker().clone();
        match Playback::start(&path, move || waker.wake()) {
          Ok(p) => h.playing = Some((id, p)),
          Err(e) => h.note = Some((format!("Could not play the recording: {e:#}"), ui.now())),
        }
      }
      Some(Act::Stop) => h.playing = None,
      Some(Act::Request(req)) => {
        h.note = None;
        self.shell.history(req);
      }
      Some(Act::Delete { id, start, audio }) => {
        let and = if audio { " and its recording are" } else { " is" };
        let message = format!("“{start}”{and} removed from the history.");
        let title = "Delete this dictation?".into();
        self.ask = Some(Ask { title, message, delete: Delete::Entry(id) });
      }
      None => {}
    }
  }
}

/// A dictation's row (`working`: being recorded or transcribed now), all of one height: its text on
/// a line and the details under it, Play and Copy centred at its right. A click on it selects it
/// (tinted), for the panel under the list.
fn entry_row(
  ui: &mut Ui,
  fs: &FormStyle,
  e: &Entry,
  selected: bool,
  playing: bool,
  working: bool,
) -> Option<Act> {
  let line = row_line(e, working);
  let caption = details(e);
  let mut fs = *fs;
  fs.row = fs.row.hit();
  if selected {
    fs.row = fs.row.bg(ui.theme().ghost).radius(fs.group.radii[0]);
  }

  // A row's controls go on its first line: they are centred in the whole row instead.
  let row_h = two_line_h(&fs);
  let text = Id::of(("text", e.id));
  let (mut act, mut on_text) = (None, false);
  let cover = |ui: &mut Ui| {
    let at = Float::parent().clip();
    on_text = ui.with_id(text).float(at).el(Style::new().w(grow()).h(grow())).clicked;
  };

  let row = FormRow::new(&line).caption(&caption).one_line();
  let r = ui.form_row_with(&fs, row, cover, |ui| {
    ui.row(Style::new().h(px(row_h)).gap(fs.gap).align(Align::End, Align::Center), |ui| {
      let audio = e.audio_path.is_some();
      let r = ui.disabled(!audio).id(("play", e.id));
      let r = Button::icon(if playing { Icon::STOP } else { Icon::PLAY }).large().show(r);
      ui.tooltip(r.id, if playing { "Stop" } else { "Play the recording" });
      if r.clicked {
        act = Some(if playing { Act::Stop } else { Act::Play(e.id) });
      }

      let r = ui.disabled(e.text.is_empty()).id(("copy", e.id));
      let r = Button::icon(Icon::COPY).large().show(r);
      ui.tooltip(r.id, "Copy");
      if r.clicked {
        act = Some(Act::Copy(e.text.clone()));
      }
    });
  });

  act.or((r.clicked || on_text).then_some(Act::Select(e.id)))
}

// MARK: About

const SOURCE: &str = "https://github.com/vladkens/wavo-app";
const COFFEE: &str = "https://buymeacoffee.com/vladkens";

/// `(name, what, url)`.
const ACKS: [(&str, &str, &str); 6] = [
  ("wavo", "Speech recognition on this machine", "https://github.com/vladkens/wavo"),
  ("nimbo", "The UI toolkit Wavo is drawn with", "https://github.com/vladkens/nimbo"),
  ("Handy", "The app that inspired Wavo", "https://github.com/cjpais/Handy"),
  (
    "transcribe.cpp",
    "The models’ GGUF conversions",
    "https://github.com/handy-computer/transcribe.cpp",
  ),
  (
    "ggml",
    "The tensor library and model format, by Georgi Gerganov and others",
    "https://github.com/ggml-org/ggml",
  ),
  (
    "The models",
    "OpenAI Whisper, NVIDIA Parakeet, Sber GigaAM, Useful Sensors Moonshine",
    "https://huggingface.co/handy-computer",
  ),
];

/// `assets/icon.png` at 128 px (64 points at 2x) as RGBA, from build.rs.
static ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/about-icon.rgba"));

impl Prefs {
  fn about(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let head =
      Style::new().w(grow()).pads([10.0, 0.0, 6.0, 0.0]).align(Align::Center, Align::Start);
    ui.col(head, |ui| {
      ui.rgba_image(128, 128, ICON, Style::new().w(px(64.0)).h(px(64.0)));
      ui.el(Style::new().h(px(10.0)));
      ui.text("Wavo", TextStyle::new(Font { size: 20.0, ..t.bold }, t.text).ellipsis());
      ui.el(Style::new().h(px(2.0)));
      ui.text(crate::VERSION, fs.note);
      ui.el(Style::new().h(px(8.0)));
      ui.text("Local dictation: hold a key, speak, and the text lands where you type.", t.label());
      ui.el(Style::new().h(px(14.0)));
      ui.row(Style::new().gap(8.0), |ui| {
        if ui.id(("link", "source")).button("View on GitHub").clicked {
          self.shell.open_url(SOURCE);
        }

        if ui.id(("link", "coffee")).button("Buy Me a Coffee").clicked {
          self.shell.open_url(COFFEE);
        }
      });
    });

    self.updates(ui, fs);
    // Denser than a setting's rows: six of them.
    let fs = &FormStyle { caption: fs.caption.ellipsis(), pad_y: 5.0, ..*fs };
    // A row's controls go on its name's line: the ↗ is centred in the whole row instead.
    let row_h = two_line_h(fs);
    ui.form_section(fs, "Acknowledgements", |ui| {
      for (name, what, url) in ACKS {
        ui.form_row(fs, FormRow::new(name).caption(what), |ui| {
          ui.col(Style::new().h(px(row_h)).align(Align::Center, Align::Center), |ui| {
            let r = Button::icon(Icon::EXTERNAL).large().show(ui.id(("link", name)));
            ui.tooltip(r.id, url.trim_start_matches("https://"));
            if r.clicked {
              self.shell.open_url(url);
            }
          });
        });
      }
    });
  }

  /// The updater's state, as the menu says it, with its button: Check for Updates, Update (and
  /// Release Notes), Retry. None in a build that doesn't update.
  fn updates(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let Some(state) = self.shell.update_state() else { return };
    let idle = match self.settings.auto_update {
      true => "Wavo looks for a new version at launch and once a day.",
      false => "Automatic updates are off: Wavo looks when you ask.",
    };
    let available = |r: &Release| format!("Update Available: {}", updater::shown(&r.version));
    let (label, caption, act, on) = match &state {
      State::Idle => ("Updates".to_owned(), idle.to_owned(), UpdateAct::Check, true),
      State::Checking => ("Updates".into(), "Checking…".into(), UpdateAct::Check, false),
      State::UpToDate(_) => {
        ("Updates".into(), "No updates available.".into(), UpdateAct::Check, true)
      }
      State::Available(v) => {
        (available(v), "Installs it and restarts Wavo.".into(), UpdateAct::Install, true)
      }
      State::Downloading(v, done) => {
        (available(v), format!("Downloading: {done}%"), UpdateAct::Install, false)
      }
      State::Restarting(v) => (available(v), "Restarting…".into(), UpdateAct::Install, false),
      State::Failed(_, why) => ("Update Failed".into(), why.clone(), UpdateAct::Retry, true),
    };

    ui.form_section(fs, "Updates", |ui| {
      ui.form_row(fs, FormRow::new(&label).caption(&caption), |ui| {
        let notes = matches!(state, State::Available(_));
        if notes && ui.id("update-notes").button("Release Notes").clicked {
          self.shell.update_act(UpdateAct::Notes);
        }

        let button = match act {
          UpdateAct::Install => Button::new("Update").primary(),
          UpdateAct::Retry => Button::new("Retry"),
          _ => Button::new("Check for Updates"),
        };
        if button.show(ui.disabled(!on).id("update")).clicked {
          self.shell.update_act(act);
        }
      });
    });
  }
}

#[cfg(test)]
mod tests {
  use std::cell::{RefCell, RefMut};
  use std::rc::Rc;

  use nimbo::{Appearance, Event, Harness, Modifiers, Phase, Point, Size};

  use super::*;
  use crate::testing::TempDir;

  #[derive(Default)]
  struct Log {
    settings: Settings,
    paused: Vec<bool>,
    /// All allowed unless set.
    access: Option<Access>,
    allowed: Vec<Pane>,
    states: HashMap<&'static str, Download>,
    downloads: Vec<&'static str>,
    cancelled: Vec<String>,
    deleted: Vec<&'static str>,
    requests: Vec<Request>,
    autostarts: Vec<bool>,
    export: Option<Export>,
    exports: Vec<PathBuf>,
    revealed: Vec<PathBuf>,
    import: Option<Import>,
    imports: Vec<PathBuf>,
    update: Option<State>,
    acts: Vec<UpdateAct>,
  }

  #[derive(Clone, Default)]
  struct Fake(Rc<RefCell<Log>>);

  impl Fake {
    fn log(&self) -> RefMut<'_, Log> {
      self.0.borrow_mut()
    }
  }

  impl Shell for Fake {
    fn edit(&self, edit: Box<dyn FnOnce(&mut Settings) + Send>) {
      edit(&mut self.log().settings);
    }

    fn has_neural_engine(&self) -> bool {
      true
    }

    fn pause_hotkey(&self, paused: bool) {
      self.log().paused.push(paused);
    }

    fn devices(&self) -> Vec<audio::Device> {
      let device = |name: &str, default, transport| audio::Device {
        id: Some(name.to_lowercase()),
        name: name.into(),
        default,
        transport,
      };
      let mac = device("MacBook Pro Microphone", true, Transport::BuiltIn);
      vec![mac, device("USB Mic", false, Transport::Usb)]
    }

    fn access(&self) -> Access {
      let all = Access {
        accessibility: true,
        microphone: Grant::Granted,
        secure_input: false,
        clipboard_unread: false,
      };
      self.log().access.unwrap_or(all)
    }

    fn allow(&self, pane: Pane) {
      self.log().allowed.push(pane);
    }

    /// It runs 42% through until the test says otherwise.
    fn download(&self, m: &'static ModelInfo) {
      let mut log = self.log();
      log.downloads.push(m.name);
      log
        .states
        .insert(m.name, Download::Running { done: m.size * 42 / 100, total: m.size, rate: None });
    }

    fn cancel_download(&self, name: &str) {
      let mut log = self.log();
      log.states.retain(|n, _| *n != name);
      log.cancelled.push(name.to_owned());
    }

    fn download_state(&self, name: &str) -> Option<Download> {
      self.log().states.get(name).cloned()
    }

    fn delete_model(&self, m: &'static ModelInfo) {
      self.log().deleted.push(m.name);
    }

    fn history(&self, req: Request) {
      self.log().requests.push(req);
    }

    fn autostart(&self) -> Result<Autostart, String> {
      Ok(Autostart::NotRegistered)
    }

    fn set_autostart(&self, on: bool) {
      self.log().autostarts.push(on);
    }

    /// It runs 37% through until the test says otherwise.
    fn export(&self, file: PathBuf) {
      let mut log = self.log();
      log.exports.push(file);
      log.export = Some(Transfer::Running { done: 6_000_000_000, total: 16_000_000_000 });
    }

    fn export_state(&self) -> Option<Export> {
      self.log().export.clone()
    }

    fn import(&self, file: PathBuf) {
      let mut log = self.log();
      log.imports.push(file);
      log.import = Some(Transfer::Running { done: 0, total: 0 });
    }

    fn import_state(&self) -> Option<Import> {
      self.log().import.clone()
    }

    fn reveal(&self, dir: &Path) {
      self.log().revealed.push(dir.to_owned());
    }

    fn update_state(&self) -> Option<State> {
      self.log().update.clone()
    }

    fn update_act(&self, act: UpdateAct) {
      self.log().acts.push(act);
    }
  }

  /// A home in a temp dir with a Hugging Face cache in it that has `models`.
  struct Home {
    dir: TempDir,
    hub: Hub,
  }

  impl Home {
    fn new(name: &str, models: &[&str]) -> Self {
      let dir = TempDir::new(name);
      let hub = Hub::at(dir.path().join("hub"));
      for m in models {
        crate::testing::download(&hub, m);
      }

      Self { dir, hub }
    }

    fn harness(&self, fake: &Fake, settings: Settings) -> Harness<Prefs> {
      fake.log().settings = settings.clone();
      let shell = Box::new(fake.clone());
      let history = History::open(self.dir.path()).ok().map(Rc::new);
      let prefs = Prefs::new(shell, settings, self.hub.clone(), self.dir.path(), history);
      let cfg = config(Page::General);
      let mut h = Harness::new(prefs, cfg.size, 2.0);
      // As high as macOS 26's, as nimbo's own shots draw it.
      h.rt.set_titlebar(Some((32.0, cfg.title)));
      h.event(Event::Appearance(Appearance::LIGHT));
      h.frame();
      h.frame();
      h
    }

    /// Dictations `(text, minutes ago)` with a recording each, the oldest first: one without text
    /// was tried by whisper-tiny. Their ids.
    fn dictations(&self, items: &[(&str, i64)]) -> Vec<i64> {
      let h = History::open(self.dir.path()).unwrap();
      let now = now_ms();
      let entries: Vec<Entry> = (items.iter())
        .map(|&(text, ago)| {
          let (name, path) = h.recordings.new_file(now - ago * 60_000);
          crate::testing::write_wav(&path, &[0.0; 1_600]);
          let model = "parakeet-tdt-0.6b-v3-Q8_0".to_owned();
          Entry {
            created_at: now - ago * 60_000,
            duration_ms: 1_500 + 70 * text.len() as i64,
            audio_path: Some(name),
            text: text.into(),
            model: (!text.is_empty()).then(|| model.clone()),
            tried_model: text.is_empty().then(|| "whisper-tiny-Q8_0".into()),
            pasted: true,
            ..Entry::default()
          }
        })
        .collect();
      h.db.insert(&entries).unwrap()
    }
  }

  const TWO: [&str; 2] = ["parakeet-v3", "parakeet-v2"];

  /// Clicks `id` and lets the frames after it settle (a select's list opens on the next one).
  fn click(h: &mut Harness<Prefs>, id: Id) {
    assert!(h.rect(id).is_some(), "{id:?} is not shown");
    h.click_id(id);
    h.frame();
    h.frame();
  }

  fn tab(h: &mut Harness<Prefs>, page: Page) {
    let i = PAGES.iter().position(|p| p.0 == page).unwrap();
    click(h, Id::of(TABS).key(i));
    h.frame();
    assert_eq!(h.app().page, page);
  }

  /// The dialog's Delete, once it has faded in.
  fn confirm(h: &mut Harness<Prefs>) {
    assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "the dialog");
    h.wait(Duration::from_millis(200));
    click(h, Id::of(CONFIRM).key("action"));
  }

  /// The dictation in progress shows as being transcribed, not as one without text.
  #[test]
  fn the_dictation_in_progress_is_transcribing() {
    let e = |text: &str| Entry {
      text: text.into(),
      tried_model: Some("parakeet-ultra-Q8_0".into()),
      ..Entry::default()
    };
    assert_eq!(row_line(&e(""), true), "Transcribing…");
    assert!(row_line(&e(""), false).starts_with("no text"));
    assert_eq!(row_line(&e("Hi."), true), "Hi.");
  }

  /// GNOME's save panel adds no extension: Import's filter would hide the archive.
  #[test]
  fn an_export_ends_in_zip() {
    for (picked, file) in
      [("a/wavo backup", "a/wavo backup.zip"), ("a/x.ZIP", "a/x.ZIP"), ("x.tar", "x.tar.zip")]
    {
      assert_eq!(with_zip(Path::new(picked)), PathBuf::from(file));
    }

    // Never over an archive the panel didn't ask about.
    let dir = TempDir::new("export-zip");
    std::fs::write(dir.path().join("backup.zip"), "").unwrap();
    assert_eq!(with_zip(&dir.path().join("backup")), dir.path().join("backup 2.zip"));
  }

  /// Every page in both looks, as high as its content or [SCROLLING] high; with `WAVO_SHOTS` set
  /// (`make shots`), written there: also Models asking before a delete and General at the first
  /// launch.
  #[test]
  fn every_page_renders_in_both_looks() {
    let home =
      Home::new("window-shots", &["parakeet-v3", "parakeet-v2", "whisper-turbo", "gigaam-v3"]);
    let ids = home.dictations(&[
      ("Draft a reply to Anna: the build is green, the release goes out on Monday.", 1500),
      ("Купить кофе, молоко и батарейки для микрофона.", 1410),
      ("Ship the CPU renderer first; Metal stays opt-in until we measure it.", 18),
      ("Remind me to check the parakeet download on the Linux box tonight.", 7),
      ("", 4),
      (
        "Let’s move the settings to top tabs and drop the sidebar for now. Models and history \
         become plates of rows, and every row keeps its play and copy buttons.",
        2,
      ),
      ("Отдельно я хотел бы чтобы ты импортировал эту штуку отдельным экшеном", 1),
    ]);
    let history = History::open(home.dir.path()).unwrap();
    let again = "Отдельно я хотел бы, чтобы ты импортировал эту штуку отдельным экшеном. Сначала \
      архив с историей, потом настройки, и чтобы можно было посмотреть, что придёт, до того как \
      что-то запишется. Если записи уже есть, они пропускаются, а аудио копируется только то, \
      которого нет. И ещё: прогресс копирования, потому что архивы бывают на десятки гигабайт, \
      и окно не должно при этом зависать. В конце короткая сводка: сколько добавлено, сколько \
      было, сколько не скопировалось и почему.";
    history.db.update_text(ids[6], again, Some("whisper-large-v3-turbo-Q8_0"), None).unwrap();
    let first_run = Home::new("window-shots-first-run", &[]);
    let dir = std::env::var_os("WAVO_SHOTS").map(PathBuf::from);
    let shot = |h: &mut Harness<Prefs>, name: &str| {
      h.leave();
      h.wait(Duration::from_millis(400));
      if let Some(dir) = &dir {
        std::fs::create_dir_all(dir).unwrap();
        h.shot(&dir.join(format!("{name}.png"))).unwrap();
      }
    };

    for (look, appearance) in [("light", Appearance::LIGHT), ("dark", Appearance::DARK)] {
      let fake = Fake::default();
      let running = Download::Running { done: 114_000_000, total: 272_000_000, rate: Some(3.2e6) };
      fake.log().states.insert("gigaam-v3-e2e-ctc", running);
      fake.log().update = Some(State::Available(Release::test("0.2.0")));
      let mut h = home.harness(&fake, Settings::default());
      h.event(Event::Appearance(appearance));
      for (page, _, name) in PAGES {
        tab(&mut h, page);
        shot(&mut h, &format!("{}-{look}", name.to_lowercase()));
        let size = h.rt.size;
        let sc = h.rt.ui.scroll(Id::of(("page", page as u8))).expect("the page's scroll area");
        match page.scrolls() {
          true => assert_eq!(size, Size::new(WIDTH, SCROLLING + 32.0), "{name}: and the title bar"),
          false => {
            let blank = sc.viewport.h - sc.content.h;
            assert!((-0.5..=1.0).contains(&blank) && size.w == WIDTH, "{name} fits: {sc:?}");
          }
        }

        // The window's ground under the toolbar's edge: dark in the dark look.
        let ground = h.render().pixel(4.0, 100.0);
        assert_eq!(ground[0] < 100, appearance.dark, "{name} {look}: {ground:?}");
      }

      tab(&mut h, Page::History);
      click(&mut h, Id::of(("text", ids[6])));
      shot(&mut h, &format!("history-selected-{look}"));
      // The panel's long text scrolls under the wheel.
      let panel = Id::of("history-text");
      let sc = h.rt.ui.scroll(panel).expect("the panel's scroll area");
      assert!(sc.content.h > sc.viewport.h, "{look}: the text overflows: {sc:?}");
      let v = sc.viewport;
      let (pos, delta) = (Point::new(v.x + v.w / 2.0, v.y + v.h / 2.0), Point::new(0.0, -40.0));
      let (phase, momentum, mods) = (Phase::None, Phase::None, Modifiers::default());
      h.event(Event::Scroll { pos, delta, precise: true, phase, momentum, mods });
      h.frame();
      assert!(h.rt.ui.scroll(panel).unwrap().offset.y > 0.0, "{look}: it scrolled");

      tab(&mut h, Page::Models);
      click(&mut h, Id::of(("delete", "whisper-turbo")));
      assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "{look}: the dialog");
      shot(&mut h, &format!("models-delete-{look}"));

      let fake = Fake::default();
      let none = Access {
        accessibility: false,
        microphone: Grant::NotDetermined,
        secure_input: false,
        clipboard_unread: false,
      };
      fake.log().access = Some(none);
      let mut h = first_run.harness(&fake, Settings::default());
      h.event(Event::Appearance(appearance));
      shot(&mut h, &format!("general-first-run-{look}"));
    }
  }

  #[test]
  fn the_recorder_pauses_the_hotkey_while_it_records() {
    let home = Home::new("window-hotkey", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    click(&mut h, Id::of("hotkey"));
    assert_eq!(fake.log().paused, [true]);
    h.key('k', Modifiers::CONTROL | Modifiers::ALT);
    h.frame();
    h.frame();
    assert_eq!(fake.log().settings.hotkey, "ctrl+option+k");
    assert_eq!(fake.log().paused, [true, false], "the hotkey is back");

    click(&mut h, Id::of("reset"));
    assert_eq!(fake.log().settings.hotkey, Settings::default().hotkey);

    // Leaving the page while recording brings the hotkey back.
    click(&mut h, Id::of("hotkey"));
    tab(&mut h, Page::About);
    assert_eq!(fake.log().paused, [true, false, true, false]);
  }

  /// About says what the updater does, with its button; Advanced turns the automatic checks off.
  /// Neither shows in a build that doesn't update.
  #[test]
  fn about_offers_the_update() {
    let home = Home::new("window-update", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::About);
    assert!(h.rect(Id::of("update")).is_none());
    tab(&mut h, Page::Advanced);
    assert!(h.rect(Id::of("auto-update")).is_none());

    // The app redraws the window as the state changes.
    let set = |h: &mut Harness<Prefs>, state| {
      fake.log().update = Some(state);
      h.app_mut();
      h.frame();
    };
    set(&mut h, State::Available(Release::test("0.2.0")));
    click(&mut h, Id::of("auto-update"));
    assert!(!fake.log().settings.auto_update);
    tab(&mut h, Page::About);
    click(&mut h, Id::of("update-notes"));
    click(&mut h, Id::of("update"));
    set(&mut h, State::Failed(None, "offline".into()));
    assert!(h.rect(Id::of("update-notes")).is_none());
    click(&mut h, Id::of("update"));
    assert_eq!(fake.log().acts, [UpdateAct::Notes, UpdateAct::Install, UpdateAct::Retry]);
  }

  #[test]
  fn selects_and_switches_edit_the_settings() {
    let home = Home::new("window-selects", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    // The microphone's items: Automatic, the system default, then the devices.
    for (id, item) in [("activation", 2), ("mic", 3), ("model", 1)] {
      click(&mut h, Id::of(id));
      click(&mut h, Id::of(id).key(("item", item)));
    }

    click(&mut h, Id::of("paste"));
    let usb = Choice { id: Some("usb mic".into()), name: "USB Mic".into() };
    let s = fake.log().settings.clone();
    assert_eq!(
      (s.activation, s.microphone, s.model.as_deref()),
      (Activation::Toggle, Microphone::Device(usb), Some(TWO[1]))
    );
    assert!(!s.auto_paste);

    // The model pop-up's last item, "Download more…", goes to Models.
    click(&mut h, Id::of("model"));
    click(&mut h, Id::of("model").key(("item", 2)));
    assert_eq!(h.app().page, Page::Models);

    tab(&mut h, Page::Advanced);
    click(&mut h, Id::of("retention"));
    click(&mut h, Id::of("retention").key(("item", 1)));
    assert_eq!(fake.log().settings.history_retention, Retention::Count(100));
    click(&mut h, Id::of("ane"));
    assert!(!fake.log().settings.neural_engine);

    // Launch at login, on General, goes to the system, one change at a time; a refusal says why.
    tab(&mut h, Page::General);
    click(&mut h, Id::of("login"));
    click(&mut h, Id::of("login"));
    assert_eq!(fake.log().autostarts, [true]);
    h.app_mut().autostart_set(true, Err("not permitted".into()));
    assert!(!h.app().settings.autostart);
    assert_eq!(h.app().login.error.as_deref(), Some("Couldn't turn it on: not permitted"));
  }

  /// Export asks the save panel where; it is disabled while it runs; Finder shows the archive only
  /// on a click.
  #[test]
  fn the_export_shows_how_it_goes() {
    let home = Home::new("window-export", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Advanced);
    click(&mut h, Id::of("export"));
    let file = home.dir.path().join("wavo-export.zip");
    let asked = h.file_dialog(Some(&file)).unwrap();
    assert!(asked.save && asked.name.unwrap().starts_with("wavo-export-"));
    h.frame();
    assert_eq!(fake.log().exports, std::slice::from_ref(&file));
    assert!(fake.log().imports.is_empty(), "the answer goes to the panel that asked");

    click(&mut h, Id::of("export"));
    assert!(h.file_dialog(None).is_none(), "disabled while it runs");
    assert!(h.rect(Id::of("export-show")).is_none());

    std::fs::write(&file, "").unwrap();
    fake.log().export = Some(Transfer::Done((file.clone(), 16_000_000_000)));
    h.frame();
    click(&mut h, Id::of("export-show"));
    assert_eq!(fake.log().revealed, [file]);
  }

  /// Import asks the system's panel for a .zip and imports the one picked, one at a time.
  #[test]
  fn the_import_takes_the_archive_picked() {
    let home = Home::new("window-import", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Advanced);
    click(&mut h, Id::of("import"));
    let file = home.dir.path().join("wavo-export.zip");
    assert_eq!(h.file_dialog(Some(&file)).unwrap().types, ["zip"]);
    h.frame();
    assert_eq!(fake.log().imports, [file]);
    assert!(fake.log().exports.is_empty());

    click(&mut h, Id::of("import"));
    assert!(h.file_dialog(None).is_none(), "disabled while it runs");
  }

  #[test]
  fn models_are_downloaded_used_found_and_deleted() {
    let home = Home::new("window-models", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let remote = hub::MODELS[2].name;
    click(&mut h, Id::of(("download", remote)));
    assert_eq!(fake.log().downloads, [remote]);
    click(&mut h, Id::of(("cancel", remote)));
    assert_eq!(fake.log().cancelled, [remote]);
    assert!(h.rect(Id::of(("download", remote))).is_some());

    // The first found is in use without a choice; the second can be.
    assert!(h.rect(Id::of(("use", TWO[0]))).is_none());
    click(&mut h, Id::of(("use", TWO[1])));
    assert_eq!(fake.log().settings.model.as_deref(), Some(TWO[1]));
    assert!(h.rect(Id::of(("use", TWO[0]))).is_some() && h.rect(Id::of(("use", TWO[1]))).is_none());

    // The search finds by name; the model in use stays shown.
    click(&mut h, Id::of("model-search"));
    h.type_text("moonshine");
    h.frame();
    h.frame();
    let shown: Vec<&str> = (hub::MODELS.iter())
      .filter(|m| h.rect(Id::of(("card", m.name))).is_some())
      .map(|m| m.name)
      .collect();
    assert_eq!(shown, [TWO[1], "moonshine-tiny", "moonshine-base"]);

    click(&mut h, Id::of(("delete", TWO[1])));
    confirm(&mut h);
    assert_eq!(fake.log().deleted, [TWO[1]]);
  }

  #[test]
  fn the_history_copies_transcribes_again_and_deletes() {
    let home = Home::new("window-history", &TWO);
    let ids =
      home.dictations(&[("Let's move the design review.", 3), ("Send the invoice to Anna.", 2)]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::History);
    click(&mut h, Id::of(("copy", ids[1])));
    assert_eq!(h.rt.clipboard().read().as_deref(), Some("Send the invoice to Anna."));
    // Said under the panel for a while, the search keeping its width; then the totals again.
    assert_eq!(h.text(Id::of("history-total")), Some("Copied to the clipboard"));
    let search = h.rect(Id::of("history-search")).unwrap();
    h.wait(NOTE_TIME);
    assert_eq!(h.rect(Id::of("history-search")), Some(search));

    // Nothing selected: the panel under the list has no actions; a click on a row selects it.
    assert!(h.rect(Id::of(("retranscribe", ids[0]))).is_none());
    click(&mut h, Id::of(("text", ids[0])));
    assert_eq!(h.app().history.selected, Some(ids[0]));
    let total = h.text(Id::of("history-total")).unwrap();
    assert!(total.starts_with("2 dictations · ") && total.contains(" · since "), "{total}");

    // Unselected by a click on it again, by Esc, and by a click on nothing above the panel.
    click(&mut h, Id::of(("text", ids[0])));
    assert_eq!(h.app().history.selected, None);
    click(&mut h, Id::of(("text", ids[0])));
    h.key(NamedKey::Escape, Modifiers::empty());
    assert_eq!(h.app().history.selected, None);
    click(&mut h, Id::of(("text", ids[0])));
    let last = h.rect(Id::of(("text", ids[1]))).unwrap();
    h.click(last.x, last.y + last.h + 60.0);
    assert_eq!(h.app().history.selected, None);
    click(&mut h, Id::of(("text", ids[0])));

    // Transcribed again from the panel: its new text came, and the one before comes back.
    click(&mut h, Id::of(("retranscribe", ids[0])));
    let history = History::open(home.dir.path()).unwrap();
    let again = "Move the design review.";
    history.db.update_text(ids[0], again, Some("whisper-tiny-Q8_0"), None).unwrap();
    let settings = h.app().settings.clone();
    let status = Status { restorable: vec![ids[0]], ..Status::default() };
    h.app_mut().update(&settings, &status, 2);
    h.rt.waker().wake();
    h.frame();
    click(&mut h, Id::of(("restore", ids[0])));
    click(&mut h, Id::of(("delete", ids[0])));
    let want = "“Move the design review.” and its recording are removed from the history.";
    assert_eq!(h.app().ask.as_ref().map(|a| a.message.as_str()), Some(want));
    confirm(&mut h);
    let want = [Request::Retranscribe(ids[0]), Request::Restore(ids[0]), Request::Delete(ids[0])];
    assert_eq!(fake.log().requests, want);

    // The search finds by text, in any case, once it is typed: meanwhile the last result stays.
    click(&mut h, Id::of("history-search"));
    h.type_text("INVOICE");
    h.frame();
    assert!(h.rect(Id::of(("copy", ids[0]))).is_some(), "the last result while it is typed");
    h.wait(SEARCH_WAIT);
    h.frame();
    assert!(
      h.rect(Id::of(("copy", ids[1]))).is_some() && h.rect(Id::of(("copy", ids[0]))).is_none()
    );
  }

  #[test]
  fn numbers_read_in_thousands() {
    let got: Vec<_> = [0, 7, 999, 1_000, 86_286, 1_234_567].map(thousands).into();
    assert_eq!(got, ["0", "7", "999", "1,000", "86,286", "1,234,567"]);
  }

  /// The whole history loads at once; the list builds only the rows it shows.
  #[test]
  fn the_history_builds_only_the_rows_it_shows() {
    let home = Home::new("window-long", &TWO);
    let texts: Vec<String> = (0..300).map(|i| format!("Dictation {i}")).collect();
    let items: Vec<(&str, i64)> = (0..300).map(|i| (texts[i].as_str(), 300 - i as i64)).collect();
    let ids = home.dictations(&items);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::History);
    assert!(h.rect(Id::of(("text", ids[299]))).is_some(), "the newest, at the top");
    assert!(h.rect(Id::of(("text", ids[0]))).is_none(), "the oldest, far below, not built");
    assert!(h.text(Id::of("history-total")).unwrap().starts_with("300 dictations"));
  }

  /// The first launch: a row for each permission missing, with Allow, gone once given and the
  /// window is in front again; with no model, a button to Models in place of the pop-up.
  #[test]
  fn general_guides_the_first_launch() {
    let home = Home::new("window-first-run", &[]);
    let fake = Fake::default();
    let none = Access {
      accessibility: false,
      microphone: Grant::NotDetermined,
      secure_input: false,
      clipboard_unread: false,
    };
    fake.log().access = Some(none);
    let mut h = home.harness(&fake, Settings::default());
    assert!(h.rect(Id::of("model")).is_none());
    click(&mut h, Id::of("open-accessibility"));
    click(&mut h, Id::of("open-microphone"));
    assert_eq!(fake.log().allowed, [Pane::Accessibility, Pane::Microphone]);

    fake.log().access = Some(Access { accessibility: true, microphone: Grant::Granted, ..none });
    h.event(Event::Focus(false));
    h.frame();
    assert!(h.rect(Id::of("open-microphone")).is_some(), "not looked at in the back");
    h.event(Event::Focus(true));
    h.frame();
    h.frame();
    assert!(h.rect(Id::of("open-microphone")).is_none());
    assert!(h.rect(Id::of("open-accessibility")).is_none());

    click(&mut h, Id::of("download-model"));
    assert_eq!(h.app().page, Page::Models);
  }
}
