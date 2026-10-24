// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The settings window on nimbo, as macOS System Settings looks: toolbar tabs (General, Models,
//! History, Advanced, About) over pages of grey plates, in a window [`WIDTH`] wide. General starts
//! with what the app needs to work, the first launch's guide: the permissions, then the model (or
//! the way to download one). General, Advanced and About are as high as their content
//! ([`Ui::fit_window_height`]); Models and History scroll in a window [`SCROLLING`] high, under
//! their search row. Light or dark and in the accent the system has.
//!
//! The window is a plain [`App`]: it shows a copy of the settings and the session's status,
//! which the shell keeps current ([`Prefs::update`]), and reaches the rest of the app only
//! through [`Shell`], so tests run it in nimbo's harness with a fake one. A change applies here
//! at once and goes to the session ([`Shell::edit`]), which saves it and sends it back with its
//! next update. Deleting a model or a dictation asks first in a modal dialog over the window
//! ([`Ask`]).
//!
//! The models on disk are looked at again when the window opens, a page is switched to, a
//! download ends or a model is deleted, and when the model in use is gone from the disk behind
//! its back; then the session looks too ([`Shell::check_model`]). A model that can't transcribe
//! says so where it shows ([`Prefs::model_problem`]: General's Model row, the Models page,
//! History's Re-transcribe).

mod about;
pub mod data;
pub mod downloads;
mod general;
pub mod history;
pub mod models;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use nimbo::autostart::State as Autostart;
use nimbo::permissions::{Access as Grant, Pane};
use nimbo::{Answer, App, Color, Config, Dialog, FormStyle, Icon, Style, Ui, grow};
use wavo::hub::{self, Hub, ModelInfo};

use self::downloads::Download;
use self::models::{Found, Pick};
use crate::audio::devices;
use crate::session::controller::{NoModel, Status};
use crate::settings::Settings;

/// The window's width.
pub const WIDTH: f32 = 680.0;
/// The height of the pages that scroll; the others are as high as their content, between
/// [`LOWEST`] and [`HIGHEST`].
pub const SCROLLING: f32 = 600.0;
const LOWEST: f32 = 200.0;
const HIGHEST: f32 = 800.0;

/// The toolbar of tabs, `TABS.key(i)` a tab.
pub const TABS: &str = "tabs";
/// The dialog that asks before a delete: `CONFIRM.key("cancel")` and `CONFIRM.key("action")`
/// its buttons.
pub const CONFIRM: &str = "confirm";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Page {
  General,
  Models,
  History,
  Advanced,
  About,
}

pub const PAGES: [(Page, Icon, &str); 5] = [
  (Page::General, Icon::GEAR, "General"),
  (Page::Models, Icon::DOWNLOAD, "Models"),
  (Page::History, Icon::HISTORY, "History"),
  (Page::Advanced, Icon::SLIDERS, "Advanced"),
  (Page::About, Icon::INFO, "About"),
];

impl Page {
  /// Its content scrolls in a window [`SCROLLING`] high.
  pub fn scrolls(self) -> bool {
    matches!(self, Self::Models | Self::History)
  }
}

/// The window opening on `page`: its title, not resizable; its height follows the page from the
/// first frame.
pub fn config(page: Page) -> Config {
  let h = if page.scrolls() { SCROLLING } else { 500.0 };
  Config::new("Wavo", WIDTH, h).fixed()
}

/// What the system allows the app, as the General page shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
  /// Accessibility: the event tap and the paste.
  pub accessibility: bool,
  pub microphone: Grant,
  /// Another app holds Secure Input: the hotkey goes through its backup path.
  pub secure_input: bool,
  /// The paste can't read the clipboard to put it back (Paste from Other Apps).
  pub clipboard_unread: bool,
}

impl Default for Access {
  /// Nothing allowed, nothing asked.
  fn default() -> Self {
    let microphone = Grant::NotDetermined;
    Self { accessibility: false, microphone, secure_input: false, clipboard_unread: false }
  }
}

/// A delete the dialog asks about ([`CONFIRM`]).
#[derive(Clone, Debug)]
pub enum Ask {
  /// A model, and when it is the one in use, what takes its place: the first other one
  /// downloaded, if any.
  Model { model: &'static ModelInfo, in_use: bool, next: Option<&'static str> },
  /// A history entry: its id, the start of its text, whether it has a recording.
  Dictation { id: i64, start: String, audio: bool },
}

impl Ask {
  /// The dialog's title and message.
  pub fn question(&self) -> (String, String) {
    match self {
      Ask::Model { model, in_use, next } => {
        let title = format!("Delete {}?", model.full_name);
        let size = models::size_text(model.size);
        let then = match (in_use, next) {
          (false, _) => "You can download it again.".to_owned(),
          (true, Some(next)) => format!("It is in use: {next} takes its place."),
          (true, None) => {
            "It is the only model: dictation stops until you download another.".into()
          }
        };
        (title, format!("It frees {size} in the Hugging Face cache. {then}"))
      }
      Ask::Dictation { start, audio, .. } => {
        let and = if *audio { " and its recording are" } else { " is" };
        ("Delete this dictation?".into(), format!("“{start}”{and} removed from the history."))
      }
    }
  }
}

/// What the window asks of the app. The macOS shell's goes through the main thread's proxy for
/// whatever needs the app's state, so a call never re-enters the shell from inside the window;
/// what may wait runs on a thread of its own.
pub trait Shell {
  /// Changes the settings in the session, which saves them and uses them from now on.
  fn edit(&self, edit: Box<dyn FnOnce(&mut Settings) + Send>);
  /// The recorder records a new hotkey: the current one is not matched meanwhile.
  fn pause_hotkey(&self, paused: bool);
  /// The input devices, the system default marked.
  fn devices(&self) -> Vec<devices::Info>;
  /// Downloads `model` ([`downloads::Downloads::pull`]); a download that ends tells the window
  /// ([`Prefs::models_changed`]).
  fn download(&self, model: &'static ModelInfo);
  /// Stops the download of model `name`, or forgets its failure.
  fn cancel_download(&self, name: &str);
  fn download_state(&self, name: &str) -> Option<Download>;
  /// Deletes `model` from the cache off the main thread ([`models::delete`]); the shell then
  /// hands how it went to [`Prefs::model_deleted`] on the main thread.
  fn delete_model(&self, model: &'static ModelInfo);
  /// The models on disk changed, or may have: the session looks whether one can transcribe.
  fn check_model(&self);
  /// Transcribes a history entry's recording again (the session does, off the main thread).
  fn retranscribe(&self, id: i64);
  /// Puts back the text a history entry's last transcription replaced (the session does, off the
  /// main thread).
  fn undo_transcribe(&self, id: i64);
  /// Deletes a history entry and its recording (the session does, off the main thread); a
  /// failure goes to [`Prefs::entry_not_deleted`] on the main thread.
  fn delete_entry(&self, id: i64);
  /// Launch at login as the system has it; `Err` with the reason when unavailable.
  fn autostart(&self) -> Result<Autostart, String>;
  /// Turns launch at login on or off off the main thread (the system takes a while to answer),
  /// and on success saves the choice in the settings; the shell then hands the state after, or
  /// why not, to [`Prefs::autostart_set`] on the main thread.
  fn set_autostart(&self, on: bool);
  fn access(&self) -> Access;
  /// Adds up the sizes of the files in `dir` off the main thread (a long history has many) and
  /// hands the bytes to `done`, on that thread.
  fn folder_size(&self, dir: PathBuf, done: Box<dyn FnOnce(u64) + Send>);
  /// Allow… for `pane`: the system's prompt while it can still ask (its answer goes to
  /// [`Prefs::access_changed`]), else its pane in System Settings. This and `open_url` open their
  /// target without waiting for it (the system may start an app for it).
  fn allow(&self, pane: Pane);
  fn open_url(&self, url: &str);
  /// Shows `dir` in the system's file manager.
  fn reveal(&self, dir: &Path);
  /// Handy's history on this computer, if any ([`crate::transfer::handy::found`], which takes a
  /// while), handed to `done` on a thread of its own.
  fn handy(&self, done: Box<dyn FnOnce(Option<crate::transfer::handy::Summary>) + Send>);
  /// Imports Handy's history in `dir` into the app's on a thread of its own; `tell` hears how it
  /// goes.
  fn import_handy(&self, dir: PathBuf, tell: Box<dyn Fn(data::Import) + Send>);
  /// Writes the history into the archive `file` on a thread of its own and shows it in the file
  /// manager; `done` hears how it went.
  fn export(
    &self,
    file: PathBuf,
    done: Box<dyn FnOnce(Result<crate::transfer::archive::Exported, String>) + Send>,
  );
}

/// `fs` with the descriptions under the labels in `color`: a row's warning or error.
fn caption_in(fs: &FormStyle, color: Color) -> FormStyle {
  FormStyle { caption: fs.caption.color(color), ..*fs }
}

/// The bytes of the files in `dir` and its folders, without following links; what can't be read
/// counts nothing. For [`Shell::folder_size`], on a thread of its own.
pub fn folder_size(dir: &Path) -> u64 {
  let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
  let mut size = 0;
  for e in entries.flatten() {
    match e.file_type() {
      Ok(t) if t.is_dir() => size += folder_size(&e.path()),
      Ok(t) if t.is_file() => size += e.metadata().map_or(0, |m| m.len()),
      _ => {}
    }
  }

  size
}

/// Why no model transcribes as chosen, as the window says it.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
  /// None is downloaded.
  NoneDownloaded,
  /// The chosen model is not on disk: another one transcribes instead, or none.
  Gone { chosen: String, instead: Option<&'static str> },
  /// The model is there but does not load.
  LoadFailed { model: String, error: String },
}

impl Problem {
  /// What General's Model row and the Models page say.
  pub fn text(&self) -> String {
    match self {
      Problem::NoneDownloaded => "Dictation needs a speech model: download one to start.".into(),
      Problem::Gone { chosen, instead: Some(instead) } => {
        format!("{chosen} is not on disk: {instead} transcribes instead.")
      }
      Problem::Gone { chosen, instead: None } => {
        format!("{chosen} is not on disk: download it again in Models, or pick another.")
      }
      Problem::LoadFailed { model, error } => format!("{model} could not be loaded: {error}."),
    }
  }

  /// Nothing transcribes: re-transcribing waits.
  pub fn stops(&self) -> bool {
    matches!(self, Problem::NoneDownloaded | Problem::Gone { instead: None, .. })
  }
}

/// The settings window's app.
pub struct Prefs {
  pub page: Page,
  /// The page the last frame showed: another one looks at the models again.
  shown: Page,
  /// The session's settings, with the window's changes applied at once.
  pub settings: Settings,
  status: Option<Status>,
  shell: Box<dyn Shell>,
  hub: Hub,
  devices: Vec<devices::Info>,
  found: Vec<Found>,
  /// Which of them transcribes: looked at again when they or the choice change.
  pick: Pick,
  login: general::Login,
  access: Access,
  recordings: general::Folder,
  /// The shortcut recorder records: the hotkey is paused.
  recording: bool,
  /// The window had the keys at the last frame.
  focused: bool,
  /// The delete being asked about.
  pub ask: Option<Ask>,
  models: models::State,
  history: history::State,
  data: data::State,
}

impl Prefs {
  /// The window over `settings` (the session's latest), the models in `hub`, the history in
  /// `home`.
  pub fn new(shell: Box<dyn Shell>, settings: Settings, hub: Hub, home: &Path) -> Self {
    let mut prefs = Self {
      page: Page::General,
      shown: Page::General,
      settings,
      status: None,
      shell,
      hub,
      devices: Vec::new(),
      found: Vec::new(),
      pick: Pick::default(),
      login: general::Login::default(),
      access: Access::default(),
      recordings: general::Folder::new(home.join(crate::history::recordings::DIR)),
      recording: false,
      focused: false,
      ask: None,
      models: models::State::default(),
      history: history::State::new(home),
      data: data::State::default(),
    };
    prefs.refresh();
    prefs
  }

  /// The session changed: its settings, status and history.
  pub fn update(&mut self, settings: &Settings, status: &Status, history: u64) {
    let model = std::mem::replace(&mut self.settings, settings.clone()).model;
    if model != self.settings.model {
      self.repick();
    }

    self.status = Some(status.clone());
    self.history.revision = history;
  }

  /// Looks again at what the session does not push: devices, models, permissions, launch at
  /// login, the recordings' size.
  pub fn refresh(&mut self) {
    self.devices = self.shell.devices();
    self.models_changed();
    self.access_changed();
    self.login.state = self.shell.autostart();
    self.recordings.asked = false;
    self.data.asked = false;
  }

  /// What the system allows may have changed: a prompt was answered, the person is back from
  /// System Settings.
  pub fn access_changed(&mut self) {
    self.access = self.shell.access();
  }

  /// Models came or went (a download ended, one was deleted), or may have: the window looks
  /// again, and the session too when they did.
  pub fn models_changed(&mut self) {
    let found = models::ready(self.hub.downloaded(), |name| self.shell.download_state(name));
    if found != self.found {
      self.found = found;
      self.shell.check_model();
    }

    self.repick();
  }

  /// Which model transcribes, after the models on disk or the choice changed.
  fn repick(&mut self) {
    self.pick = Pick::new(&self.hub, self.settings.model.as_deref(), &self.found);
  }

  /// [`Shell::delete_model`] of `model` ended: the bytes freed, or why not. A choice of the
  /// model goes back to the first one left, as the shell told the session ([`models::forget`]).
  pub fn model_deleted(&mut self, model: &'static ModelInfo, result: Result<u64, String>) {
    self.models.deleting.retain(|n| *n != model.name);
    match result {
      Ok(_) => {
        models::forget(&mut self.settings, &self.hub, model);
        self.models.note = None;
      }
      Err(e) => self.models.note = Some(format!("Couldn't delete {}: {e}", model.full_name)),
    }

    self.models_changed();
  }

  /// Changes the settings here and in the session.
  fn edit(&mut self, edit: impl Fn(&mut Settings) + Send + 'static) {
    let model = self.settings.model.clone();
    edit(&mut self.settings);
    if self.settings.model != model {
      self.repick();
    }

    self.shell.edit(Box::new(move |s| edit(s)));
  }

  /// The model that transcribes, as the session picks it ([`Pick`]).
  fn active(&self) -> Option<&Found> {
    self.found.get(self.pick.active?)
  }

  /// Why no model transcribes as chosen, as the window sees it: the chosen one gone from the
  /// disk, else none downloaded, else what the session says (a model that does not load).
  pub fn model_problem(&self) -> Option<Problem> {
    if let Some(chosen) = self.pick.gone() {
      let instead = self.active().map(|f| f.0.full_name);
      return Some(Problem::Gone { chosen: chosen.to_owned(), instead });
    }

    if self.found.is_empty() && self.pick.other.is_none() {
      return Some(Problem::NoneDownloaded);
    }

    match self.status.as_ref().and_then(|s| s.no_model.clone()) {
      Some(NoModel::LoadFailed { model, error }) => Some(Problem::LoadFailed { model, error }),
      _ => None,
    }
  }

  /// The dialog asking before a delete, while one is asked about; the delete once it says so.
  fn confirm(&mut self, ui: &mut Ui) {
    let Some(ask) = &self.ask else { return };
    let (title, message) = ask.question();
    let mut open = true;
    let d = Dialog::new(&title).message(&message).destructive("Delete");
    let answer = d.show(ui.id(CONFIRM), &mut open);
    if open {
      return;
    }

    match (self.ask.take(), answer) {
      (Some(Ask::Model { model, .. }), Some(Answer::Action)) => self.delete_model(model),
      (Some(Ask::Dictation { id, .. }), Some(Answer::Action)) => self.delete_entry(id),
      _ => {}
    }
  }

  /// The page's search row above its scrolling content, when it has one.
  fn head(&mut self, ui: &mut Ui) {
    let s = Style::new().w(grow()).pads([12.0, 20.0, 2.0, 20.0]).gap(10.0);
    match self.page {
      Page::Models => self.models_head(ui, s),
      Page::History => self.history_head(ui, s),
      _ => {}
    }
  }

  /// The page under the toolbar (and its search row), in its scroll area: as far from the
  /// window's bottom as from its sides.
  fn page(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let page = match self.page {
      Page::About => {
        self.about_head(ui, fs);
        fs.page.pads([20.0, 20.0, 20.0, 20.0])
      }
      _ => fs.page.pads([12.0, 20.0, 20.0, 20.0]),
    };
    ui.col(page, |ui| match self.page {
      Page::General => self.general(ui, fs),
      Page::Models => self.models(ui, fs),
      Page::History => self.history(ui, fs),
      Page::Advanced => self.advanced(ui, fs),
      Page::About => self.about(ui, fs),
    });
  }
}

impl App for Prefs {
  fn view(&mut self, ui: &mut Ui) {
    // Another page, or the model in use gone from the disk behind the window's back: the models
    // are looked at again.
    if self.page != self.shown || self.active().is_some_and(|f| !f.1.is_file()) {
      self.shown = self.page;
      self.models_changed();
    }

    match self.page.scrolls() {
      true => ui.set_window_size(WIDTH, SCROLLING),
      false => ui.fit_window_height(WIDTH, LOWEST, HIGHEST),
    }

    let fs = FormStyle::plates(&ui.theme());
    ui.col(Style::new().grow().bg(fs.bg), |ui| {
      ui.id(TABS).toolbar_tabs(&fs, &mut self.page, &PAGES);
      self.head(ui);
      ui.id(("page", self.page as u8)).form_scroll(&fs, Style::new().grow(), |ui| {
        ui.col(Style::new().w(grow()), |ui| self.page(ui, &fs));
      });
    });
    self.confirm(ui);
    // The recorder lost its focus with the page, or the window lost the keys: the hotkey comes
    // back.
    if self.recording && !general::records(ui) {
      self.recording = false;
      self.shell.pause_hotkey(false);
    }

    // Back from another app (System Settings, where a permission was given): what the system
    // allows is looked at again.
    let focused = ui.window_focused();
    if focused && !self.focused {
      self.access_changed();
    }

    self.focused = focused;
  }
}

/// The models wavo knows by name, for a choice that names one: `hub::lookup`.
fn lookup(name: &str) -> Option<&'static ModelInfo> {
  hub::lookup(name).ok()
}
