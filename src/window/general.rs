// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The General and Advanced pages, every row one line high: what a row would say more is behind
//! its ⓘ, which shows only when there is something to say. General: while the system does not
//! allow one, a row for the microphone and one for Accessibility (why in the ⓘ, Allow while the
//! system can still ask, else Open Settings), gone once allowed; then the hotkey and how it
//! records, the microphone, the model (a quick switch among the downloaded ones and the way to
//! the Models page, or with none a button to download one), the language, launch at login.
//! Advanced: the output, the model's unload, the history's retention, where the files are (the
//! models, the recordings).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nimbo::autostart::State as Autostart;
use nimbo::permissions::{Access as Grant, Pane};
use nimbo::{Button, FormRow, FormStyle, Hotkey, Icon, Id, Select, Tile, Ui, open, paths};
use tracing::{info, warn};

use super::{Page, Prefs, Problem, models};
use crate::audio::Choice;
use crate::history::Retention;
use crate::settings::{Activation, HudPosition, Settings, language_name};

pub const HOTKEY: &str = "hotkey";
pub const RESET: &str = "reset";
pub const ACTIVATION: &str = "activation";
pub const MIC: &str = "mic";
/// The model pop-up: the downloaded models, then "Download more…".
pub const MODEL: &str = "use-model";
/// With no model downloaded, in place of [`MODEL`]: the Models page.
pub const DOWNLOAD_MODEL: &str = "download-model";
pub const LANG: &str = "lang";
pub const PASTE: &str = "paste";
pub const KEEP: &str = "keep";
pub const UNLOAD: &str = "unload";
pub const HUD: &str = "hud";
pub const LOGIN: &str = "login";
pub const RETENTION: &str = "retention";
pub const OPEN_ACCESSIBILITY: &str = "open-accessibility";
pub const OPEN_MICROPHONE: &str = "open-microphone";
pub const MODEL_FOLDER: &str = "model-folder";
pub const RECORDINGS_FOLDER: &str = "recordings-folder";

/// The shortcut recorder records: it has the focus in a window that has the keys.
pub fn records(ui: &Ui) -> bool {
  ui.focused(Id::of(HOTKEY)) && ui.window_focused()
}

/// The row of a control: its ⓘ is `row(name).key("info")`, its caption `row(name).key("caption")`.
pub fn row(name: &str) -> Id {
  Id::of(("row", name))
}

const FN_NOTE: &str =
  "Set System Settings › Keyboard › “Press 🌐 key to” to “Do Nothing”, or macOS acts on it too.";
const ACTIVATION_HELP: &str = "How the hotkey records: while you hold it, from one press to the \
  next, or both (a short tap latches the recording on).";
const SECURE_INPUT_NOTE: &str = "Secure Input is on (a password field, or a terminal’s Secure \
  Keyboard Entry): the shortcut works through a backup path, and Esc may not cancel.";
const CLIPBOARD_NOTE: &str = "Wavo can’t put the clipboard back after a paste: allow it in System \
  Settings › Privacy & Security › Paste from Other Apps.";
const MORE_MODELS: &str = "Download more…";
const MICROPHONE_WHY: &str = "Wavo records your voice through it while you hold the shortcut; \
  the recording stays on this Mac.";
const ACCESSIBILITY_WHY: &str = "Wavo hears the shortcut in every app and pastes the text where \
  you type through it.";

/// A warning sign, for a permission missing: a triangle with "!" in it.
const WARNING: Icon = Icon::new("M12 4.5 3 19.5h18ZM12 10v4.5M12 17v.2");
const UNLOAD_HELP: &str = "Frees the model's memory after Wavo has been idle this long. The next \
  dictation loads it again, which takes a moment.";

const ACTIVATIONS: [(Activation, &str); 3] = [
  (Activation::HoldOrToggle, "Hold to talk, or tap to start and stop"),
  (Activation::PushToTalk, "Hold to talk"),
  (Activation::Toggle, "Tap to start and stop"),
];

const HUDS: [(HudPosition, &str); 2] =
  [(HudPosition::Bottom, "Bottom of the screen"), (HudPosition::Top, "Top of the screen")];

const UNLOADS: [Option<u32>; 7] = [None, Some(0), Some(2), Some(5), Some(10), Some(15), Some(60)];

const RETENTIONS: [Retention; 7] = [
  Retention::Unlimited,
  Retention::Count(100),
  Retention::Count(1000),
  Retention::Count(10_000),
  Retention::Days3,
  Retention::Weeks2,
  Retention::Months3,
];

/// Launch at login as the switch shows it: the system's state, the change on its way (the
/// system answers in its own time, [`super::Shell::set_autostart`]) and one asked for meanwhile,
/// and why the last change failed.
pub struct Login {
  /// `Err` with the reason when unavailable; empty before it is known.
  pub state: Result<Autostart, String>,
  /// Being set: to on or off.
  pub busy: Option<bool>,
  /// Asked for while another was being set: set once that one ends.
  pub next: Option<bool>,
  pub error: Option<String>,
}

impl Default for Login {
  fn default() -> Self {
    Self { state: Err(String::new()), busy: None, next: None, error: None }
  }
}

impl Login {
  /// What the switch shows: the latest asked for, else the system's (the saved choice while the
  /// system's is unknown).
  pub fn shown(&self, saved: bool) -> bool {
    let state = self.state.as_ref().map_or(saved, |s| s.wanted());
    self.next.or(self.busy).unwrap_or(state)
  }
}

/// A folder the Advanced page shows, with its size once a shell's thread added it up
/// ([`super::Shell::folder_size`]).
pub struct Folder {
  pub dir: PathBuf,
  pub size: Arc<Mutex<Option<u64>>>,
  /// The size is asked for: not again until the window looks again ([`Prefs::refresh`]).
  pub asked: bool,
}

impl Folder {
  pub fn new(dir: PathBuf) -> Self {
    Self { dir, size: Arc::default(), asked: false }
  }
}

/// `~/.cache/huggingface/hub · 2.9 GB`, the path alone while the size is unknown or nothing.
fn folder_caption(dir: &Path, size: Option<u64>) -> String {
  match size {
    None | Some(0) => paths::display(dir),
    Some(n) => format!("{} · {}", paths::display(dir), models::size_text(n)),
  }
}

/// A pop-up `id` of `labels`, item `at` the current one, with a line after each of `lines`:
/// the item picked, when it is another.
fn pick(ui: &mut Ui, id: &str, labels: &[String], at: usize, lines: &[usize]) -> Option<usize> {
  let items: Vec<(usize, &str)> = labels.iter().enumerate().map(|(i, l)| (i, l.as_str())).collect();
  let mut cur = at;
  Select::new(&mut cur, &items).separators(lines).show(ui.id(id)).changed.then_some(cur)
}

/// `10,000`.
pub(super) fn thousands(n: impl ToString) -> String {
  let digits = n.to_string();
  let mut out = String::new();
  for (i, c) in digits.chars().enumerate() {
    if i > 0 && (digits.len() - i).is_multiple_of(3) {
      out.push(',');
    }

    out.push(c);
  }

  out
}

fn unload_title(minutes: Option<u32>) -> String {
  match minutes {
    None => "Never".into(),
    Some(0) => "Right after each dictation".into(),
    Some(1) => "After 1 minute".into(),
    Some(m) => format!("After {m} minutes"),
  }
}

fn retention_title(r: Retention) -> String {
  match r {
    Retention::Unlimited => "Forever".into(),
    Retention::Count(n) => format!("The last {} dictations", thousands(n)),
    Retention::Days3 => "3 days".into(),
    Retention::Weeks2 => "2 weeks".into(),
    Retention::Months3 => "3 months".into(),
  }
}

impl Prefs {
  /// The microphone choices: the system default, the devices, and the saved one when it is not
  /// connected; with the index of the current one.
  fn microphones(&self) -> (Vec<(String, Option<Choice>)>, usize) {
    let default = match self.devices.iter().find(|d| d.default) {
      Some(d) => format!("System Default ({})", d.name),
      None => "System Default".to_owned(),
    };
    let mut items = vec![(default, None)];
    for d in &self.devices {
      items.push((d.name.clone(), Some(Choice { id: d.id.clone(), name: d.name.clone() })));
    }

    let Some(saved) = &self.settings.microphone else { return (items, 0) };
    let by_id = self.devices.iter().position(|d| saved.id.is_some() && d.id == saved.id);
    let found = by_id.or_else(|| self.devices.iter().position(|d| d.name == saved.name));
    match found {
      Some(i) => (items, i + 1),
      None => {
        items.push((format!("{} (not connected)", saved.name), Some(saved.clone())));
        let at = items.len() - 1;
        (items, at)
      }
    }
  }

  /// The model choices: the downloaded models by name, the chosen one when it is not one of
  /// them (gone from the disk, a file of its own), then "Download more…", the last; with the
  /// index of the current one.
  pub(super) fn model_choices(&self) -> (Vec<String>, usize) {
    let mut items: Vec<String> = self.found.iter().map(|f| f.0.full_name.to_owned()).collect();
    let other = match (&self.pick.other, self.pick.active) {
      (Some((name, true)), _) => Some(name.clone()),
      (Some((name, false)), _) => Some(format!("{name} (not on disk)")),
      (None, None) => Some("No model".to_owned()),
      (None, Some(_)) => None,
    };
    let at = match other {
      Some(label) => {
        items.push(label);
        items.len() - 1
      }
      None => self.pick.active.unwrap_or_default(),
    };
    items.push(MORE_MODELS.into());
    (items, at)
  }

  /// The model row's pop-up picked item `i` of [`Prefs::model_choices`]: a model to use, or the
  /// Models page.
  fn model_picked(&mut self, i: usize, items: usize) {
    if i + 1 == items {
      self.page = Page::Models;
      return;
    }

    if let Some(&(m, _)) = self.found.get(i) {
      self.use_model(m.name);
    }
  }

  /// The language choices: auto, then the languages the active model is meant for, by name;
  /// with the index of the current one.
  fn languages(&self) -> (Vec<(String, String)>, usize) {
    let codes = self.active().map_or(&[][..], |f| f.0.langs);
    let mut langs: Vec<(String, String)> =
      codes.iter().map(|c| (language_name(c), (*c).to_owned())).collect();
    langs.sort();
    let mut items = vec![("Auto-detect".to_owned(), "auto".to_owned())];
    items.extend(langs);
    let Some(current) = self.settings.engine_language() else { return (items, 0) };

    if let Some(i) = items.iter().position(|(_, c)| c.eq_ignore_ascii_case(&current)) {
      return (items, i);
    }

    let label = format!("{} (not one the model is meant for)", language_name(&current));
    items.push((label, current));
    let at = items.len() - 1;
    (items, at)
  }

  pub(super) fn general(&mut self, ui: &mut Ui, fs: &FormStyle) {
    self.missing(ui, fs);
    ui.form_section(fs, "Dictation", |ui| {
      self.model(ui, fs);
      let mut row = FormRow::new("Shortcut");
      if let Some(note) = self.shortcut_note() {
        row = row.info(note);
      }

      ui.with_id(self::row(HOTKEY)).form_row(fs, row, |ui| self.shortcut(ui));

      let row = FormRow::new("Activation").info(ACTIVATION_HELP);
      ui.with_id(self::row(ACTIVATION)).form_row(fs, row, |ui| {
        let mut cur = self.settings.activation;
        if ui.id(ACTIVATION).select(&mut cur, &ACTIVATIONS).changed {
          self.edit(move |s| s.activation = cur);
        }
      });

      let (mics, at) = self.microphones();
      let labels: Vec<String> = mics.iter().map(|(l, _)| l.clone()).collect();
      ui.form_row(fs, FormRow::new("Microphone"), |ui| {
        if let Some(i) = pick(ui, MIC, &labels, at, &[0]) {
          let choice = mics[i].1.clone();
          self.edit(move |s| s.microphone = choice.clone());
        }
      });
    });

    self.output(ui, fs);
  }

  /// What the shortcut's ⓘ says, when it applies: Secure Input while it is on, else what macOS
  /// does with the fn key too.
  pub(super) fn shortcut_note(&self) -> Option<&'static str> {
    match (self.access.secure_input, self.settings.hotkey == "fn") {
      (true, _) => Some(SECURE_INPUT_NOTE),
      (false, true) => Some(FN_NOTE),
      (false, false) => None,
    }
  }

  /// The recorder and its reset button. A cleared value keeps the hotkey: the app needs one.
  fn shortcut(&mut self, ui: &mut Ui) {
    let mut hotkey = Hotkey::parse(&self.settings.hotkey).ok();
    if ui.id(HOTKEY).shortcut(&mut hotkey).changed
      && let Some(hotkey) = hotkey
    {
      let hotkey = hotkey.to_string();
      info!("settings: hotkey {hotkey}");
      self.edit(move |s| s.hotkey = hotkey.clone());
    }

    let recording = records(ui);
    if recording != self.recording {
      self.recording = recording;
      self.shell.pause_hotkey(recording);
    }

    let default = Settings::default().hotkey;
    let at_default = Hotkey::parse(&self.settings.hotkey).ok() == Hotkey::parse(&default).ok();
    ui.disabled(at_default);
    let r = ui.id(RESET).icon_button(Icon::RESET);
    ui.tooltip(r.id, "Back to the default shortcut");
    if r.clicked {
      self.edit(move |s| s.hotkey = default.clone());
    }
  }

  /// The switch flips at once; the system is asked off the main thread and the answer comes to
  /// [`Prefs::autostart_set`], which turns it back if the change failed.
  fn login(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let on = self.login.shown(self.settings.autostart);
    let note = self.login_note();
    let mut row = FormRow::new("Launch at login").disabled(self.login.state.is_err());
    if let Some(note) = &note {
      row = row.info(note);
    }

    ui.with_id(self::row(LOGIN)).form_row(fs, row, |ui| {
      let mut want = on;
      if ui.id(LOGIN).toggle(&mut want, "").changed {
        self.ask_autostart(want);
      }
    });
  }

  /// What launch at login's ⓘ says, when it has something to: why the last change failed, that
  /// the system waits for the person's approval, or why it is unavailable.
  pub(super) fn login_note(&self) -> Option<String> {
    let login = &self.login;
    match (&login.error, &login.state) {
      (Some(e), _) => Some(e.clone()),
      (None, Ok(Autostart::RequiresApproval)) if login.busy.is_none() => {
        Some("Allow Wavo in System Settings › General › Login Items.".to_owned())
      }
      (None, Err(why)) if !why.is_empty() => Some(sentence(why)),
      _ => None,
    }
  }

  /// Sets launch at login to `on` through the shell, or once the change on its way ends.
  fn ask_autostart(&mut self, on: bool) {
    self.login.error = None;
    if self.login.busy.is_some() {
      self.login.next = Some(on);
      return;
    }

    self.login.busy = Some(on);
    self.shell.set_autostart(on);
  }

  /// [`super::Shell::set_autostart`] to `on` ended: the system's state after it, or why not (the
  /// switch goes back and says so in its row's ⓘ). One asked for meanwhile goes next.
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

    if let Some(next) = self.login.next.take()
      && next != self.login.shown(self.settings.autostart)
    {
      self.ask_autostart(next);
    }
  }

  /// How the text goes out: pasted, and kept on the clipboard (with why it may not come back in
  /// the ⓘ).
  fn output(&mut self, ui: &mut Ui, fs: &FormStyle) {
    ui.form_section(fs, "Output", |ui| {
      let row = FormRow::new("Paste the text into the app you dictate in");
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.auto_paste;
        if ui.id(PASTE).toggle(&mut on, "").changed {
          self.edit(move |s| s.auto_paste = on);
        }
      });
      // Without pasting the text only goes to the clipboard, and stays there.
      let settings = &self.settings;
      let mut row = FormRow::new("Keep the text on the clipboard").disabled(!settings.auto_paste);
      if settings.restores_clipboard() && self.access.clipboard_unread {
        row = row.info(CLIPBOARD_NOTE);
      }

      ui.with_id(self::row(KEEP)).form_row(fs, row, |ui| {
        let mut on = self.settings.keep_in_clipboard;
        if ui.id(KEEP).toggle(&mut on, "").changed {
          self.edit(move |s| s.keep_in_clipboard = on);
        }
      });
    });
  }

  /// The recordings' size, asked of the shell the first time the page shows after a refresh;
  /// `None` until it answers.
  fn recordings_size(&mut self, ui: &Ui) -> Option<u64> {
    let folder = &mut self.recordings;
    if !folder.asked {
      folder.asked = true;
      let (slot, waker) = (folder.size.clone(), ui.waker().clone());
      let done = move |bytes| {
        *slot.lock().unwrap() = Some(bytes);
        waker.wake();
      };
      self.shell.folder_size(folder.dir.clone(), Box::new(done));
    }

    *self.recordings.size.lock().unwrap()
  }

  pub(super) fn advanced(&mut self, ui: &mut Ui, fs: &FormStyle) {
    ui.form_section(fs, "App", |ui| {
      self.login(ui, fs);
      ui.form_row(fs, FormRow::new("Show the HUD at"), |ui| {
        let mut cur = self.settings.hud_position;
        if ui.id(HUD).select(&mut cur, &HUDS).changed {
          self.edit(move |s| s.hud_position = cur);
        }
      });

      let mut unloads = UNLOADS.to_vec();
      if !unloads.contains(&self.settings.unload_after_minutes) {
        unloads.push(self.settings.unload_after_minutes);
      }

      let labels: Vec<String> = unloads.iter().map(|&u| unload_title(u)).collect();
      let row = FormRow::new("Unload the model").info(UNLOAD_HELP);
      ui.with_id(self::row(UNLOAD)).form_row(fs, row, |ui| {
        let current = self.settings.unload_after_minutes;
        let at = unloads.iter().position(|&u| u == current).unwrap_or(0);
        if let Some(i) = pick(ui, UNLOAD, &labels, at, &[0]) {
          let minutes = unloads[i];
          self.edit(move |s| s.unload_after_minutes = minutes);
        }
      });

      let mut all = RETENTIONS.to_vec();
      if !all.contains(&self.settings.history_retention) {
        all.insert(1, self.settings.history_retention);
      }

      let labels: Vec<String> = all.iter().map(|&r| retention_title(r)).collect();
      ui.form_row(fs, FormRow::new("Keep the history"), |ui| {
        let at = all.iter().position(|&r| r == self.settings.history_retention).unwrap_or(0);
        if let Some(i) = pick(ui, RETENTION, &labels, at, &[]) {
          let r = all[i];
          self.edit(move |s| s.history_retention = r);
        }
      });
    });

    let dir = self.hub.root().to_owned();
    let size: u64 = self.found.iter().map(|f| f.0.size).sum();
    let caption = folder_caption(&dir, Some(size));
    let recordings = self.recordings.dir.clone();
    let recorded = folder_caption(&recordings, self.recordings_size(ui));
    let show = format!("Show in {}", open::file_manager());
    let mut reveal = None;
    // A path stays on one line, ending in "…": a long one has no space to wrap at.
    let fs = &FormStyle { caption: fs.caption.ellipsis(), ..*fs };
    ui.form_section(fs, "Files", |ui| {
      let row = FormRow::new("Model folder").caption(&caption).tile(Tile::Icon(Icon::FOLDER));
      ui.form_row(fs, row, |ui| {
        let r = ui.disabled(!dir.is_dir()).id(MODEL_FOLDER).button(&show);
        ui.tooltip(r.id, "The Hugging Face cache, which other apps may share");
        if r.clicked {
          reveal = Some(&dir);
        }
      });
      let row = FormRow::new("Recordings").caption(&recorded).tile(Tile::Icon(Icon::FOLDER));
      ui.form_row(fs, row, |ui| {
        let r = ui.disabled(!recordings.is_dir()).id(RECORDINGS_FOLDER).button(&show);
        ui.tooltip(r.id, "The history’s recordings, a folder a day");
        if r.clicked {
          reveal = Some(&recordings);
        }
      });
    });
    if let Some(dir) = reveal {
      self.shell.reveal(dir);
    }

    self.data(ui, fs);
  }

  /// While the system does not allow them, the microphone's and Accessibility's rows, each with
  /// why in its ⓘ and its button; nothing once both are allowed.
  fn missing(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let access = self.access;
    let mic = access.microphone != Grant::Granted;
    let title = match (mic, !access.accessibility) {
      (false, false) => return,
      (true, true) => "Wavo needs two permissions",
      _ => "Wavo needs a permission",
    };
    let fs = &FormStyle { tile_icon: ui.theme().warning, ..*fs };
    ui.form_section(fs, title, |ui| {
      if mic {
        let row = FormRow::new("Microphone").info(MICROPHONE_WHY).tile(Tile::Icon(WARNING));
        // A denied microphone is not asked again: only System Settings changes it.
        ui.with_id(self::row(OPEN_MICROPHONE)).form_row(fs, row, |ui| {
          let ask = access.microphone == Grant::NotDetermined;
          self.permission(ui, ask, OPEN_MICROPHONE, Pane::Microphone)
        });
      }

      if !access.accessibility {
        let row = FormRow::new("Accessibility").info(ACCESSIBILITY_WHY).tile(Tile::Icon(WARNING));
        ui.with_id(self::row(OPEN_ACCESSIBILITY)).form_row(fs, row, |ui| {
          let ask = !access.accessibility_asked;
          self.permission(ui, ask, OPEN_ACCESSIBILITY, Pane::Accessibility)
        });
      }
    });
  }

  /// A missing permission's button, [`super::Shell::allow`]: Allow while the system can still
  /// ask, else Open Settings.
  fn permission(&mut self, ui: &mut Ui, ask: bool, id: &str, pane: Pane) {
    let button = if ask { Button::new("Allow").primary() } else { Button::new("Open Settings") };
    if button.show(ui.id(id)).clicked {
      self.shell.allow(pane);
    }
  }

  /// The model and the language rows; with no model downloaded, the way to download one. A model
  /// that can't transcribe as chosen says why in the row's ⓘ.
  fn model(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let (labels, at) = self.model_choices();
    let problem = self.model_problem();
    let none = problem == Some(Problem::NoneDownloaded);
    let note = problem.as_ref().filter(|_| !none).map(Problem::text);
    let mut row = FormRow::new("Model");
    if let Some(note) = &note {
      row = row.info(note);
    }

    let mut picked = None;
    ui.with_id(self::row(MODEL)).form_row(fs, row, |ui| {
      if !none {
        picked = pick(ui, MODEL, &labels, at, &[labels.len() - 2]);
      } else if Button::new("Download a Model…").primary().show(ui.id(DOWNLOAD_MODEL)).clicked {
        self.page = Page::Models;
      }
    });
    if let Some(i) = picked {
      self.model_picked(i, labels.len());
    }

    let (langs, at) = self.languages();
    let labels: Vec<String> = langs.iter().map(|(l, _)| l.clone()).collect();
    ui.form_row(fs, FormRow::new("Language"), |ui| {
      if let Some(i) = pick(ui, LANG, &labels, at, &[0]) {
        let code = langs[i].1.clone();
        self.edit(move |s| s.language = code.clone());
      }
    });
  }
}

/// `launch at login needs Wavo.app` as a sentence.
fn sentence(text: &str) -> String {
  let mut chars = text.chars();
  match chars.next() {
    Some(first) => first.to_uppercase().chain(chars).collect::<String>() + ".",
    None => String::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn titles_of_the_choices() {
    assert_eq!(thousands(10_000), "10,000");
    assert_eq!(thousands(100), "100");
    assert_eq!(retention_title(Retention::Count(1000)), "The last 1,000 dictations");
    assert_eq!(unload_title(Some(1)), "After 1 minute");
    assert_eq!(sentence("needs Wavo.app"), "Needs Wavo.app.");
  }
}
