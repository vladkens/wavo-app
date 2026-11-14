// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The settings window, as macOS System Settings looks: toolbar tabs over pages of grey plates.
//! General, Advanced and About are as high as their content; Models and History scroll under
//! their search row. The window shows a copy of the settings and the session's status, which the
//! shell keeps current ([Prefs::update]), and reaches the app only through [Shell]: tests run it
//! in nimbo's harness with a fake one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use nimbo::autostart::State as Autostart;
use nimbo::permissions::{Access as Grant, Pane};
use nimbo::system::Change;
use nimbo::ui::Role;
use nimbo::ui::widgets::icon;
use nimbo::{
  Align, Answer, App, Button, ButtonLook, ButtonRole, ButtonStyle, Config, ControlSize, Cx, Dialog,
  Event, FileDialog, Font, FormRow, FormStyle, Frame, Hotkey, Icon, Id, Item, LinePattern, List,
  ListStyle, Menu, Picker, Rect, Response, Section, Selection, Style, TabItem, Tag, TextStyle,
  Tone, Ui, Weight, grow, open, paths, px,
};
use tracing::{info, warn};
use wavo::hub::{self, Hub, ModelInfo};

use crate::audio::{self, Auto, Choice, Microphone, Playback, Transport};
use crate::history::{
  Entry, History, IN_MEMORY, Local, RECORDINGS_DIR, Retention, fold, local_time, now_ms,
};
use crate::session::{self, NoModel, Request, Status, label};
use crate::settings::{Activation, HudPosition, HudStyle, KEYBOARD, Settings, language_name};
use crate::transfer::Imported;
use crate::{engine, mic_start};

// MARK: Window

pub const WIDTH: f32 = 560.0;
/// The model to start with, wavo's own default: first in Available, its Download the blue one.
const RECOMMENDED: &str = "parakeet-ultra";
/// The height of the pages that scroll.
pub const SCROLLING: f32 = 600.0;

const TABS: &str = "tabs";
const CONFIRM: &str = "confirm";
/// The row of the Models and History pages' search, over the page.
const PAGE_HEAD: &str = "page-head";
/// A page's padding in the window, the History list's too.
const PAGE_PAD: [f32; 4] = [12.0, 20.0, 20.0, 20.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
  General,
  Models,
  History,
  Advanced,
  About,
}

const PAGES: [TabItem<Page>; 5] = [
  TabItem::new(Page::General, Icon::GEAR, "General"),
  TabItem::new(Page::Models, Icon::DOWNLOAD, "Models"),
  TabItem::new(Page::History, Icon::HISTORY, "History"),
  TabItem::new(Page::Advanced, Icon::SLIDERS, "Advanced"),
  TabItem::new(Page::About, Icon::INFO, "About"),
];

impl Page {
  fn scrolls(self) -> bool {
    matches!(self, Self::Models | Self::History)
  }
}

/// A macOS preferences window: the title over the tabs, under one background.
pub fn config(page: Page) -> Config {
  let h = if page.scrolls() { SCROLLING } else { 500.0 };
  crate::with_icon(Config::new(crate::INFO.name, WIDTH, h).fixed().unified())
}

/// What the system allows the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
  pub accessibility: bool,
  /// Accessibility is missing and only an administrator can allow it (macOS).
  pub needs_admin: bool,
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

  /// The keyboard layouts the person can switch to that type a language: its code and the
  /// layout's name (`ru`, `Russian – PC`); none where the system doesn't tell.
  fn keyboard_layouts(&self) -> Vec<(String, String)> {
    Vec::new()
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
  /// Plays `file` on the speakers until the [Playback] is dropped: [Prefs::played] once it has
  /// played to its end or could not play.
  fn play(&self, file: &Path) -> anyhow::Result<Playback>;
  /// This Mac has a Neural Engine wavo can use.
  fn has_neural_engine(&self) -> bool {
    false
  }
  /// The app can update itself: not where [shipo::supported] says no, a dev build or Wavo Dev, a
  /// run with its own `WAVO_HOME`, unless shipo is [faking](shipo::faking).
  fn updates(&self) -> bool {
    false
  }
  /// The update's window, with a check when it has nothing else to show.
  fn check_updates(&self) {}
}

enum Delete {
  Models(Vec<&'static ModelInfo>),
  Entries(Vec<i64>),
  /// The history saved on disk, Delete All's.
  Saved,
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
  /// [Shell::keyboard_layouts].
  layouts: Vec<(String, String)>,
  found: Vec<Found>,
  pick: Pick,
  access: Access,
  login: Login,
  /// The shortcut recorder records: the hotkey is paused.
  recording: bool,
  ask: Option<Ask>,
  /// The file panel open: its answer goes to it.
  panel: Option<Panel>,
  model_search: String,
  /// An index of [LANGUAGES].
  model_lang: usize,
  /// The downloaded models selected, keyed by their place in wavo's list.
  model_sel: Selection,
  history: HistoryPage,
  /// How many dictations the history saved on disk has, counted when Advanced shows it; `None`
  /// once it may have changed.
  saved: Option<usize>,
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
    let in_memory =
      history.as_ref().map_or(settings.history_retention.in_memory(), |h| h.in_memory());
    let mut prefs = Self {
      page: Page::General,
      settings,
      status: Status::default(),
      shell,
      hub,
      home: home.to_owned(),
      devices: Vec::new(),
      lid_closed: false,
      layouts: Vec::new(),
      found: Vec::new(),
      pick: Pick::default(),
      access,
      login: Login { state: Err(String::new()), busy: None, error: None },
      recording: false,
      ask: None,
      panel: None,
      model_search: String::new(),
      model_lang: 0,
      model_sel: Selection::default(),
      history: HistoryPage { db: history, in_memory, ..HistoryPage::default() },
      saved: None,
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
    if status.in_memory != self.history.in_memory {
      self.history.reopen(&self.home, status.in_memory);
      self.saved = None;
    }

    // Also when the saved history went while the one in use is in memory.
    if history != self.history.revision {
      self.saved = None;
    }

    self.history.revision = history;
  }

  /// Looks again at what the session does not push: when the window opens or comes to the front.
  pub fn refresh(&mut self) {
    self.devices = self.shell.devices();
    self.lid_closed = self.shell.lid_closed();
    self.layouts = self.shell.keyboard_layouts();
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

  /// wavo's model chosen in the settings, on disk or not.
  fn chosen(&self) -> Option<&'static str> {
    hub::lookup(self.settings.model.as_deref()?).ok().map(|m| m.name)
  }

  /// The model the engine has loaded while a dictation or a transcription again runs: it goes on
  /// with it after a switch to another.
  fn busy(&self, m: &ModelInfo) -> bool {
    let s = &self.status;
    let working = s.state != session::Phase::Idle
      || !s.retranscribing.is_empty()
      || matches!(s.engine.phase, engine::Phase::Loading | engine::Phase::Running);
    let file = m.file();
    working && s.engine.model.as_deref().is_some_and(|l| file.strip_suffix(".gguf") == Some(l))
  }

  /// The model stays on disk: it transcribes now, or is [busy](Self::busy).
  fn in_use(&self, m: &ModelInfo) -> bool {
    self.dictates(m) || self.busy(m)
  }

  /// Dictations go to the model: it is the Model, or a keyboard layout's language's.
  fn dictates(&self, m: &ModelInfo) -> bool {
    let keyboard = self.settings.follows_keyboard().then_some(&self.settings.keyboard_models);
    let row =
      keyboard.is_some_and(|rows| rows.values().flatten().any(|v| model_name(v) == Some(m.name)));
    self.active().is_some_and(|f| f.0.name == m.name) || row
  }

  /// Some model transcribes: the chosen one, or the first downloaded in its place.
  fn transcribes(&self) -> bool {
    self.pick.active.is_some() || self.pick.other.as_ref().is_some_and(|o| o.1)
  }

  /// Why the model does not transcribe as chosen.
  fn model_note(&self) -> Option<String> {
    if let Some(chosen) = self.pick.gone() {
      return Some(match self.active() {
        Some(f) => format!("{chosen} is not on disk: {} transcribes instead.", f.0.name),
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
    let d = Dialog::new(&ask.title).message(&ask.message).button("Delete", ButtonRole::Destructive);
    let answer = d.show(ui.id(CONFIRM), &mut open);
    if open {
      return;
    }

    let ask = self.ask.take().unwrap();
    if !matches!(answer, Some(Answer::Action)) {
      return;
    }

    match ask.delete {
      Delete::Models(models) => {
        // In use since the dialog opened, by a switch from the menu bar, it stays.
        for m in models {
          if self.in_use(m) {
            info!("settings: keep model {}, in use", m.name);
            continue;
          }

          info!("settings: delete model {}", m.name);
          self.shell.delete_model(m);
        }

        self.model_sel.clear();
      }
      Delete::Entries(ids) => self.shell.history(Request::Delete(ids)),
      Delete::Saved => self.shell.history(Request::DeleteSaved),
    }
  }

  /// A click on the page's ground (the margins, the room between its sections, the search's
  /// row) clears its list's selection, as Finder's; the list clears it on its own ground, the
  /// History list's margins and the room between its days. The History panel under the list
  /// keeps it: it shows the selection.
  fn deselect(&mut self) {
    match self.page {
      Page::Models => self.model_sel.clear(),
      Page::History => self.history.selected.clear(),
      _ => {}
    }
  }
}

impl App for Prefs {
  fn view(&mut self, ui: &mut Ui) {
    // The title bar is part of the window's content.
    let bar = ui.titlebar().map_or(0.0, |b| b.height);
    match self.page.scrolls() {
      true => ui.set_window_size(WIDTH, SCROLLING + bar),
      false => ui.fit_window_height(WIDTH, 200.0, 800.0),
    }

    let fs = FormStyle::plates(&ui.theme());
    ui.col(Style::new().grow().bg(fs.bg), |ui| {
      ui.id(TABS).toolbar_tabs(&fs, &mut self.page, &PAGES);
      let head = Style::new().w(grow()).pads([12.0, 20.0, 2.0, 20.0]).gap(10.0);
      let head = head.align(Align::Start, Align::Center);
      let head = match self.page {
        Page::Models => self.models_head(ui, head).clicked,
        Page::History => self.history_head(ui, head).clicked,
        _ => false,
      };

      // History is a list, its own scroll area.
      let page = match self.page {
        Page::History => {
          self.history(ui, &fs);
          false
        }
        page => {
          let r = ui.id(("page", page as u8)).form_scroll(&fs, Style::new().grow(), |ui| {
            ui.col(fs.page.pads(PAGE_PAD), |ui| match page {
              Page::General => self.general(ui, &fs),
              Page::Models => self.models(ui, &fs),
              Page::Advanced => self.advanced(ui, &fs),
              _ => self.about(ui, &fs),
            });
          });
          r.clicked
        }
      };
      if head || page {
        self.deselect();
      }

      if self.page == Page::History {
        self.history_foot(ui, &fs);
        self.history_total(ui, &fs);
      }
    });
    self.confirm(ui);

    // Also when the recorder lost its focus with the page, or the window lost the keys.
    let recording = ui.focused(Id::of("hotkey")) && ui.window_focused();
    if recording != self.recording {
      self.recording = recording;
      self.shell.pause_hotkey(recording);
    }
  }

  /// The file Export's or Import's panel picked; the microphones or the lid changed: read again
  /// for the Microphone row; back in front, from another app (System Settings, where a permission
  /// was given): [Prefs::refresh].
  fn event(&mut self, _cx: &mut Cx, ev: &Event) -> bool {
    match ev {
      Event::Focus(true) => {
        self.refresh();
        true
      }
      Event::System(Change::AudioInputs | Change::DefaultInput) => {
        self.devices = self.shell.devices();
        true
      }
      Event::System(Change::Lid) => {
        self.lid_closed = self.shell.lid_closed();
        true
      }
      Event::System(Change::InputSource) => {
        let layouts = self.shell.keyboard_layouts();
        std::mem::replace(&mut self.layouts, layouts) != self.layouts
      }
      Event::FileDialog(answer) => {
        match (self.panel.take(), answer) {
          // GNOME's save panel adds no extension: Import's *.zip would not show it.
          (Some(Panel::Export), Some(file)) => self.shell.export(with_zip(file)),
          (Some(Panel::Import), Some(file)) => self.shell.import(file.clone()),
          _ => {}
        }

        true
      }
      _ => false,
    }
  }
}

// The microphone's pop-up at its widest, in points.
const MIC_WIDTH: f32 = 320.0;

/// A picker of `labels`, item `at` the current one, a divider after each of `lines`: the item
/// picked, when it is another.
fn pick(ui: &mut Ui, id: &str, labels: &[String], at: usize, lines: &[usize]) -> Option<usize> {
  let mut cur = at;
  picker(&mut cur, labels.iter().map(String::as_str), lines).show(ui.id(id)).changed.then_some(cur)
}

/// The languages of keyboard `layouts` ([Shell::keyboard_layouts]) by name, each with its
/// layouts' names: `("ru", "Russian – PC")`.
fn keyboard_languages(layouts: &[(String, String)]) -> Vec<(String, String)> {
  let mut by: BTreeMap<String, (String, Vec<&str>)> = BTreeMap::new();
  for (lang, layout) in layouts {
    let entry = by.entry(language_name(lang)).or_insert_with(|| (lang.clone(), Vec::new()));
    if !entry.1.contains(&layout.as_str()) {
      entry.1.push(layout);
    }
  }

  by.into_values().map(|(lang, names)| (lang, names.join(", "))).collect()
}

/// wavo's name for a model choice, `None` for a file of the person's own.
fn model_name(choice: &str) -> Option<&'static str> {
  hub::lookup(choice).ok().map(|m| m.name)
}

/// A picker of `labels` by their place, a divider after each of `lines`.
fn picker<'s>(
  cur: &'s mut usize,
  labels: impl Iterator<Item = &'s str>,
  lines: &[usize],
) -> Picker<'s, usize> {
  let mut p = Picker::new(cur);
  for (i, label) in labels.enumerate() {
    p = p.option(i, label);
    if lines.contains(&i) {
      p = p.divider();
    }
  }

  p
}

/// What a row's mark says ([marked]).
enum Mark<'s> {
  None,
  /// An ⓘ: what one line can't explain.
  Info(&'s str),
  /// What is wrong with the setting, in the ⓘ's place.
  Warning(&'s str),
}

/// A row's label: `name`, then its mark with its tip, as `id` when given.
fn marked<'s>(
  name: &'s str,
  mark: Mark<'s>,
  id: Option<Id>,
) -> impl FnOnce(&mut Ui, TextStyle) + 's {
  move |ui: &mut Ui, s: TextStyle| {
    ui.text(name, s);
    let help = |ui: &mut Ui, tip: &str| match id {
      Some(id) => _ = ui.help(tip).with_id(id),
      None => _ = ui.help(tip),
    };
    match mark {
      Mark::None => {}
      Mark::Info(tip) => {
        help(ui, tip);
        ui.info_button();
      }
      Mark::Warning(tip) => {
        help(ui, tip);
        ui.warning_button();
      }
    }
  }
}

// MARK: General

const FN_NOTE: &str =
  "Set System Settings › Keyboard › “Press 🌐 key to” to “Do Nothing”, or macOS acts on it too.";
const SECURE_INPUT_NOTE: &str = "Secure Input is on (a password field, or a terminal’s Secure \
  Keyboard Entry): the shortcut works through a backup path, and Esc may not cancel.";
const ACTIVATION_HELP: &str = "How the hotkey records: while you hold it, from one press to the \
  next, or both (a short tap latches the recording on).";
// What Automatic does here ([audio::automatic]): macOS tells the built-in microphone from a
// headset on its jack, and the lid; Windows tells USB and Bluetooth, and counts its board's sound
// (HDAUDIO), the jack too, as built-in; Linux (ALSA) tells no transport and lists no default.
const MIC_HELP: &str = match () {
  _ if cfg!(target_os = "macos") => {
    "Automatic takes a USB microphone first, then the built-in one while the lid is open, then a \
     wired one, and a Bluetooth headset only when there is no other: it records at call quality \
     and loses the first words. System Default uses whatever the system picks."
  }
  _ if cfg!(windows) => {
    "Automatic takes a USB microphone first, then the computer’s own or a wired one, and a \
     Bluetooth headset only when there is no other: it records at call quality and loses the \
     first words. System Default uses whatever the system picks."
  }
  _ => {
    "Automatic and System Default both use the system’s input. Linux doesn’t tell Wavo how a \
     microphone is attached, so neither skips a Bluetooth headset."
  }
};
// The menu's slow start warning ([mic_start]) in the Microphone row: what happens, what to do.
const SLOW_MIC: &str = "The microphone takes longer and longer to start, so the first words get \
  lost. Unplug it and plug it back in.";
// Where the person turns launch at login back on, which the system turned off.
const LOGIN_APPROVAL: &str = if cfg!(windows) {
  "Turn Wavo on in Task Manager › Startup apps."
} else {
  "Allow Wavo in System Settings › General › Login Items."
};
const KEEP_HELP: &str = "On: the dictated text stays on the clipboard, pasted or not. Off: after \
  a paste the clipboard gets back what it held; with pasting off, the text is only in History.";
const AUTO_HELP: &str = "The model tells the language of each dictation by itself. Short \
  phrases, an accent or two languages mixed can mislead it: choose the language you speak, or \
  Keyboard layout to follow the layout you type with.";
const MODEL_KEYBOARD_HELP: &str = "With Keyboard layout, each language under Language goes to \
  the model chosen there. This one takes a language set to it, one no model knows, and a layout \
  of any other language.";
/// How far a keyboard language sits in from the Language row's name.
const KEYBOARD_INDENT: f32 = 16.0;
const KEYBOARD_HELP: &str = "Each dictation is in the language of the keyboard layout in use when \
  it ends, with the model chosen for that language below. Switch the layout while you speak and \
  the whole dictation follows it; the HUD shows the language.";
const CLEAN_HELP: &str = "Adds a space after each paste. A paste into the same window less than \
  20 s after one that ended mid-sentence starts in lower case, unless its first word is in Latin \
  letters. Hesitation sounds like “э-э” and “um” are left out in Russian and English, when you \
  choose the language or the model has only one. In Russian, a number of more than one word, or \
  from 11 to 19, is written in digits: “семьдесят шестой” becomes “76-й”.";
const CLIPBOARD_NOTE: &str = "Wavo can’t put the clipboard back after a paste: allow it in System \
  Settings › Privacy & Security › Paste from Other Apps.";
const MICROPHONE_WHY: &str = "Wavo records your voice through it while you hold the shortcut; \
  the recording stays on this Mac.";
const ACCESSIBILITY_WHY: &str = "Wavo hears the shortcut in every app and pastes the text where \
  you type through it.";
const NO_ADMIN: &str = "You’re not an administrator on this Mac, so Wavo can’t paste for you. \
  You can turn on Keep the text on the clipboard and paste with ⌘V.";
const LONE_KEY_NOTE: &str = "A single key needs Accessibility, which only an administrator can \
  allow on this Mac. Use a combination like ⌥Space.";

/// A triangle with "!" in it, for a permission missing.
const WARNING: Icon = Icon::new("M12 4.5 3 19.5h18ZM12 10v4.5M12 17v.2");

const ACTIVATIONS: [Tag<Activation>; 3] = [
  Tag::new(Activation::HoldOrToggle, "Hold to talk, or tap to start and stop"),
  Tag::new(Activation::PushToTalk, "Hold to talk"),
  Tag::new(Activation::Toggle, "Tap to start and stop"),
];

impl Prefs {
  fn general(&mut self, ui: &mut Ui, fs: &FormStyle) {
    self.permissions(ui, fs);
    // By what a dictation needs: the microphone, the model, how the key records and the key; then
    // the language, which may open into one row per keyboard layout.
    ui.form_section(fs, "Dictation", |ui| {
      let (devices, saved) = (&self.devices, &self.settings.microphone);
      let (mics, at) = microphones(devices, saved, self.lid_closed);
      let labels: Vec<&str> = mics.iter().map(|m| m.0.as_str()).collect();
      // One unplugged or only Bluetooth first: what records matters more than how fast it starts.
      let slow = self.status.warnings.iter().any(|w| w == mic_start::SLOW);
      let warning =
        mic_warning(devices, saved, self.lid_closed).or_else(|| slow.then(|| SLOW_MIC.to_owned()));
      // The warning alone in the ⓘ's place: what is wrong matters then, not how the modes choose.
      let mark = match &warning {
        Some(warning) => Mark::Warning(warning),
        None => Mark::Info(MIC_HELP),
      };
      let row = FormRow::new(marked("Microphone", mark, Some(Id::of("mic-row").key("info"))));
      ui.id("mic-row").form_row(fs, row, |ui| {
        // ALSA's names run past 60 characters: a longer one ends in "…" beside its label.
        let mut cur = at;
        let pop = picker(&mut cur, labels.iter().copied(), &[1]).max_width(MIC_WIDTH);
        if pop.show(ui.id("mic")).changed {
          let choice = mics[cur].1.clone();
          self.edit(move |s| s.microphone = choice.clone());
        }
      });

      self.model_row(ui, fs);
      let note = match (self.access.secure_input, self.settings.hotkey == "fn") {
        (true, _) => Some(SECURE_INPUT_NOTE),
        (false, true) => Some(FN_NOTE),
        (false, false) => None,
      };
      // A modifier alone needs the event tap, and so Accessibility.
      let lone = matches!(Hotkey::parse(&self.settings.hotkey), Ok(Hotkey::Modifier(_)));
      let mark = match note {
        _ if lone && self.access.needs_admin => Mark::Warning(LONE_KEY_NOTE),
        Some(note) => Mark::Info(note),
        None => Mark::None,
      };

      let row = FormRow::new(marked("Activation", Mark::Info(ACTIVATION_HELP), None));
      ui.form_row(fs, row, |ui| {
        let mut cur = self.settings.activation;
        if ui.id("activation").picker(&mut cur, &ACTIVATIONS).changed {
          self.edit(move |s| s.activation = cur);
        }
      });
      ui.form_row(fs, FormRow::new(marked("Shortcut", mark, None)), |ui| self.shortcut(ui));
      self.language_row(ui, fs);
    });

    ui.form_section(fs, "Output", |ui| {
      let locked = self.access.needs_admin;
      let row = FormRow::new("Paste the text into the app you dictate in");
      ui.disabled(locked).form_row(fs, row, |ui| {
        let mut on = self.settings.auto_paste;
        if ui.id("paste").toggle(&mut on, "").changed {
          self.edit(move |s| s.auto_paste = on);
        }
      });

      let restores = self.settings.restores_clipboard() && !locked;
      let help = match self.access.clipboard_unread && restores {
        true => format!("{KEEP_HELP}\n\n{CLIPBOARD_NOTE}"),
        false => KEEP_HELP.to_owned(),
      };
      let row = FormRow::new(marked("Keep the text on the clipboard", Mark::Info(&help), None));
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.keep_in_clipboard;
        if ui.id("keep").toggle(&mut on, "").changed {
          self.edit(move |s| s.keep_in_clipboard = on);
        }
      });

      let row = FormRow::new(marked("Clean up the text", Mark::Info(CLEAN_HELP), None));
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.clean_text;
        if ui.id("clean").toggle(&mut on, "").changed {
          self.edit(move |s| s.clean_text = on);
        }
      });
    });

    ui.form_section(fs, "Startup", |ui| self.login(ui, fs));
  }

  /// While the system does not allow them, a row for the microphone and one for Accessibility, or
  /// in its place a warning with no button where only an administrator can allow it.
  fn permissions(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let warning = ui.theme().warning;
    let size = fs.tile_icon;
    let tile = move |ui: &mut Ui, _: TextStyle| _ = ui.icon(WARNING.stroke(1.6), size, warning);
    if self.access.needs_admin {
      let row = FormRow::new(NO_ADMIN).tile(tile);
      ui.form_section(fs, "", |ui| ui.id("no-admin").form_row(fs, row, |_| {}));
    }

    let mic = self.access.microphone != Grant::Granted;
    let ax = !self.access.accessibility && !self.access.needs_admin;
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
    ui.form_section(fs, title, |ui| {
      for (_, name, why, id, pane, ask) in rows.into_iter().filter(|r| r.0) {
        let row = FormRow::new(marked(name, Mark::Info(why), None)).tile(tile);
        ui.form_row(fs, row, |ui| {
          let button = if ask {
            Button::new("Allow").style(ButtonStyle::BorderedProminent)
          } else {
            Button::new("Open Settings")
          };
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
    let off = Hotkey::parse(&self.settings.hotkey).ok() == Hotkey::parse(&default).ok();
    let ui = ui.disabled(off).help("Back to the default shortcut");
    if ui.id("reset").icon_button(Icon::RESET).clicked {
      self.edit(move |s| s.hotkey = default.clone());
    }
  }

  /// The downloaded models, the chosen one when it is none of them, then "Download more…".
  fn model_choices(&self) -> (Vec<String>, usize) {
    let mut items: Vec<String> = self.found.iter().map(|f| f.0.name.to_owned()).collect();
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

  /// The language choices: auto, the keyboard layout's where the system tells it, then the
  /// languages the model in use is meant for.
  fn languages(&self) -> (Vec<(String, String)>, usize) {
    let codes = self.active().map_or(&[][..], |f| f.0.langs);
    let mut langs: Vec<(String, String)> =
      codes.iter().map(|c| (language_name(c), (*c).to_owned())).collect();
    langs.sort();
    langs.insert(0, ("Auto-detect".to_owned(), "auto".to_owned()));
    let keyboard = self.settings.follows_keyboard();
    if keyboard || !self.layouts.is_empty() {
      langs.insert(1, ("Keyboard layout".to_owned(), KEYBOARD.to_owned()));
    }

    if keyboard {
      return (langs, 1);
    }

    let Some(current) = self.settings.engine_language() else { return (langs, 0) };

    if let Some(i) = langs.iter().position(|(_, c)| c.eq_ignore_ascii_case(&current)) {
      return (langs, i);
    }

    langs.push((format!("{} (not one the model is meant for)", language_name(&current)), current));
    let at = langs.len() - 1;
    (langs, at)
  }

  /// The model row; with no model downloaded, the way to download one.
  fn model_row(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let (labels, at) = self.model_choices();
    let note = self.model_note();
    let keyboard = self.settings.follows_keyboard().then_some(MODEL_KEYBOARD_HELP);
    let mark = note.as_deref().or(keyboard).map_or(Mark::None, Mark::Info);
    let name = if keyboard.is_some() { "Default model" } else { "Model" };
    let row = FormRow::new(marked(name, mark, Some(Id::of("model-row").key("info"))));

    let none = self.found.is_empty() && self.pick.other.is_none();
    let mut picked = None;
    ui.form_row(fs, row, |ui| {
      if !none {
        picked = pick(ui, "model", &labels, at, &[labels.len() - 2]);
      } else if Button::new("Download a Model…")
        .style(ButtonStyle::BorderedProminent)
        .show(ui.id("download-model"))
        .clicked
      {
        self.page = Page::Models;
      }
    });

    match picked {
      Some(i) if i + 1 == labels.len() => self.page = Page::Models,
      Some(i) if i < self.found.len() => self.use_model(self.found[i].0.name),
      _ => {}
    }
  }

  /// The language, last of the dictation's rows: with Keyboard layout, its languages under it.
  fn language_row(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let (langs, at) = self.languages();
    let labels: Vec<String> = langs.iter().map(|l| l.0.clone()).collect();
    let head = langs.iter().position(|l| l.1 == KEYBOARD).unwrap_or(0);
    let keyboard = self.settings.follows_keyboard();
    let auto = self.auto_help();
    let mark = match () {
      _ if keyboard => Mark::Info(KEYBOARD_HELP),
      _ if self.settings.engine_language().is_none() => Mark::Info(&auto),
      _ => Mark::None,
    };
    let label = marked("Language", mark, Some(Id::of("lang-row").key("info")));
    // With Keyboard layout, its languages under it in the same row, each with its model.
    let rows = if keyboard { keyboard_languages(&self.layouts) } else { Vec::new() };
    let choices: Vec<_> = rows.iter().map(|(lang, _)| self.keyboard_choices(lang)).collect();
    let mut row = FormRow::new(label);
    let mut chosen = None;
    if !rows.is_empty() {
      let (label_style, chosen, rows, choices) = (fs.label, &mut chosen, &rows, &choices);
      row = row.footer(move |ui: &mut Ui, s: TextStyle| {
        let list = Style::new().w(grow()).gap(4.0).pads([2.0, 0.0, 0.0, KEYBOARD_INDENT]);
        ui.col(list, |ui| {
          for (i, ((lang, layouts), (labels, _, at))) in rows.iter().zip(choices).enumerate() {
            let line = Style::new().w(grow()).gap(6.0).align(Align::Start, Align::Center);
            ui.row(line, |ui| {
              ui.text(language_name(lang), label_style.line_limit(1));
              // The label's size, dimmed: one baseline with the name.
              ui.text(layouts.as_str(), label_style.color(s.color).line_limit(1));
              ui.el(Style::new().w(grow()));
              let id = format!("keyboard-{lang}");
              if let Some(p) = pick(ui, &id, labels, *at, &[0, labels.len() - 2]) {
                *chosen = Some((i, p));
              }
            });
          }
        });
      });
    }

    let mut picked = None;
    ui.form_row(fs, row, |ui| picked = pick(ui, "lang", &labels, at, &[head]));
    if let Some(i) = picked {
      let code = langs[i].1.clone();
      // Keyboard layout starts a language not seen before on its best model downloaded; one the
      // person set, the default too, keeps it.
      let mut rows = self.settings.keyboard_models.clone();
      if code == KEYBOARD {
        for (lang, _) in keyboard_languages(&self.layouts) {
          if let Some(m) = self.best_for(&lang).filter(|_| !rows.contains_key(&lang)) {
            rows.insert(lang, Some(m.to_owned()));
          }
        }
      }

      self.edit(move |s| (s.language, s.keyboard_models) = (code.clone(), rows.clone()));
    }

    let Some((i, p)) = chosen else { return };
    let ((lang, _), (labels, models, _)) = (&rows[i], &choices[i]);
    let lang = lang.clone();
    if p == 0 {
      info!("settings: {lang} goes to the default model");
      self.edit(move |s| _ = s.keyboard_models.insert(lang.clone(), None));
    } else if p + 1 == labels.len() {
      self.model_lang = LANGUAGES.iter().position(|l| l.1 == lang).unwrap_or_default();
      self.page = Page::Models;
    } else if let Some(&model) = models.get(p - 1) {
      info!("settings: {lang} goes to {model}");
      self.edit(move |s| _ = s.keyboard_models.insert(lang.clone(), Some(model.to_owned())));
    }
  }

  /// What Auto-detect does with the model in use: one of one language always takes its own.
  fn auto_help(&self) -> String {
    match self.active().map(|f| f.0) {
      Some(m) if let [code] = m.langs => {
        let lang = language_name(code);
        format!("{} knows {lang} only, so every dictation is in {lang}.", m.name)
      }
      _ => AUTO_HELP.to_owned(),
    }
  }

  /// The downloaded models that know `lang`, as Models orders them in it: the recommended one
  /// first, then by fewest errors.
  fn models_for(&self, lang: &str) -> Vec<&'static ModelInfo> {
    let best = recommended(lang).map(|m| m.name);
    let mut models: Vec<&'static ModelInfo> =
      self.found.iter().map(|f| f.0).filter(|m| m.langs.contains(&lang)).collect();
    models.sort_by_key(|m| rank(lang, best, m));
    models
  }

  /// The model a keyboard layout's language starts on: the best downloaded for it.
  fn best_for(&self, lang: &str) -> Option<&'static str> {
    self.models_for(lang).first().map(|m| m.name)
  }

  /// A keyboard language's choices: the default model (the Model's: none of the language's own),
  /// the downloaded models that know `lang`, best first, the one chosen when it is not on disk,
  /// and Download more…; the models' names, and the choice in use.
  fn keyboard_choices(&self, lang: &str) -> (Vec<String>, Vec<&'static str>, usize) {
    let models = self.models_for(lang);
    let default = match (self.active(), &self.pick.other) {
      (_, Some((name, _))) => name.clone(),
      (Some(f), None) => f.0.name.to_owned(),
      (None, None) => "none".to_owned(),
    };
    let mut labels = vec![format!("{default} (default)")];
    labels.extend(models.iter().map(|m| m.name.to_owned()));
    let at = match self.settings.keyboard_models.get(lang).and_then(Option::as_ref) {
      None => 0,
      Some(c) => match models.iter().position(|m| model_name(c) == Some(m.name)) {
        Some(i) => i + 1,
        None => {
          labels.push(format!("{} (not on disk)", label(c)));
          labels.len() - 1
        }
      },
    };

    labels.push("Download more…".into());
    (labels, models.iter().map(|m| m.name).collect(), at)
  }
}

// MARK: Advanced

const TOOK_HELP: &str = "After each dictation the HUD shows for a second how long the text took after \
  the stop.";
const LIVE_HELP: &str = "The HUD shows the words as you speak, recognized again every half second. \
  The text pasted is still recognized from the whole recording after the stop.";
const UNLOAD_HELP: &str = "Frees the model's memory after Wavo has been idle this long. The next \
  dictation loads it again, which takes a moment.";
const ANE_HELP: &str = "Models marked ANE run on the Neural Engine: as fast as on the GPU at a \
  fraction of the energy. Each new version of Wavo prepares such a model once, in the background \
  (10–30 s); meanwhile it runs on the GPU. Other models always run on the GPU.";
const IMPORT_HELP: &str = "A .zip that Wavo exported, here or on another computer: the dictations this \
  history does not have are added with their recordings; the settings stay as they are.";
const EXPORT_HELP: &str = "The history, its recordings and the settings, into a .zip archive: \
  Import brings it back, here or on another computer.";
/// Export and Import while the history is kept until Wavo quits.
const NOT_SAVED: &str = "Off while the history is kept until Wavo quits";
const DELETE_HELP: &str = "Deletes every dictation saved on disk and its recording. The ones kept \
  until Wavo quits stay.";
/// Delete All with no dictation saved on disk.
const NOTHING_SAVED: &str = "No dictations saved";
/// Delete All asked for, while the dictation, the export or the import runs.
const DELETE_WAITS: &str = "Deleted once the dictation, export or import ends";

const UPDATE_HELP: &str = "Wavo looks for a new version at launch and once a day; the menu says \
  when there is one. Off, it looks only when you choose Check for Updates.";
const NO_UPDATES: &str = "This copy of Wavo doesn’t update itself.";

const HUDS: [Tag<HudPosition>; 2] = [
  Tag::new(HudPosition::Bottom, "Bottom of the screen"),
  Tag::new(HudPosition::Top, "Top of the screen"),
];

const HUD_STYLES: [Tag<HudStyle>; 2] =
  [Tag::new(HudStyle::Gradient, "Gradient"), Tag::new(HudStyle::Plain, "Plain")];

const UNLOADS: [Tag<Option<u32>>; 7] = [
  Tag::new(None, "Never"),
  Tag::new(Some(0), "Right after each dictation"),
  Tag::new(Some(2), "After 2 minutes"),
  Tag::new(Some(5), "After 5 minutes"),
  Tag::new(Some(10), "After 10 minutes"),
  Tag::new(Some(15), "After 15 minutes"),
  Tag::new(Some(60), "After 60 minutes"),
];

/// The last item, after a line: the history in memory.
const UNTIL_QUIT: Tag<Retention> = Tag::new(Retention::UntilQuit, "Until Wavo quits");

const RETENTIONS: [Tag<Retention>; 7] = [
  Tag::new(Retention::Unlimited, "Forever"),
  Tag::new(Retention::Count(100), "The last 100 dictations"),
  Tag::new(Retention::Count(1000), "The last 1,000 dictations"),
  Tag::new(Retention::Count(10_000), "The last 10,000 dictations"),
  Tag::new(Retention::Days3, "3 days"),
  Tag::new(Retention::Weeks2, "2 weeks"),
  Tag::new(Retention::Months3, "3 months"),
];

impl Prefs {
  fn advanced(&mut self, ui: &mut Ui, fs: &FormStyle) {
    // The model and where it runs, then the history, then updates.
    ui.form_section(fs, "App", |ui| {
      let row = FormRow::new(marked("Unload the model", Mark::Info(UNLOAD_HELP), None));
      ui.form_row(fs, row, |ui| {
        let mut cur = self.settings.unload_after_minutes;
        let p = Picker::new(&mut cur).options(&UNLOADS[..1]).divider().options(&UNLOADS[1..]);
        if p.show(ui.id("unload")).changed {
          self.edit(move |s| s.unload_after_minutes = cur);
        }
      });

      // Only where there is one: Apple silicon with macOS 15 or later.
      if self.shell.has_neural_engine() {
        let row = FormRow::new(marked("Use the Neural Engine", Mark::Info(ANE_HELP), None));
        ui.form_row(fs, row, |ui| {
          let mut on = self.settings.neural_engine;
          if ui.id("ane").toggle(&mut on, "").changed {
            self.edit(move |s| s.neural_engine = on);
          }
        });
      }

      let help = format!(
        "Until Wavo quits: the last {IN_MEMORY} dictations stay in memory for History and the \
        menu, without their audio, and nothing is saved to disk."
      );
      let row = FormRow::new(marked("Keep the history", Mark::Info(&help), None));
      ui.form_row(fs, row, |ui| {
        let mut cur = self.settings.history_retention;
        let p = Picker::new(&mut cur).options(&RETENTIONS).divider().options(&[UNTIL_QUIT]);
        if p.show(ui.id("retention")).changed {
          self.edit(move |s| s.history_retention = cur);
        }
      });

      // Off and disabled where the app can't update, the setting kept; its ⓘ says why.
      let updates = self.shell.updates();
      let help = if updates { UPDATE_HELP } else { NO_UPDATES };
      let mark = Some(Id::of("auto-update-row").key("info"));
      let row = FormRow::new(marked("Check for updates", Mark::Info(help), mark));
      ui.disabled(!updates).id("auto-update-row").form_row(fs, row, |ui| {
        let mut on = updates && self.settings.auto_update;
        if ui.id("auto-update").toggle(&mut on, "").changed {
          self.edit(move |s| s.auto_update = on);
        }
      });
    });

    ui.form_section(fs, "HUD", |ui| {
      ui.form_row(fs, FormRow::new("Show the HUD at"), |ui| {
        let mut cur = self.settings.hud_position;
        if ui.id("hud").picker(&mut cur, &HUDS).changed {
          self.edit(move |s| s.hud_position = cur);
        }
      });

      ui.form_row(fs, FormRow::new("HUD style"), |ui| {
        let mut cur = self.settings.hud_style;
        if ui.id("hud-style").picker(&mut cur, &HUD_STYLES).changed {
          self.edit(move |s| s.hud_style = cur);
        }
      });

      let row = FormRow::new(marked("Show the text while speaking", Mark::Info(LIVE_HELP), None));
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.live_preview;
        if ui.id("live").toggle(&mut on, "").changed {
          self.edit(move |s| s.live_preview = on);
        }
      });

      let row = FormRow::new(marked("Show the transcription time", Mark::Info(TOOK_HELP), None));
      ui.form_row(fs, row, |ui| {
        let mut on = self.settings.show_transcription_time;
        if ui.id("took").toggle(&mut on, "").changed {
          self.edit(move |s| s.show_transcription_time = on);
        }
      });
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
        let row = FormRow::new(marked(name, Mark::Info(&help), None)).tile(Icon::FOLDER);
        ui.form_row(fs, row, |ui| {
          let ui = ui.help(&paths::display(dir)).disabled(!dir.is_dir());
          if ui.id(("reveal", name)).button(&show).clicked {
            self.shell.reveal(dir);
          }
        });
      }
    });

    ui.form_section(fs, "Data", |ui| {
      self.export_row(ui, fs, &show);
      self.import_row(ui, fs);
      self.delete_row(ui, fs);
    });
  }

  /// Delete All…, asking first, for the history saved on disk in either mode; off with nothing
  /// saved. Its caption says when the delete waits for what runs.
  fn delete_row(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let n = match self.saved {
      Some(n) => n,
      None => *self.saved.insert(self.count_saved()),
    };
    let waits = self.status.warnings.iter().any(|w| w == session::DELETE_WAITS);
    let caption = match () {
      _ if waits => Some(DELETE_WAITS.to_owned()),
      _ if n == 0 => Some(NOTHING_SAVED.to_owned()),
      _ => None,
    };
    let shown = Shown { caption, progress: None, failed: false };
    let row = FormRow::new(marked("Delete history", Mark::Info(DELETE_HELP), None));
    let row = row.tile(Icon::TRASH);
    data_row(ui, fs, Id::of("delete-row"), row, &shown, |ui| {
      let button = Button::new("Delete All…").role(ButtonRole::Destructive);
      if button.show(ui.disabled(n == 0).id("delete-all")).clicked {
        let title = match n {
          1 => "Delete 1 dictation and its recording?".to_owned(),
          n => format!("Delete all {} dictations and their recordings?", thousands(n)),
        };
        let message = "This can’t be undone.".to_owned();
        self.ask = Some(Ask { title, message, delete: Delete::Saved });
      }
    });
  }

  /// How many dictations the history saved on disk has: the one read here, or the one left there
  /// while the history is kept in memory; 0 when it can't be read.
  fn count_saved(&self) -> usize {
    let total = match self.history.db.as_ref().filter(|h| !h.in_memory()) {
      Some(h) => h.db.get_total().map(|t| t.0),
      None => History::count_saved(&self.home),
    };
    total.inspect_err(|e| warn!("the saved history: {e:#}")).unwrap_or(0)
  }

  /// Export, the system's save panel in Downloads ([Prefs::event]); then the archive's size with
  /// Show in Finder. Off in memory, as Import.
  fn export_row(&mut self, ui: &mut Ui, fs: &FormStyle, show: &str) {
    let state = self.shell.export_state();
    let ended = |(_, size): &(PathBuf, u64)| format!("Exported · {}", size_text(*size));
    let mut shown = shown(&state, "export", "Copying the history…", ended);
    let running = shown.progress.is_some();
    let off = self.not_saved(&mut shown);
    let row = FormRow::new(marked("Export history", Mark::Info(EXPORT_HELP), None)).tile(Icon::DOC);
    data_row(ui, fs, Id::of("export-row"), row, &shown, |ui| {
      if let Some(Transfer::Done((file, _))) = &state
        && ui.disabled(!file.is_file()).id("export-show").button(show).clicked
      {
        self.shell.reveal(file);
      }

      if ui.disabled(running || off).id("export").button("Export…").clicked {
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
    let mut shown = shown(&state, "import", "Reading the archive…", imported_text);
    let running = shown.progress.is_some();
    let off = self.not_saved(&mut shown);
    let row = FormRow::new(marked("Import history", Mark::Info(IMPORT_HELP), None));
    let row = row.tile(Icon::DOWNLOAD);
    data_row(ui, fs, Id::of("import-row"), row, &shown, |ui| {
      if ui.disabled(running || off).id("import").button("Import…").clicked {
        let panel = FileDialog::open("Import").types(&["zip"]);
        ui.file_dialog(FileDialog { dir: nimbo::paths::downloads(), ..panel });
        self.panel = Some(Panel::Import);
      }
    });
  }

  /// The history is kept until Wavo quits: a Data row says it is off, unless it runs.
  fn not_saved(&self, shown: &mut Shown) -> bool {
    let off = self.settings.history_retention.in_memory();
    if off && shown.progress.is_none() {
      (shown.caption, shown.failed) = (Some(NOT_SAVED.into()), false);
    }

    off
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
    let off = login.state.is_err() || login.busy.is_some();
    let mark = note.as_deref().map_or(Mark::None, Mark::Info);
    ui.disabled(off).form_row(fs, FormRow::new(marked("Launch at login", mark, None)), |ui| {
      if ui.id("login").toggle(&mut on, "").changed {
        self.login.busy = Some(on);
        self.login.error = None;
        self.shell.set_autostart(on);
      }
    });
  }

  /// A history request the session refused: said under the panel.
  pub fn history_failed(&mut self, req: Request, error: &str) {
    let what = match req {
      Request::Delete(ids) if ids.len() > 1 => "Could not delete the dictations",
      Request::Delete(_) => "Could not delete the dictation",
      Request::Retranscribe(_) => "Could not transcribe it again",
      Request::Restore(_) => "Could not put the previous text back",
      Request::DeleteSaved => "Could not delete the history",
      Request::Unload | Request::Transferred | Request::Close => return,
    };
    self.history.said = Some(format!("{what}: {error}"));
  }

  /// [Shell::set_autostart] to `on` ended: the system's state after it, or why not.
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
  let default = devices.iter().find(|d| d.default);
  let automatic = match (audio::automatic(devices, lid_closed), default) {
    (Auto::Device(i), _) => format!("Automatic ({})", devices[i].name),
    // Only virtual ones, or a closed lid's with a virtual default: the system's.
    (Auto::System, Some(d)) => format!("Automatic ({})", d.name),
    (Auto::System, None) => "Automatic".to_owned(),
    (Auto::Nothing, _) => "Automatic (none: the lid is closed)".to_owned(),
  };
  let default = match default {
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
  if let Some(i) = audio::position(devices, saved) {
    return (items, i + 2);
  }

  items.push((format!("{} (not connected)", saved.name), Microphone::Device(saved.clone())));
  let at = items.len() - 1;
  (items, at)
}

/// While the `saved` microphone isn't connected: its name, and what records in its place, the
/// system's input ([audio::in_use]), [Auto::Nothing] when there is no input. Here and in the menu.
pub fn not_connected<'a>(
  devices: &[audio::Device],
  saved: &'a Microphone,
  lid_closed: bool,
) -> Option<(&'a str, Auto)> {
  let Microphone::Device(choice) = saved else { return None };
  let missing = audio::position(devices, choice).is_none();
  missing.then(|| (choice.name.as_str(), audio::in_use(devices, saved, lid_closed)))
}

/// A listed microphone's name, the built-in one's under a closed lid saying so, as Automatic's
/// does: it hears nothing.
pub fn mic_name(device: &audio::Device, lid_closed: bool) -> String {
  match lid_closed && device.transport == Transport::BuiltIn {
    true => format!("{} (the lid is closed)", device.name),
    false => device.name.clone(),
  }
}

/// The Microphone row's warning: the one chosen isn't connected, and what records instead; or
/// Automatic has only a Bluetooth one.
fn mic_warning(devices: &[audio::Device], saved: &Microphone, lid_closed: bool) -> Option<String> {
  let Some((name, instead)) = not_connected(devices, saved, lid_closed) else {
    return bluetooth_only(devices, saved, lid_closed).map(str::to_owned);
  };

  Some(match instead {
    Auto::Device(i) => {
      format!("{name} isn’t connected: Wavo records with {}", mic_name(&devices[i], lid_closed))
    }
    Auto::System => format!("{name} isn’t connected: Wavo records with the system’s input"),
    Auto::Nothing => format!("{name} isn’t connected, and there is no other microphone"),
  })
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
  let line = Style::new().w(grow()).h(px(ui.line_height(fs.caption))).gap(8.0);
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

/// Where a model goes in a list of Settings › Models, the least first: `best`, then the measured
/// ones by [measure], then the rest, with a language chosen those made for it alone first.
fn rank(lang: &str, best: Option<&str>, m: &ModelInfo) -> (bool, bool, i32, bool) {
  let by = measure(lang, m);
  let at = by.map_or(0, |x| (x * 100.0).round() as i32);
  (Some(m.name) != best, by.is_none(), at, !lang.is_empty() && m.langs != [lang])
}

/// What orders a measured model, the least first: its errors in `lang`, or with all languages
/// its speed, the fastest first. `None` when not measured.
fn measure(lang: &str, m: &ModelInfo) -> Option<f32> {
  if lang.is_empty() { m.speed().map(|x| -x) } else { m.wer(lang) }
}

/// A model at least this many times faster than whisper-turbo is fast.
const FAST: f32 = 3.0;

/// The most errors in 100 words (characters where wavo counts those) a model may make in a
/// language and still be usable in it: with more, a person fixes about every fourth word and
/// would rather type. Settings › Models never recommends a model above it, and folds those away.
const USABLE: f32 = 25.0;

/// Whether `m` was measured in `lang` and makes too many errors there to be usable ([USABLE]).
fn unusable(lang: &str, m: &ModelInfo) -> bool {
  m.wer(lang).is_some_and(|x| x > USABLE)
}

/// Whether models were measured in `lang` but none of them is usable there ([USABLE]).
fn none_usable(lang: &str) -> bool {
  let measured = hub::MODELS.iter().any(|m| m.wer(lang).is_some());
  measured && recommended(lang).is_none()
}

/// The model to recommend for `lang` by wavo's measures: the fast one with the fewest errors, or
/// the fastest within 10% of it; a slow one only with 1.5 times fewer errors. `None` when none
/// was measured on it, or none of those is usable in it ([USABLE]).
fn recommended(lang: &str) -> Option<&'static ModelInfo> {
  let measured: Vec<(&'static ModelInfo, f32, f32)> = (hub::MODELS.iter())
    .filter(|m| m.punctuated)
    .filter_map(|m| Some((m, m.wer(lang).filter(|&x| x <= USABLE)?, m.speed().unwrap_or(0.0))))
    .collect();
  let least = |fast: bool| {
    let them = measured.iter().filter(move |x| (x.2 >= FAST) == fast);
    them.min_by(|a, b| a.1.total_cmp(&b.1))
  };
  let Some(&(_, wer, _)) = least(true) else {
    return least(false).map(|x| x.0);
  };

  if let Some(slow) = least(false).filter(|x| x.1 * 1.5 <= wer) {
    return Some(slow.0);
  }

  let near = measured.iter().filter(|x| x.2 >= FAST && x.1 <= wer * 1.1);
  near.max_by(|a, b| a.2.total_cmp(&b.2)).map(|x| x.0)
}

/// `WER 4.5%` of `m` in `lang`, a decimal under 10 (`WER 32%`); `None` when not measured.
fn wer_text(lang: &str, m: &ModelInfo) -> Option<String> {
  Some(format!("WER {}%", figure(m.wer(lang)?)))
}

/// Languages written without spaces, whose WER wavo counts in characters: `BY_CHARACTER` in
/// wavo's `scripts/wer.py`, which makes `src/hub/scores.rs` (the crate doesn't say it).
const BY_CHARACTER: [&str; 7] = ["zh", "yue", "ja", "th", "lo", "my", "km"];

/// What a WER in `lang` counts, on its hover.
fn errors_per(lang: &str) -> &'static str {
  if BY_CHARACTER.contains(&lang) { "Errors per 100 characters" } else { "Errors per 100 words" }
}

/// A WER or a speed as Settings › Models shows it: a decimal under 10 (`4.5`), whole from it
/// (`32`; 9.96 is `10`).
fn figure(x: f32) -> String {
  let tenths = (x * 10.0).round() / 10.0;
  if tenths < 10.0 { format!("{tenths:.1}") } else { format!("{:.0}", x.round()) }
}

/// The model Settings › Models recommends for `lang` and why: [recommended], else whisper-turbo
/// where it takes the language and wasn't measured on it, the best general model; [RECOMMENDED]
/// for all languages.
fn best_for(lang: &str) -> Option<(&'static str, String)> {
  let name = language_name(lang);
  match (lang, recommended(lang)) {
    ("", _) => Some((RECOMMENDED, "The default model".into())),
    (_, Some(m)) => Some((m.name, format!("Best balance of accuracy and speed in {name}"))),
    _ => (hub::lookup("whisper-turbo").ok())
      .filter(|m| m.langs.contains(&lang) && m.wer(lang).is_none())
      .map(|m| (m.name, format!("No accuracy data for {name} yet: the best general model"))),
  }
}

/// `1 model`, `3 models`.
fn model_count(n: usize) -> String {
  format!("{n} model{}", if n == 1 { "" } else { "s" })
}

/// A speed as times whisper-turbo's: `22×`, a decimal under 10 (`8.5×`, `0.9×`).
fn times(x: f32) -> String {
  format!("{}×", figure(x))
}

/// What [times] says on hover: `22× faster than whisper-turbo`, `As fast as whisper-turbo` when
/// it shows `1.0×`; `None` when not measured.
fn speed_text(m: &ModelInfo) -> Option<String> {
  let x = m.speed()?;
  let times = times(x);
  Some(match m.name {
    "whisper-turbo" => format!("The speed baseline ({times})"),
    _ if times == "1.0×" => "As fast as whisper-turbo".to_owned(),
    _ if x < 1.0 => format!("{times} the speed of whisper-turbo"),
    _ => format!("{times} faster than whisper-turbo"),
  })
}

impl Prefs {
  fn models_head(&mut self, ui: &mut Ui, s: Style) -> Response {
    let items: Vec<Tag<usize>> =
      LANGUAGES.iter().enumerate().map(|(i, l)| Tag::new(i, l.0.as_str())).collect();
    ui.id(PAGE_HEAD).row(s, |ui| {
      ui.id("model-search").search(&mut self.model_search, "Search models");
      ui.id("model-lang").picker(&mut self.model_lang, &items);
    })
  }

  /// wavo's models in two plates by [rank], the one in use and the one chosen shown whatever the
  /// search.
  /// Every row is one line of name and one of caption in every state: the list never moves
  /// under the pointer.
  fn models(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let lang = LANGUAGES[self.model_lang].1;
    if let Some(note) = self.model_note() {
      ui.text(note, fs.caption.color(ui.theme().warning));
    }

    if none_usable(lang) {
      let note = format!("No model recognizes {} well yet.", language_name(lang));
      ui.id("no-usable-model").text(note, fs.caption);
    }

    let fs = &FormStyle { caption: fs.caption.line_limit(1), ..*fs };
    let (active, chosen) = (self.active().map(|f| f.0.name), self.chosen());
    let shown = |m: &ModelInfo| {
      let lang = lang.is_empty() || m.langs.contains(&lang);
      active == Some(m.name) || chosen == Some(m.name) || (lang && finds(&self.model_search, m))
    };
    let on_disk = |m: &ModelInfo| self.found.iter().any(|f| f.0.name == m.name);
    let pick = best_for(lang);
    let best = pick.as_ref().map(|p| p.0);
    let why = |m: &ModelInfo| pick.as_ref().filter(|p| p.0 == m.name).map(|p| p.1.as_str());
    let mut downloaded: Vec<&'static ModelInfo> =
      hub::MODELS.iter().filter(|m| on_disk(m) && shown(m)).collect();
    // The ones dictations go to first, then as the rest.
    downloaded.sort_by_key(|m| (!self.dictates(m), rank(lang, best, m)));
    // Lowercase without punctuation only to delete, but the one chosen, to download it again.
    let offered = |m: &ModelInfo| m.punctuated || chosen == Some(m.name);
    let mut remote: Vec<&'static ModelInfo> =
      hub::MODELS.iter().filter(|m| !on_disk(m) && offered(m) && shown(m)).collect();
    remote.sort_by_key(|m| rank(lang, best, m));
    // Folded under rows of their own, in rank: those with too many errors, and the ones not
    // measured unless none is; but one downloading and the one chosen, and none while searching.
    let searching = !self.model_search.trim().is_empty();
    let busy = |m: &ModelInfo| self.shell.download_state(m.name).is_some();
    let kept = |m: &ModelInfo| searching || busy(m) || chosen == Some(m.name);
    let measured = remote.iter().any(|m| measure(lang, m).is_some());
    let (remote, poor): (Vec<&ModelInfo>, Vec<&ModelInfo>) =
      remote.into_iter().partition(|m| kept(m) || !unusable(lang, m));
    let (remote, other): (Vec<&ModelInfo>, Vec<&ModelInfo>) =
      remote.into_iter().partition(|m| kept(m) || !measured || measure(lang, m).is_some());
    let available = remote.len() + other.len() + poor.len();

    let fit = |m: &ModelInfo| Fit { lang, best: why(m) };
    let mut acts = vec![];
    if self.found.is_empty() {
      ui.form_section(fs, "Downloaded", |ui| {
        let row = FormRow::new("No model yet")
          .caption("Download the recommended one below to start dictating.")
          .tile(Icon::DOWNLOAD);
        ui.form_row(fs, row, |_| {});
      });
    } else if !downloaded.is_empty() {
      acts = self.downloaded_list(ui, fs, &downloaded, fit);
    }

    if available > 0 {
      let note = model_count(available);
      let head = |ui: &mut Ui, s: TextStyle| {
        ui.text("Available", s);
        ui.spacer();
        ui.text(&note, fs.note);
      };
      ui.form_section(fs, Section::new(head), |ui| {
        for &m in &remote {
          self.remote_row(ui, fs, m, fit(m));
        }

        // A folded row of models, none recommended.
        let folded =
          |ui: &mut Ui, id: &str, title: &str, caption: &str, them: &[&'static ModelInfo]| {
            let row = FormRow::new(title).caption(caption).tile(MORE);
            ui.id((id, lang)).form_disclosure(fs, row, |ui| {
              for &m in them {
                self.remote_row(ui, fs, m, Fit { lang, best: None });
              }
            });
          };
        if !other.is_empty() {
          let more = match lang {
            "" => format!("{} more, not measured yet", other.len()),
            _ => format!("{} more, not measured in {}", other.len(), language_name(lang)),
          };
          folded(ui, "other-models", "Other models", &more, &other);
        }

        if !poor.is_empty() {
          let title = format!("Too many errors in {}", language_name(lang));
          folded(ui, "poor-models", &title, &format!("{} more", poor.len()), &poor);
        }
      });
    }

    if downloaded.is_empty() && available == 0 {
      ui.text("No model matches.", fs.caption);
    }

    self.ask_delete_models(acts);
  }

  /// The models on disk in a list for selecting several to delete, as the History page's, under
  /// its Downloaded header: the models to ask about deleting, by a row's bin, or the selection's
  /// Delete key or menu.
  fn downloaded_list<'f>(
    &mut self,
    ui: &mut Ui,
    fs: &FormStyle,
    downloaded: &[&'static ModelInfo],
    fit: impl Fn(&ModelInfo) -> Fit<'f>,
  ) -> Vec<&'static ModelInfo> {
    let (active, mut act) = (self.active().map(|f| f.0.name), None);
    let size: u64 = downloaded.iter().map(|m| m.size).sum();
    let note = format!("{} · {}", model_count(downloaded.len()), size_text(size));
    let line = ui.line_height(fs.head);
    let mut sel = std::mem::take(&mut self.model_sel);
    // As a form section's title: the note centred on the title's line.
    let head = |ui: &mut Ui, _| {
      ui.row(Style::new().w(grow()).h(px(line)).align(Align::Start, Align::Center), |ui| {
        ui.text("Downloaded", fs.head);
        ui.spacer();
        ui.text(&note, fs.note);
      });
    };
    // Its rows the form's, padded by the list.
    let keys: Vec<u64> = downloaded.iter().map(|m| model_key(m)).collect();
    let key = |i: usize| keys[i];
    let (row_h, head_h) = (two_line_h(ui, fs), line + fs.head_pad[2]);
    let rfs = &FormStyle { row: fs.row.pad(0.0), tile_x: fs.tile_x - fs.row.pad[3], ..*fs };
    let list = List::new(downloaded.len(), row_h).key(&key).multi().selection(&mut sel);
    let list = list.sections(&[0], head_h, &head).list_style(ListStyle::Grouped);
    let list = list.style(Style::new().w(grow()).h(px(head_h + row_h * keys.len() as f32)));
    let r = list.show(ui.id("downloaded"), |ui, row| {
      let m = downloaded[row.index];
      act = self.downloaded_row(ui, rfs, m, fit(m), active == Some(m.name)).or(act);
    });

    let chosen: Vec<_> =
      downloaded.iter().copied().filter(|m| sel.contains(model_key(m))).collect();
    self.model_sel = sel;
    let n = chosen.iter().filter(|m| !self.in_use(m)).count();
    let label = format!("Delete {n} Model{}…", if n == 1 { "" } else { "s" });
    let item = match chosen.is_empty() {
      true => None,
      false => ui.context_menu(r.id, || {
        let delete = Item::button((), &label).role(ButtonRole::Destructive);
        Menu::new(vec![delete.enabled(n > 0)])
      }),
    };
    match act {
      Some(m) => vec![m],
      None if r.delete || item.is_some() => chosen,
      None => vec![],
    }
  }

  /// Asks about deleting `models`, but those [in use](Self::in_use), which stay.
  fn ask_delete_models(&mut self, models: Vec<&'static ModelInfo>) {
    let (kept, models): (Vec<&ModelInfo>, Vec<_>) =
      models.into_iter().partition(|m| self.in_use(m));
    let size = size_text(models.iter().map(|m| m.size).sum());
    let (title, mut message) = match &models[..] {
      [] => return,
      [m] => (
        format!("Delete {}?", m.name),
        format!("It frees {size} in the Hugging Face cache. You can download it again."),
      ),
      _ => (
        format!("Delete {} models?", models.len()),
        format!("They free {size} in the Hugging Face cache. You can download them again."),
      ),
    };
    match &kept[..] {
      [] => {}
      [m] => message += &format!(" {} is in use and stays.", m.name),
      _ => {
        let names: Vec<&str> = kept.iter().map(|m| m.name).collect();
        message += &format!(" {} are in use and stay.", names.join(" and "));
      }
    }

    self.ask = Some(Ask { title, message, delete: Delete::Models(models) });
  }

  fn use_model(&mut self, name: &'static str) {
    info!("settings: use model {name}");
    self.edit(move |s| s.model = Some(name.to_owned()));
  }

  /// Whether the model runs on this Mac's Neural Engine.
  fn ane(&self, m: &ModelInfo) -> bool {
    self.shell.has_neural_engine() && m.neural_engine()
  }

  /// Its [Fit], Use (or In use) and the bin in [actions], ↗: the model to ask about deleting.
  fn downloaded_row(
    &mut self,
    ui: &mut Ui,
    fs: &FormStyle,
    m: &'static ModelInfo,
    fit: Fit,
    in_use: bool,
  ) -> Option<&'static ModelInfo> {
    let t = ui.theme();
    let (id, ane) = (Id::of(("model", m.name)), self.ane(m));
    let (mut used, mut act) = (false, None);
    let caption = |ui: &mut Ui, s: TextStyle| model_caption(ui, s, id, m, fit, ane);
    ui.with_id(id).form_row(fs, model_row(m, fit).caption(caption), |ui| {
      fit.show(ui, m);
      let slot = actions(ui, fs.gap);
      // Use and In use as wide, both centred in one column: the rows line up.
      let w = use_w(ui).ceil();
      let column = Style::new().w(px(w)).align(Align::Center, Align::Center);
      ui.row(slot, |ui| {
        ui.row(column, |ui| {
          if in_use {
            let s = t.label().color(t.accent).font(t.font.weight(Weight::Medium)).nowrap();
            ui.text("In use", s);
          } else {
            let b = t.button(ButtonStyle::Bordered);
            let look = ButtonLook { frame: b.frame.w(px(w)), ..b };
            used = Button::new("Use").look(look).show(ui.id(("use", m.name))).clicked;
          }
        });

        // The one in use stays, and the one a dictation goes on with after a switch.
        let busy = !in_use && self.busy(m);
        let tip = match (in_use, busy) {
          (true, _) => "In use: choose another to delete it",
          (_, true) => "In use until the dictation ends",
          _ => "Delete…",
        };
        let off = in_use || busy;
        if ui.help(tip).disabled(off).id(("delete", m.name)).icon_button(Icon::TRASH).clicked {
          act = Some(m);
        }
      });
      self.card(ui, m);
    });

    if used {
      self.use_model(m.name);
    }

    act
  }

  /// Its [Fit] and Download (Retry after a failure) in [actions], or its progress in place of the
  /// caption with ✕ there.
  fn remote_row(&self, ui: &mut Ui, fs: &FormStyle, m: &'static ModelInfo, fit: Fit) {
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
      None => (None, None),
    };

    let (id, failed) = (Id::of(("model", m.name)), matches!(state, Some(Download::Failed(_))));
    let shown = Shown { caption, progress, failed };
    let controls = |ui: &mut Ui| {
      fit.show(ui, m);
      let slot = actions(ui, fs.gap);
      ui.row(slot, |ui| {
        if shown.progress.is_none() {
          let label = if failed { "Retry" } else { "Download" };
          let mut button = Button::new(label);
          if fit.best.is_some() {
            button = button.style(ButtonStyle::BorderedProminent);
          }

          if button.show(ui.id(("download", m.name))).clicked {
            self.shell.download(m);
          }
        } else {
          let preparing = matches!(state, Some(Download::Preparing));
          let tip = if preparing { "Cancel" } else { "Cancel the download" };
          if ui.help(tip).id(("cancel", m.name)).icon_button(Icon::X).clicked {
            self.shell.cancel_download(m.name);
          }
        }
      });
      self.card(ui, m);
    };
    // Idle, its caption; else its download's.
    match state {
      None => {
        let caption = |ui: &mut Ui, s: TextStyle| model_caption(ui, s, id, m, fit, self.ane(m));
        ui.with_id(id).form_row(fs, model_row(m, fit).caption(caption), controls);
      }
      Some(_) => data_row(ui, fs, id, model_row(m, fit), &shown, controls),
    }
  }

  /// A model's ↗, last in its row: its card (license, languages, benchmarks) in the browser, the
  /// sign About's links have too.
  fn card(&self, ui: &mut Ui, m: &ModelInfo) {
    let ui = ui.help("Open the model’s page on Hugging Face").id(("card", m.name));
    if ui.icon_button(Icon::EXTERNAL).clicked {
      self.shell.open_url(m.card);
    }
  }
}

/// The slot of a model row's buttons but ↗, as wide as the widest it holds (the column of Use
/// and In use and the bin, Download, Retry) in this theme's fonts: the bolts before it line up
/// on every row.
fn actions(ui: &mut Ui, gap: f32) -> Style {
  let t = ui.theme();
  let b = t.button(ButtonStyle::Bordered);
  let (h, pad) = (b.frame.min[0], b.frame.pad[1] + b.frame.pad[3]);
  let column = use_w(ui).ceil();
  let mut button = |s: &str| (ui.measure(s, b.text) + pad).max(h);
  let wide = [button("Download"), button("Retry")];
  let w = wide.into_iter().fold(column + gap + h, f32::max);
  Style::new().w(px(w.ceil())).gap(gap).align(Align::End, Align::Center)
}

/// The column of Use and In use: as wide as the wider.
fn use_w(ui: &mut Ui) -> f32 {
  let t = ui.theme();
  let b = t.button(ButtonStyle::Bordered);
  let button = (ui.measure("Use", b.text) + b.frame.pad[1] + b.frame.pad[3]).max(b.frame.min[0]);
  let in_use = ui.measure("In use", t.label().font(t.font.weight(Weight::Medium)));
  in_use.max(button)
}

/// Three dots, the tile of Other models.
const MORE: Icon = Icon::filled(concat!(
  "M4.4 12a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0",
  "M10.2 12a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0",
  "M16 12a1.8 1.8 0 1 0 3.6 0a1.8 1.8 0 1 0-3.6 0"
));
/// A bolt, as SF Symbols' bolt.fill, after a speed.
const BOLT: Icon = Icon::filled("M14.24 1.78 6.68 13.26H12.14L10.46 22.22 18.02 10.74H12.56Z");

/// How a model suits the language chosen in Settings › Models.
#[derive(Clone, Copy)]
struct Fit<'a> {
  lang: &'a str,
  /// Why it is the one recommended for it ([best_for]), on the badge's hover.
  best: Option<&'a str>,
}

impl Fit<'_> {
  /// The model's speed, [times] and a bolt after it, in the secondary label's colour, at the right
  /// of a slot as wide as the widest speed, so the bolts and the × line up from row to row; empty
  /// for a model not measured; what it means on hover and for assistive technology.
  fn show(self, ui: &mut Ui, m: &ModelInfo) {
    let t = ui.theme();
    let s = TextStyle::new(t.small, t.muted).nowrap();
    let w = ["8.8×", "88×"].map(|x| ui.measure(x, s)).into_iter().fold(0.0, f32::max);
    let slot = Style::new().w(px((w + 11.0).ceil())).h(px(16.0)).gap(4.0);
    let slot = slot.align(Align::End, Align::Center);
    let (Some(speed), Some(text)) = (m.speed(), speed_text(m)) else {
      ui.row(slot, |_| {});
      return;
    };

    let (id, times) = (Id::of(("speed", m.name)), times(speed));
    ui.help(&text).with_id(id).row(slot.hit(), |ui| {
      ui.text(&times, s);
      // Its view box 13 square, the bolt 3.6 in from the left.
      let draw = move |f: &mut Frame, r: Rect| {
        icon::draw(f, BOLT, Rect::new(r.x - 3.6, r.y, 13.0, 13.0), t.muted)
      };
      ui.with_id(id.key("bolt")).canvas(Style::new().w(px(7.0)).h(px(13.0)), draw);
    });
    ui.a11y_label(&times).a11y(id, Role::StaticText);
  }
}

/// A model's key in a list: its place in wavo's.
fn model_key(m: &ModelInfo) -> u64 {
  hub::MODELS.iter().position(|x| x.name == m.name).unwrap_or_default() as u64
}

/// A model's row: the letter of its family on its tile, its short name on one line, Recommended
/// after it on the [Fit]'s best.
fn model_row<'a>(m: &'a ModelInfo, fit: Fit<'a>) -> FormRow<'a> {
  let tile = m.family.get(..1).unwrap_or("·");
  let label = move |ui: &mut Ui, s: TextStyle| {
    ui.text(m.name, s.line_limit(1));
    // Why it is the one recommended for it, on the badge's hover and for assistive technology.
    if let Some(why) = fit.best {
      ui.help(why).control_size(ControlSize::Small).badge("Recommended", Tone::Accent);
    }
  };
  FormRow::new(label).tile(tile)
}

/// Model row `row`'s caption ([FormRow::caption]) in its style `s`: its WER in the [Fit]'s
/// language, dotted, what it means on hover, else its languages; the size; with `ane` ANE,
/// dotted, what it is on hover.
fn model_caption(ui: &mut Ui, s: TextStyle, row: Id, m: &ModelInfo, fit: Fit, ane: bool) {
  // One line, each piece ending in "…" where the row is short.
  let s = s.line_limit(1);
  ui.row(Style::new().w(grow()).clip(), |ui| {
    match wer_text(fit.lang, m) {
      Some(wer) => ui
        .with_id(row.key("wer"))
        .help(errors_per(fit.lang))
        .text(wer, s.underline(LinePattern::Dot)),
      None => ui.text(m.languages, s),
    }

    ui.text(format!(" · {}{}", size_text(m.size), if ane { " · " } else { "" }), s);
    if ane {
      ui.with_id(row.key("ane"))
        .help("Runs on the Apple Neural Engine")
        .text("ANE", s.underline(LinePattern::Dot));
    }
  });
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
  let danger = ui.theme().danger;
  let caption = move |ui: &mut Ui, s: TextStyle| {
    if let Some(caption) = &shown.caption {
      // A failure in red; cut short, it shows whole in its tooltip.
      let s = if shown.failed { s.color(danger) } else { s };
      if shown.failed {
        ui.help(caption);
      }

      ui.with_id(id.key("caption")).text(caption, s);
    }

    if let Some((k, text)) = &shown.progress {
      progress_line(ui, fs, *k, text);
    }
  };
  ui.with_id(id).form_row(fs, row.caption(caption), controls);
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
  /// The session's history is the one in memory: this one too, unless it could not open.
  in_memory: bool,
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
  /// The dictations selected, by id: the one the panel under the list shows, or how many.
  selected: Selection,
  /// What the line under the panel says for [NOTE_TIME], and since when ([Ui::now]): an error,
  /// "Copied".
  note: Option<(String, Duration)>,
  /// A note from outside a frame (a request that failed on its thread, a recording that could not
  /// play), stamped when History shows next.
  said: Option<String>,
}

impl HistoryPage {
  /// The session took the other history: it is read from now on, from scratch but the search.
  fn reopen(&mut self, home: &Path, in_memory: bool) {
    let db = History::open_as(home, in_memory).inspect_err(|e| warn!("history: {e:#}")).ok();
    let search = std::mem::take(&mut self.search);
    *self = Self { db: db.map(Rc::new), in_memory, search, ..Self::default() };
  }

  /// The search field changed: its wait starts again, unless only spaces around it did.
  fn typed(&mut self, now: Duration) {
    let search = self.search.trim();
    if self.typed.as_ref().is_none_or(|(s, _)| s != search) {
      self.typed = Some((search.to_owned(), now));
    }
  }

  /// Reads the history again when it or the search changed; a search being typed keeps the last
  /// result until it stays the same for [SEARCH_WAIT]. Returns how long it still waits. The
  /// selection keeps those read again: not deleted, still in the search.
  fn load(&mut self, now: Duration) -> Option<Duration> {
    let search = self.search.trim().to_owned();
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
      Ok(entries) => {
        if !self.selected.is_empty() {
          let read: HashSet<u64> = entries.iter().map(|e| e.id as u64).collect();
          self.selected.retain(|k| read.contains(&k));
        }

        (self.days, self.entries) = (days(&entries), entries);
      }
      Err(e) => self.note = Some((format!("Could not read the history: {e:#}"), now)),
    }

    None
  }

  /// The selected dictations, newest first.
  fn chosen(&self) -> Vec<&Entry> {
    if self.selected.is_empty() {
      return Vec::new();
    }

    self.entries.iter().filter(|e| self.selected.contains(e.id as u64)).collect()
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
fn two_line_h(ui: &mut Ui, fs: &FormStyle) -> f32 {
  2.0 * fs.pad_y + ui.line_height(fs.label) + 2.0 + ui.line_height(fs.caption)
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
  Copy(String),
  Play(i64),
  Stop,
  CopyPath(i64),
  Reveal(i64),
  Request(Request),
  /// Deleting the selection ([Prefs::ask_delete]).
  Delete,
  /// Transcribing the selection again: each one with its recording that isn't busy.
  RetranscribeChosen,
}

/// The panel under the list, with the selected dictation's whole text and its actions.
const FOOT_H: f32 = 150.0;
/// Play's and Transcribe Again's tip on a dictation without a recording: in memory, imported
/// without one.
const NO_RECORDING: &str = "Its recording isn’t kept";
/// The line under it with the whole history.
const TOTAL_H: f32 = 30.0;

impl Prefs {
  fn history_head(&mut self, ui: &mut Ui, s: Style) -> Response {
    let h = &mut self.history;
    ui.id(PAGE_HEAD).row(s, |ui| {
      if ui.id("history-search").search(&mut h.search, "Search dictations").changed {
        h.typed(ui.now());
      }
    })
  }

  /// The dictations, newest first, on a plate a day: nimbo's list, which builds only the rows in
  /// view and selects them by click and key; a right-click opens the menu of the row it selected,
  /// or of several.
  fn history(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let h = &mut self.history;
    if let Some(said) = h.said.take() {
      h.note = Some((said, ui.now()));
    }

    if let Some(wait) = h.load(ui.now()) {
      ui.request_frame_in(wait);
    }

    if h.entries.is_empty() {
      let empty = match (h.search.trim().is_empty(), h.in_memory) {
        (true, false) => "No dictations yet: hold the shortcut and speak.",
        (true, true) => "No dictations since Wavo started: they’re kept only until it quits.",
        (false, _) => "Nothing found",
      };
      ui.col(fs.page.pads(PAGE_PAD).h(grow()), |ui| {
        ui.form_section(fs, "", |ui| ui.form_row(fs, FormRow::new(empty), |_| {}));
      });
      return;
    }

    let (now, mut act) = (now_ms(), None);
    let fs = &FormStyle { caption: fs.caption.line_limit(1), ..*fs };
    let HistoryPage { entries, days, selected, playing, .. } = &mut self.history;
    let status = &self.status;
    let starts: Vec<usize> = days.iter().map(|(_, r, _)| r.start).collect();
    let line = ui.line_height(fs.head);
    let row_h = two_line_h(ui, fs);
    let head = |ui: &mut Ui, g: usize| {
      let (day, r, ms) = &days[g];
      ui.text(day_title(*day, now), fs.head);
      ui.spacer();
      ui.text(day_note((r.len(), *ms)), fs.note);
    };
    let key = |i: usize| entries[i].id as u64;
    let list = List::new(entries.len(), row_h).key(&key).multi().selection(selected);
    let list = list.sections(&starts, line + fs.head_pad[2], &head).list_style(ListStyle::Grouped);
    if fs.fade > 0.0 {
      ui.fade_edges(fs.bg, fs.fade);
    }

    let list = list.style(Style::new().grow().pads(PAGE_PAD));
    let r = list.show(ui.id("history-list"), |ui, row| {
      let e = &entries[row.index];
      let playing = playing.as_ref().is_some_and(|(id, _)| *id == e.id);
      let working = status.retranscribing.contains(&e.id) || status.current == Some(e.id);
      if let Some(a) = entry_row(ui, fs, e, playing, working) {
        act = Some(a);
      }
    });
    if r.delete {
      act = Some(Act::Delete);
    }

    let h = &self.history;
    let chosen = h.chosen();
    if let Some(&e) = chosen.first() {
      let item = ui.context_menu(r.id, || {
        let st = RowState::of(self, e);
        match chosen.len() {
          1 => row_menu(e, &st, h.db.as_deref().and_then(|db| db.audio_file(e)).is_some()),
          _ => several_menu(&st, chosen.iter().any(|x| x.audio_path.is_some())),
        }
      });
      act = item.map(|item| row_act(item, e)).or(act);
    }

    self.history_act(ui, act);
  }

  /// Under the list, where it stays: the selected dictation's whole text, Transcribe Again,
  /// Restore the Previous Text and Delete; several selected, how many and Delete.
  fn history_foot(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let chosen = self.history.chosen();
    let mut act = None;
    ui.el(Style::new().w(grow()).h(px(fs.hairline)).bg(fs.divider));
    let s = Style::new().w(grow()).h(px(FOOT_H)).pads([12.0, 20.0, 16.0, 20.0]).gap(10.0);
    ui.col(s, |ui| {
      let e = match chosen[..] {
        [] => {
          let muted = t.label().color(t.muted);
          ui.text("Select a dictation to see all of its text.", muted);
          return;
        }
        [e] => e,
        _ => {
          ui.id("history-chosen").text(format!("{} dictations selected", chosen.len()), t.body());
          ui.el(Style::new().h(grow()));
          ui.row(Style::new().w(grow()), |ui| {
            ui.el(Style::new().w(grow()));
            let busy = self.selection_busy();
            if ui.disabled(busy).id("delete-chosen").button("Delete…").clicked {
              act = Some(Act::Delete);
            }
          });
          return;
        }
      };

      let text = if e.text.is_empty() { no_text(e.tried_model.as_deref()) } else { e.text.clone() };
      let area = Style::new().w(grow()).h(grow());
      ui.id("history-text").scroll_area(area, |ui| {
        ui.col(Style::new().w(grow()), |ui| ui.text(&text, t.body()));
      });
      let st = RowState::of(self, e);
      ui.row(Style::new().w(grow()).gap(8.0), |ui| {
        let off = !st.retranscribes(e);
        if e.audio_path.is_none() {
          ui.help(NO_RECORDING);
        }

        if ui.disabled(off).id(("retranscribe", e.id)).button("Transcribe Again").clicked {
          act = Some(Act::Request(Request::Retranscribe(e.id)));
        }

        if st.restorable
          && ui.disabled(st.busy).id(("restore", e.id)).button("Restore the Previous Text").clicked
        {
          act = Some(Act::Request(Request::Restore(e.id)));
        }

        ui.el(Style::new().w(grow()));
        if ui.disabled(st.busy).id(("delete", e.id)).button("Delete…").clicked {
          act = Some(Act::Delete);
        }
      });
    });

    self.history_act(ui, act);
  }

  /// Under the panel, the whole history: `86,286 dictations · 312 h · since March 14, 2026`, in
  /// memory `3 dictations · 12 s · kept until Wavo quits`; for [NOTE_TIME] after a note, the note.
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
    let mut text = format!("{} {what} · {spoken}", thousands(n));
    if self.history.in_memory {
      return Some(text + " · kept until Wavo quits");
    }

    let since = local_time(first).map(|t| format!("{} {}, {}", t.month_name(), t.day, t.year));
    if let Some(since) = since {
      text += &format!(" · since {since}");
    }

    Some(text)
  }

  fn history_act(&mut self, ui: &mut Ui, act: Option<Act>) {
    let h = &mut self.history;
    match act {
      Some(Act::Copy(text)) => {
        ui.clipboard().write(&text);
        h.note = Some(("Copied to the clipboard".into(), ui.now()));
      }
      Some(act @ (Act::Play(id) | Act::CopyPath(id) | Act::Reveal(id))) => {
        let e = h.entries.iter().find(|e| e.id == id).unwrap();
        let Some(path) = h.db.as_ref().unwrap().audio_file(e) else {
          h.note = Some(("The recording is gone".into(), ui.now()));
          return;
        };

        match act {
          Act::CopyPath(_) => {
            let path = std::path::absolute(&path).unwrap_or(path);
            ui.clipboard().write(&path.to_string_lossy());
            h.note = Some(("Copied to the clipboard".into(), ui.now()));
          }
          Act::Reveal(_) => self.shell.reveal(&path),
          _ => match self.shell.play(&path) {
            Ok(p) => h.playing = Some((id, p)),
            Err(e) => h.note = Some((format!("Could not play the recording: {e:#}"), ui.now())),
          },
        }
      }
      Some(Act::Stop) => h.playing = None,
      Some(Act::Request(req)) => {
        h.note = None;
        self.shell.history(req);
      }
      Some(Act::Delete) => self.ask_delete(),
      Some(Act::RetranscribeChosen) => {
        h.note = None;
        let busy = &self.status.retranscribing;
        let chosen = h.chosen().into_iter().filter(|e| e.audio_path.is_some());
        let ids: Vec<i64> = chosen.map(|e| e.id).filter(|id| !busy.contains(id)).collect();
        for id in ids {
          self.shell.history(Request::Retranscribe(id));
        }
      }
      None => {}
    }
  }

  /// Asks to delete the selected dictations; not while one of them is being transcribed again.
  fn ask_delete(&mut self) {
    let doomed = self.history.chosen();
    let busy = |e: &&Entry| self.status.retranscribing.contains(&e.id);
    if doomed.is_empty() || doomed.iter().any(busy) {
      return;
    }

    let ids = doomed.iter().map(|e| e.id).collect();
    let audio = doomed.iter().any(|e| e.audio_path.is_some());
    let (title, message) = match doomed[..] {
      [e] => {
        let start = crate::history::shorten(&first_line(e), 60);
        let and = if audio { " and its recording are" } else { " is" };
        ("Delete this dictation?".into(), format!("“{start}”{and} removed from the history."))
      }
      _ => {
        let n = doomed.len();
        let and = if audio { " and their recordings are" } else { " are" };
        (
          format!("Delete {n} dictations?"),
          format!("{n} dictations{and} removed from the history."),
        )
      }
    };
    self.ask = Some(Ask { title, message, delete: Delete::Entries(ids) });
  }

  /// One of the selection is being transcribed again: it can't be deleted.
  fn selection_busy(&self) -> bool {
    let h = &self.history;
    self.status.retranscribing.iter().any(|&id| h.selected.contains(id as u64))
  }

  /// The recording playing ended, or could not play ([Shell::play]): the speakers are let go
  /// whatever page shows, and why it could not play is said under the panel.
  pub fn played(&mut self) {
    let h = &mut self.history;
    if let Some((_, p)) = h.playing.take_if(|(_, p)| !p.playing())
      && let Some(e) = p.error()
    {
      h.said = Some(format!("Could not play the recording: {e}"));
    }
  }
}

/// A dictation as its menu and the panel under the list show it: the model `transcribes`; `busy`,
/// being transcribed again, when the session refuses to change it; its previous text `restorable`;
/// `deletable` with the selection it is one of.
#[derive(Default)]
struct RowState {
  playing: bool,
  transcribes: bool,
  busy: bool,
  restorable: bool,
  deletable: bool,
}

impl RowState {
  fn of(p: &Prefs, e: &Entry) -> RowState {
    let (h, status) = (&p.history, &p.status);
    let busy = status.retranscribing.contains(&e.id);
    let selected = h.selected.contains(e.id as u64);
    RowState {
      deletable: if selected { !p.selection_busy() } else { !busy },
      playing: h.playing.as_ref().is_some_and(|(id, _)| *id == e.id),
      transcribes: p.transcribes(),
      busy,
      restorable: status.restorable.contains(&e.id),
    }
  }

  /// Transcribe Again is on: the model transcribes, the recording is there, and it isn't busy.
  fn retranscribes(&self, e: &Entry) -> bool {
    self.transcribes && !self.busy && e.audio_path.is_some()
  }
}

/// A dictation's row, all of one height: its text on a line and the details under it, Play and
/// Copy at its right; the list selects it and draws the selection.
fn entry_row(ui: &mut Ui, fs: &FormStyle, e: &Entry, playing: bool, working: bool) -> Option<Act> {
  ui.col(Style::new().w(grow()).gap(2.0), |ui| {
    ui.text(row_line(e, working), fs.label.line_limit(1));
    ui.text(details(e), fs.caption);
  });

  let mut act = None;
  let tip = match () {
    _ if playing => "Stop",
    _ if e.audio_path.is_none() => NO_RECORDING,
    _ => "Play the recording",
  };
  let r = ui.help(tip).disabled(e.audio_path.is_none()).control_size(ControlSize::Large);
  let r = r.id(("play", e.id)).icon_button(if playing { Icon::STOP } else { Icon::PLAY });
  if r.clicked {
    act = Some(if playing { Act::Stop } else { Act::Play(e.id) });
  }

  let r = ui.help("Copy").disabled(e.text.is_empty()).control_size(ControlSize::Large);
  let r = r.id(("copy", e.id)).icon_button(Icon::COPY);
  if r.clicked {
    act = Some(Act::Copy(e.text.clone()));
  }

  act
}

/// What a row's context menu does ([row_menu], [several_menu]), to the row or the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowItem {
  Copy,
  Play,
  Stop,
  CopyPath,
  Reveal,
  Retranscribe,
  Restore,
  RetranscribeChosen,
  Delete,
}

/// A row's context menu: what its buttons and the panel under the list do; with its recording
/// `on_disk`, its path copied and the file shown in the file manager.
fn row_menu(e: &Entry, st: &RowState, on_disk: bool) -> Menu<RowItem> {
  let (play, label) = match st.playing {
    true => (RowItem::Stop, "Stop"),
    false => (RowItem::Play, "Play the Recording"),
  };
  let mut items = vec![
    Item::button(RowItem::Copy, "Copy Text").enabled(!e.text.is_empty()),
    Item::button(play, label).enabled(e.audio_path.is_some()),
  ];
  if on_disk {
    let show = format!("Show in {}", open::file_manager());
    let path = Item::button(RowItem::CopyPath, "Copy Recording Path");
    items.extend([path, Item::button(RowItem::Reveal, &show)]);
  }

  items.extend([
    Item::Divider,
    Item::button(RowItem::Retranscribe, "Transcribe Again").enabled(st.retranscribes(e)),
  ]);
  if st.restorable {
    items.push(Item::button(RowItem::Restore, "Restore the Previous Text").enabled(!st.busy));
  }

  let delete = Item::button(RowItem::Delete, "Delete…").role(ButtonRole::Destructive);
  items.extend([Item::Divider, delete.enabled(st.deletable)]);
  Menu::new(items)
}

/// The menu of a row among several selected: what is done to all of them. Transcribe Again is on
/// when the model transcribes, one of them is `recorded`, and none is busy, as with Delete.
fn several_menu(st: &RowState, recorded: bool) -> Menu<RowItem> {
  let again = st.transcribes && st.deletable && recorded;
  let delete = Item::button(RowItem::Delete, "Delete…").role(ButtonRole::Destructive);
  Menu::new(vec![
    Item::button(RowItem::RetranscribeChosen, "Transcribe Again").enabled(again),
    Item::Divider,
    delete.enabled(st.deletable),
  ])
}

/// The act of the row menu's item `item`, whatever the row is when the answer comes: Stop, chosen
/// as the playback ended, doesn't play it again.
fn row_act(item: RowItem, e: &Entry) -> Act {
  match item {
    RowItem::Copy => Act::Copy(e.text.clone()),
    RowItem::Play => Act::Play(e.id),
    RowItem::Stop => Act::Stop,
    RowItem::CopyPath => Act::CopyPath(e.id),
    RowItem::Reveal => Act::Reveal(e.id),
    RowItem::Retranscribe => Act::Request(Request::Retranscribe(e.id)),
    RowItem::Restore => Act::Request(Request::Restore(e.id)),
    RowItem::RetranscribeChosen => Act::RetranscribeChosen,
    RowItem::Delete => Act::Delete,
  }
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

impl Prefs {
  fn about(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let head =
      Style::new().w(grow()).pads([10.0, 0.0, 6.0, 0.0]).align(Align::Center, Align::Start);
    ui.col(head, |ui| {
      // 128 px: 64 points at 2x.
      let icon = nimbo::Png::new(crate::ICON).side(128);
      ui.image(icon, Style::new().w(px(64.0)).h(px(64.0)));
      ui.el(Style::new().h(px(10.0)));
      let title = TextStyle::new(Font { size: 20.0, ..t.bold }, t.text).line_limit(1);
      ui.text(crate::INFO.name, title);
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

        // Only where the app can update.
        if self.shell.updates() && ui.id("updates").button("Check for Updates…").clicked {
          self.shell.check_updates();
        }
      });
    });

    // Denser than a setting's rows: six of them.
    let fs = &FormStyle { caption: fs.caption.line_limit(1), pad_y: 5.0, ..*fs };
    // A row's controls go on its name's line: the ↗ is centred in the whole row instead.
    let row_h = two_line_h(ui, fs);
    ui.form_section(fs, "Acknowledgements", |ui| {
      for (name, what, url) in ACKS {
        ui.form_row(fs, FormRow::new(name).caption(what), |ui| {
          ui.col(Style::new().h(px(row_h)).align(Align::Center, Align::Center), |ui| {
            let ui = ui.help(url.trim_start_matches("https://")).control_size(ControlSize::Large);
            if ui.id(("link", name)).icon_button(Icon::EXTERNAL).clicked {
              self.shell.open_url(url);
            }
          });
        });
      }
    });
  }
}

#[cfg(test)]
mod tests {
  use std::cell::{RefCell, RefMut};
  use std::rc::Rc;

  use nimbo::harness::mac_titlebar;
  use nimbo::{
    Appearance, Event, Harness, Modifiers, MouseButton, NamedKey, Os, Phase, Point, Size,
  };

  use super::*;
  use crate::session::EngineStatus;
  use crate::testing::TempDir;

  #[derive(Default)]
  struct Log {
    settings: Settings,
    /// A Mac with a Neural Engine unless set.
    no_neural_engine: bool,
    paused: Vec<bool>,
    /// A MacBook's microphone and a USB one unless set.
    devices: Option<Vec<audio::Device>>,
    /// Times the devices were read.
    looks: usize,
    lid_closed: bool,
    layouts: Vec<(String, String)>,
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
    updates: bool,
    checks: usize,
    played: Vec<PathBuf>,
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
      !self.log().no_neural_engine
    }

    fn pause_hotkey(&self, paused: bool) {
      self.log().paused.push(paused);
    }

    fn devices(&self) -> Vec<audio::Device> {
      let mut log = self.log();
      log.looks += 1;
      if let Some(devices) = &log.devices {
        return devices.clone();
      }

      let device = |name: &str, default, transport| audio::Device {
        id: Some(name.to_lowercase()),
        name: name.into(),
        default,
        transport,
      };
      let mac = device("MacBook Pro Microphone", true, Transport::BuiltIn);
      vec![mac, device("USB Mic", false, Transport::Usb)]
    }

    fn lid_closed(&self) -> bool {
      self.log().lid_closed
    }

    fn keyboard_layouts(&self) -> Vec<(String, String)> {
      self.log().layouts.clone()
    }

    fn access(&self) -> Access {
      let all = Access {
        accessibility: true,
        needs_admin: false,
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

    /// No speakers in tests.
    fn play(&self, file: &Path) -> anyhow::Result<Playback> {
      self.log().played.push(file.to_owned());
      Err(anyhow::anyhow!("no speakers"))
    }

    fn updates(&self) -> bool {
      self.log().updates
    }

    fn check_updates(&self) {
      self.log().checks += 1;
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
      let in_memory = settings.history_retention.in_memory();
      let history = History::open_as(self.dir.path(), in_memory).ok().map(Rc::new);
      let prefs = Prefs::new(shell, settings, self.hub.clone(), self.dir.path(), history);
      let cfg = config(Page::General);
      let mut h = Harness::new(prefs, cfg.size, 2.0);
      // macOS 26's, as nimbo's own shots draw it.
      h.set_titlebar(Some(mac_titlebar(cfg.title, false)));
      h.event(Event::Appearance(Appearance::LIGHT));
      h.frame();
      h.frame();
      h
    }

    /// [add_dictations] to the saved history, with a recording each.
    fn dictations(&self, items: &[(&str, i64)]) -> Vec<i64> {
      add_dictations(&History::open(self.dir.path()).unwrap(), items)
    }
  }

  /// Dictations `(text, minutes ago)` into `h`, the oldest first, with a recording each where it
  /// keeps them: one without text was tried by whisper-tiny. Their ids.
  fn add_dictations(h: &History, items: &[(&str, i64)]) -> Vec<i64> {
    let now = now_ms();
    let entries: Vec<Entry> = (items.iter())
      .map(|&(text, ago)| {
        let created_at = now - ago * 60_000;
        let audio_path = h.recordings.as_ref().map(|r| {
          let (name, path) = r.new_file(created_at);
          crate::testing::write_wav(&path, &[0.0; 1_600]);
          name
        });
        let model = "parakeet-tdt-0.6b-v3-Q8_0".to_owned();
        Entry {
          created_at,
          duration_ms: 1_500 + 70 * text.len() as i64,
          audio_path,
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

  const TWO: [&str; 2] = ["parakeet-v3", "parakeet-v2"];

  /// Clicks `id` and lets the frames after it settle (a select's list opens on the next one).
  fn click(h: &mut Harness<Prefs>, id: Id) {
    assert!(h.rect(id).is_some(), "{id:?} is not shown");
    h.click_id(id);
    h.frame();
    h.frame();
  }

  fn tab(h: &mut Harness<Prefs>, page: Page) {
    let i = PAGES.iter().position(|p| p.value == page).unwrap();
    click(h, Id::of(TABS).key(i));
    h.frame();
    assert_eq!(h.app().page, page);
  }

  /// Dictation `id`'s row in the history's list.
  fn row(id: i64) -> Id {
    Id::of("history-list").key(("row", id as u64))
  }

  /// The ids of the dictations selected, smallest first.
  fn selected(h: &Harness<Prefs>) -> Vec<i64> {
    let mut ids: Vec<i64> = h.app().history.selected.keys().map(|k| k as i64).collect();
    ids.sort();
    ids
  }

  /// The dialog's Delete, once it has faded in.
  fn confirm(h: &mut Harness<Prefs>) {
    assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "the dialog");
    h.wait(Duration::from_millis(200));
    click(h, Id::of(CONFIRM).key("action"));
  }

  /// The accessibility tree's line that starts with `prefix`, as assistive technology reads it.
  fn a11y_line(h: &Harness<Prefs>, prefix: &str) -> Option<String> {
    let tree = h.a11y();
    tree.lines().map(str::trim).find(|l| l.starts_with(prefix)).map(str::to_owned)
  }

  /// Rests the pointer on the mark of the row `row` (its ⓘ, or the warning in its place) until
  /// its tooltip shows; the tooltip's text.
  fn tip(h: &mut Harness<Prefs>, row: &str) -> String {
    let mark = Id::of(row).key("info");
    let r = h.rect(mark).unwrap_or_else(|| panic!("no mark in {row}"));
    h.move_to(r.x + r.w / 2.0, r.y + r.h / 2.0);
    h.wait(Duration::from_millis(400));
    let tip = mark.key("tip");
    assert!(h.rect(tip).is_some(), "no tooltip on the mark of {row}");
    let text = h.text(tip.nth(0)).unwrap_or_default().to_owned();
    h.leave();
    h.frame();
    text
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

  /// A Mac's layouts: two of English, one of Russian.
  fn layouts() -> Vec<(String, String)> {
    let pairs = [("en", "U.S."), ("ru", "Russian – PC"), ("en", "ABC")];
    pairs.iter().map(|(l, n)| ((*l).to_owned(), (*n).to_owned())).collect()
  }

  /// Auto-detect with a model of one language says that language is every dictation's.
  #[test]
  fn auto_detect_says_what_a_model_of_one_language_does() {
    let home = Home::new("window-auto", &["parakeet-v2"]);
    let mut h = home.harness(&Fake::default(), Settings::default());
    let want = "parakeet-v2 knows English only, so every dictation is in English.";
    assert_eq!(tip(&mut h, "lang-row"), want);
  }

  /// Keyboard layout is a Language where the system tells the layouts. Chosen, it starts each of
  /// their languages on its best model downloaded, listed in the Language row by name: the
  /// default model, the models that know it as Models orders them, and Download more…, which
  /// opens Models in that language. A language's model is in use: Models won't delete it. One
  /// without a model of its own shows the default.
  #[test]
  fn a_language_per_keyboard_layout() {
    let home = Home::new("window-keyboard", &["parakeet-v3", "parakeet-v2", "gigaam-v3"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    assert_eq!(tip(&mut h, "lang-row"), AUTO_HELP);
    click(&mut h, Id::of("lang"));
    let item = |i: usize| Id::of("lang").key(("item", i));
    let items = |h: &Harness<Prefs>| (0..).take_while(|&i| h.rect(item(i)).is_some()).count();
    let without = items(&h);
    h.key(NamedKey::Escape, Modifiers::empty());
    h.frame();

    fake.log().layouts = layouts();
    assert!(h.event(Event::System(Change::InputSource)));
    h.frame();
    click(&mut h, Id::of("lang"));
    assert_eq!(items(&h), without + 1, "Keyboard layout is offered");
    click(&mut h, item(1));
    type Rows = BTreeMap<String, Option<String>>;
    let rows = |m: &[(&str, Option<&str>)]| {
      m.iter().map(|(l, m)| ((*l).into(), m.map(Into::into))).collect()
    };
    let started: Rows = rows(&[("en", Some("parakeet-v2")), ("ru", Some("gigaam-v3"))]);
    assert_eq!(fake.log().settings.language, KEYBOARD);
    assert_eq!(fake.log().settings.keyboard_models, started);
    assert_eq!(tip(&mut h, "lang-row"), KEYBOARD_HELP);
    assert_eq!(tip(&mut h, "model-row"), MODEL_KEYBOARD_HELP);

    let (en, ru) = (Id::of("keyboard-en"), Id::of("keyboard-ru"));
    assert!(h.rect(en).unwrap().y < h.rect(ru).unwrap().y, "English, then Russian");
    let named = vec![("en".into(), "U.S., ABC".into()), ("ru".into(), "Russian – PC".into())];
    assert_eq!(keyboard_languages(&layouts()), named);

    click(&mut h, ru);
    click(&mut h, ru.key(("item", 2)));
    let moved: Rows = rows(&[("en", Some("parakeet-v2")), ("ru", Some("parakeet-v3"))]);
    assert_eq!(fake.log().settings.keyboard_models, moved);
    assert!(h.app().in_use(hub::lookup("parakeet-v2").unwrap()));

    click(&mut h, ru);
    click(&mut h, ru.key(("item", 0)));
    let default: Rows = rows(&[("en", Some("parakeet-v2")), ("ru", None)]);
    assert_eq!(fake.log().settings.keyboard_models, default, "the default model");

    // Auto-detect and back: each language keeps what the person set, the default too.
    click(&mut h, Id::of("lang"));
    click(&mut h, item(0));
    assert_eq!(fake.log().settings.language, "auto");
    click(&mut h, Id::of("lang"));
    click(&mut h, item(1));
    assert_eq!(fake.log().settings.language, KEYBOARD);
    assert_eq!(fake.log().settings.keyboard_models, default, "kept");

    click(&mut h, ru);
    click(&mut h, ru.key(("item", 3)));
    assert_eq!(h.app().page, Page::Models);
    assert_eq!(LANGUAGES[h.app().model_lang].1, "ru");

    let alone = Settings {
      language: KEYBOARD.into(),
      model: Some("parakeet-v2".into()),
      ..Settings::default()
    };
    let h = home.harness(&fake, alone);
    let (labels, models, at) = h.app().keyboard_choices("ru");
    assert_eq!(models, ["gigaam-v3", "parakeet-v3"]);
    assert_eq!((at, labels[at].as_str()), (0, "parakeet-v2 (default)"));
  }

  /// Every page in both looks, as high as its content or [SCROLLING] high; with `WAVO_SHOTS` set
  /// (`make shots`), written there: also Advanced in a build that doesn't update and asking before
  /// Delete All, History in memory, Models asking before a delete and General at the first launch.
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
    // This run's dictations, in memory without their recordings.
    let memory = History::open_memory(home.dir.path()).unwrap();
    let this_run = add_dictations(
      &memory,
      &[
        ("Book the train to Lviv for Friday morning.", 25),
        ("Ask Anna for the draft of the release notes.", 9),
        ("Купить хлеб, сыр и кофе.", 2),
      ],
    );
    let first_run = Home::new("window-shots-first-run", &[]);
    let fits = Home::new("window-shots-fit", &["parakeet-ultra", "gigaam-v3", "whisper-turbo"]);
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
      fake.log().updates = true;
      let mut h = home.harness(&fake, Settings::default());
      h.event(Event::Appearance(appearance));
      for TabItem { value: page, label: name, .. } in PAGES {
        tab(&mut h, page);
        shot(&mut h, &format!("{}-{look}", name.to_lowercase()));
        let size = h.size();
        let area = match page {
          Page::History => Id::of("history-list"),
          _ => Id::of(("page", page as u8)),
        };
        let sc = h.scroll(area).expect("the page's scroll area");
        match page.scrolls() {
          true => assert_eq!(size, Size::new(WIDTH, SCROLLING + 32.0), "{name}: and the title bar"),
          false => {
            let blank = sc.viewport.h - sc.content.h;
            assert!((-0.5..=1.0).contains(&blank) && size.w == WIDTH, "{name} fits: {sc:?}");
          }
        }

        // The window's ground under the toolbar's edge: dark in the dark look.
        let ground = h.render().pixel(4.0, 100.0);
        assert_eq!(ground[0] < 100, appearance.scheme.is_dark(), "{name} {look}: {ground:?}");
      }

      // Language by the keyboard layout, Russian to its own model.
      let keyboard = Fake::default();
      keyboard.log().layouts = layouts();
      let rows = BTreeMap::from([("ru".to_owned(), Some("gigaam-v3".to_owned()))]);
      let settings = Settings {
        language: KEYBOARD.into(),
        model: Some("parakeet-v3".into()),
        keyboard_models: rows,
        ..Settings::default()
      };
      let mut k = home.harness(&keyboard, settings);
      k.event(Event::Appearance(appearance));
      shot(&mut k, &format!("general-keyboard-{look}"));

      fake.log().updates = false;
      tab(&mut h, Page::Advanced);
      shot(&mut h, &format!("advanced-no-updates-{look}"));

      tab(&mut h, Page::History);
      click(&mut h, row(ids[6]));
      shot(&mut h, &format!("history-selected-{look}"));
      // The panel's long text scrolls under the wheel.
      let panel = Id::of("history-text");
      let sc = h.scroll(panel).expect("the panel's scroll area");
      assert!(sc.content.h > sc.viewport.h, "{look}: the text overflows: {sc:?}");
      let v = sc.viewport;
      let (pos, delta) = (Point::new(v.x + v.w / 2.0, v.y + v.h / 2.0), Point::new(0.0, -40.0));
      let (phase, momentum, mods) = (Phase::None, Phase::None, Modifiers::default());
      h.event(Event::Scroll { pos, delta, precise: true, phase, momentum, mods });
      h.frame();
      assert!(h.scroll(panel).unwrap().offset.y > 0.0, "{look}: it scrolled");

      tab(&mut h, Page::Models);
      click(&mut h, Id::of(("delete", "whisper-turbo")));
      assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "{look}: the dialog");
      shot(&mut h, &format!("models-delete-{look}"));

      // Delete All asks first; History in memory shows this run's.
      let fake = Fake::default();
      let mut h = home.harness(&fake, Settings::default());
      h.event(Event::Appearance(appearance));
      tab(&mut h, Page::Advanced);
      click(&mut h, Id::of("delete-all"));
      assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "{look}: the dialog");
      shot(&mut h, &format!("advanced-delete-all-{look}"));
      let settings = Settings { history_retention: Retention::UntilQuit, ..Settings::default() };
      let mut h = home.harness(&fake, settings);
      h.event(Event::Appearance(appearance));
      tab(&mut h, Page::History);
      click(&mut h, row(this_run[1]));
      shot(&mut h, &format!("history-memory-{look}"));

      // Models by language, the meter's numbers on hover.
      let fake = Fake::default();
      let settings = Settings { model: Some(RECOMMENDED.into()), ..Settings::default() };
      let mut h = fits.harness(&fake, settings);
      h.event(Event::Appearance(appearance));
      tab(&mut h, Page::Models);
      for code in ["ru", "el", "he", "ka", "sq", ""] {
        h.app_mut().model_lang = LANGUAGES.iter().position(|l| l.1 == code).unwrap();
        h.frame();
        let name = if code.is_empty() { "all" } else { code };
        shot(&mut h, &format!("models-{name}-{look}"));
        // The speeds and their bolts line up whatever the button.
        for part in [None, Some("bolt")] {
          let id = |m: &ModelInfo| {
            part.map_or(Id::of(("speed", m.name)), |p| Id::of(("speed", m.name)).key(p))
          };
          let x: Vec<f32> = hub::MODELS.iter().filter_map(|m| h.rect(id(m))).map(|r| r.x).collect();
          assert!(x.len() >= 2 && x.iter().all(|&x0| x0 == x[0]), "{name} {part:?}: {x:?}");
        }
      }

      h.app_mut().model_lang = LANGUAGES.iter().position(|l| l.1 == "ru").unwrap();
      h.frame();
      click(&mut h, Id::of(("other-models", "ru")));
      shot(&mut h, &format!("models-ru-other-{look}"));
      // Other models hovered, then held down.
      let r = h.rect(Id::of(("other-models", "ru"))).unwrap();
      let (x, y) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
      h.move_to(x, y);
      h.wait(Duration::from_millis(400));
      if let Some(dir) = &dir {
        h.shot(&dir.join(format!("models-other-hover-{look}.png"))).unwrap();
        h.press(x, y);
        h.frame();
        h.shot(&dir.join(format!("models-other-pressed-{look}.png"))).unwrap();
        h.release(x + 400.0, y);
        h.frame();
      }

      let r = h.rect(Id::of(("speed", RECOMMENDED))).expect("the bolts");
      h.move_to(r.x + r.w / 2.0, r.y + r.h / 2.0);
      h.wait(Duration::from_millis(400));
      let tip = Id::of(("speed", RECOMMENDED)).key("tip");
      let text = "6.7× faster than whisper-turbo";
      assert_eq!(h.text(tip.nth(0)), Some(text));
      let read = format!("static text \"6.7×\" desc {text:?}");
      assert!(h.a11y().lines().any(|l| l.trim() == read), "{look}: read\n{}", h.a11y());
      if let Some(dir) = &dir {
        h.shot(&dir.join(format!("models-tip-{look}.png"))).unwrap();
      }

      // What the WER means, on the recommended one.
      h.leave();
      let wer = Id::of(("model", recommended("ru").unwrap().name)).key("wer");
      let r = h.rect(wer).expect("the WER");
      h.move_to(r.x + r.w / 2.0, r.y + r.h / 2.0);
      h.wait(Duration::from_millis(400));
      let text = "Errors per 100 words";
      assert_eq!(h.text(wer.key("tip").nth(0)), Some(text));
      if let Some(dir) = &dir {
        h.shot(&dir.join(format!("models-wer-tip-{look}.png"))).unwrap();
      }

      // The three downloaded selected, then the dialog for the two not in use.
      h.leave();
      h.set_focus(Some(Id::of("downloaded")));
      h.key('a', nimbo::CMD);
      h.frame();
      assert_eq!(h.app().model_sel.len(), 3, "{look}");

      shot(&mut h, &format!("models-selected-{look}"));
      // Scrolled, the header goes under the search with the rows.
      let page = Id::of(("page", Page::Models as u8));
      let v = h.scroll(page).expect("the page's scroll area").viewport;
      let wheel = |h: &mut Harness<Prefs>, dy: f32| {
        let (pos, delta) = (Point::new(v.x + v.w / 2.0, v.y + v.h / 2.0), Point::new(0.0, dy));
        let (phase, momentum, mods) = (Phase::None, Phase::None, Modifiers::default());
        h.event(Event::Scroll { pos, delta, precise: true, phase, momentum, mods });
        h.frame();
      };
      wheel(&mut h, -20.0);
      assert!(h.scroll(page).unwrap().offset.y > 0.0, "{look}: it scrolled");
      shot(&mut h, &format!("models-selected-scrolled-{look}"));
      wheel(&mut h, 20.0);
      h.key(NamedKey::Delete, Modifiers::empty());
      h.frame();
      assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "{look}: the dialog");
      shot(&mut h, &format!("models-delete-many-{look}"));

      let fake = Fake::default();
      let none = Access {
        accessibility: false,
        needs_admin: false,
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

  /// About's Check for Updates… opens the update's window, Advanced turns the automatic checks off;
  /// in a build that doesn't update About has no button and Advanced's switch is off and disabled,
  /// the setting kept on, its row's ⓘ enabled and saying why.
  #[test]
  fn about_checks_for_updates() {
    let home = Home::new("window-update", &TWO);
    let fake = Fake::default();
    // On, as by default.
    let mut h = home.harness(&fake, Settings::default());
    assert!(fake.log().settings.auto_update);
    let switch = |h: &Harness<Prefs>| a11y_line(h, r#"switch "Check for updates""#);
    tab(&mut h, Page::About);
    assert!(h.rect(Id::of("updates")).is_none());
    tab(&mut h, Page::Advanced);
    assert_eq!(switch(&h).as_deref(), Some(r#"switch "Check for updates" off disabled"#));
    let info = format!(r#"button "Info" desc {NO_UPDATES:?}"#);
    assert_eq!(a11y_line(&h, &info), Some(info), "enabled");
    assert_eq!(tip(&mut h, "auto-update-row"), NO_UPDATES);
    click(&mut h, Id::of("auto-update"));
    assert!(fake.log().settings.auto_update, "kept");

    fake.log().updates = true;
    h.app_mut();
    h.frame();
    assert_eq!(switch(&h).as_deref(), Some(r#"switch "Check for updates" on"#));
    assert_eq!(tip(&mut h, "auto-update-row"), UPDATE_HELP);
    click(&mut h, Id::of("auto-update"));
    assert!(!fake.log().settings.auto_update);
    tab(&mut h, Page::About);
    click(&mut h, Id::of("updates"));
    assert_eq!(fake.log().checks, 1);
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
    click(&mut h, Id::of("hud-style"));
    click(&mut h, Id::of("hud-style").key(("item", 1)));
    assert_eq!(fake.log().settings.hud_style, HudStyle::Plain);
    click(&mut h, Id::of("live"));
    assert!(fake.log().settings.live_preview);

    // Launch at login, on General, goes to the system, one change at a time; a refusal says why.
    tab(&mut h, Page::General);
    click(&mut h, Id::of("login"));
    click(&mut h, Id::of("login"));
    assert_eq!(fake.log().autostarts, [true]);
    h.app_mut().autostart_set(true, Err("not permitted".into()));
    assert!(!h.app().settings.autostart);
    assert_eq!(h.app().login.error.as_deref(), Some("Couldn't turn it on: not permitted"));
  }

  /// A chosen microphone that isn't connected stays the pop-up's choice, and the warning in its
  /// row's ⓘ's place names the one that records instead: the system's input, not Automatic's USB
  /// one. Its tooltip is the warning alone; no caption under the label. With no warning, the ⓘ.
  #[test]
  fn a_mic_not_connected_says_what_records() {
    let home = Home::new("window-mic", &TWO);
    let fake = Fake::default();
    let row = |h: &Harness<Prefs>| a11y_line(h, r#"pop up button "Microphone""#);
    let mark = |h: &Harness<Prefs>| a11y_line(h, r#"button "Warning""#);
    let info = format!(r#"button "Info" desc {MIC_HELP:?}"#);
    let wh = Choice { id: Some("wh-1000xm5".into()), name: "WH-1000XM5".into() };
    let wh = Microphone::Device(wh);
    let mut h = home.harness(&fake, Settings { microphone: wh.clone(), ..Settings::default() });
    let warning = "WH-1000XM5 isn’t connected: Wavo records with MacBook Pro Microphone";
    let pop = r#"pop up button "Microphone" value "WH-1000XM5 (not connected)""#;
    assert_eq!(row(&h).as_deref(), Some(pop), "no caption");
    assert_eq!(mark(&h), Some(format!(r#"button "Warning" desc {warning:?}"#)));
    assert_eq!(a11y_line(&h, &info), None, "no ⓘ beside the warning");
    assert_eq!(tip(&mut h, "mic-row"), warning);
    assert!(h.rect(Id::of("mic-row").key("caption")).is_none());

    let mut h = home.harness(&fake, Settings::default());
    let auto = r#"pop up button "Microphone" value "Automatic (USB Mic)""#;
    assert_eq!(row(&h).as_deref(), Some(auto));
    assert_eq!(mark(&h), None, "the ⓘ alone");
    assert_eq!(a11y_line(&h, &info).as_ref(), Some(&info));
    assert_eq!(tip(&mut h, "mic-row"), MIC_HELP);

    // Under a closed lid that one hears nothing: the warning says so.
    let lid = format!("{warning} (the lid is closed)");
    assert_eq!(mic_warning(&fake.devices(), &wh, true), Some(lid));

    // The system's input named by no listed device (ALSA's), or none listed: Linux has ALSA's
    // `default` all the same, elsewhere there is no input at all.
    let alsa = [audio::Device { default: false, ..fake.devices()[1].clone() }];
    let unnamed = "WH-1000XM5 isn’t connected: Wavo records with the system’s input";
    assert_eq!(mic_warning(&alsa, &wh, false).as_deref(), Some(unnamed));
    let none = match cfg!(target_os = "linux") {
      true => unnamed,
      false => "WH-1000XM5 isn’t connected, and there is no other microphone",
    };
    assert_eq!(mic_warning(&[], &wh, false).as_deref(), Some(none));
  }

  /// Automatic with only a Bluetooth microphone left warns in the row's ⓘ's place, the warning
  /// alone in its tip; with another one there, the ⓘ and its tip. The open window reads the
  /// microphones again on either audio change.
  #[test]
  fn only_a_bluetooth_mic_warns() {
    let home = Home::new("window-bluetooth", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    let airpods = audio::Device {
      id: Some("airpods".into()),
      name: "AirPods Pro".into(),
      default: true,
      transport: Transport::Bluetooth,
    };
    fake.log().devices = Some(vec![airpods]);
    assert!(h.event(Event::System(Change::DefaultInput)));
    h.frame();
    let warning = "Only a Bluetooth mic: the sound is worse";
    assert_eq!(tip(&mut h, "mic-row"), warning);

    fake.log().devices = None;
    assert!(h.event(Event::System(Change::AudioInputs)));
    h.frame();
    assert_eq!(tip(&mut h, "mic-row"), MIC_HELP);
  }

  /// The session's slow start warning shows in the Microphone row's ⓘ's place, saying what to do;
  /// gone, the ⓘ is back. The chosen one unplugged says so instead.
  #[test]
  fn a_slow_mic_says_to_replug_it() {
    let home = Home::new("window-slow", &TWO);
    let fake = Fake::default();
    let usb = Choice { id: Some("usb mic".into()), name: "USB Mic".into() };
    let settings = Settings { microphone: Microphone::Device(usb), ..Settings::default() };
    let mut h = home.harness(&fake, settings);
    let slow = Status { warnings: vec![mic_start::SLOW.into()], ..Status::default() };
    update(&mut h, slow.clone());
    assert_eq!(tip(&mut h, "mic-row"), SLOW_MIC);

    update(&mut h, Status::default());
    assert_eq!(tip(&mut h, "mic-row"), MIC_HELP);

    let mac = fake.devices()[0].clone();
    fake.log().devices = Some(vec![mac]);
    assert!(h.event(Event::System(Change::AudioInputs)));
    update(&mut h, slow);
    let gone = "USB Mic isn’t connected: Wavo records with MacBook Pro Microphone";
    assert_eq!(tip(&mut h, "mic-row"), gone);
  }

  /// The chosen microphone unplugged while the window is open: on the change, not before, the
  /// pop-up says it isn't connected and the warning names the one that records instead; the lid
  /// closed, that one hears nothing; plugged in again, the warning goes and the ⓘ is back.
  #[test]
  fn devices_and_the_lid_redraw_the_mic_row() {
    let home = Home::new("window-system", &TWO);
    let fake = Fake::default();
    let usb = Choice { id: Some("usb mic".into()), name: "USB Mic".into() };
    let settings = Settings { microphone: Microphone::Device(usb), ..Settings::default() };
    let mut h = home.harness(&fake, settings);
    let row = |h: &Harness<Prefs>| a11y_line(h, r#"pop up button "Microphone""#);
    let mark = |h: &Harness<Prefs>| a11y_line(h, r#"button "Warning""#);
    let pop = |value: &str| Some(format!(r#"pop up button "Microphone" value {value:?}"#));
    let warns = |w: &str| Some(format!(r#"button "Warning" desc {w:?}"#));
    assert_eq!(row(&h), pop("USB Mic"));
    assert_eq!(mark(&h), None);

    let mac = fake.devices()[0].clone();
    fake.log().devices = Some(vec![mac]);
    h.app_mut();
    assert!(h.frame());
    assert_eq!(row(&h), pop("USB Mic"), "not read again before the change");
    assert!(h.event(Event::System(Change::AudioInputs)));
    h.frame();
    let gone = "USB Mic isn’t connected: Wavo records with MacBook Pro Microphone";
    assert_eq!(row(&h), pop("USB Mic (not connected)"));
    assert_eq!(mark(&h), warns(gone));

    fake.log().lid_closed = true;
    assert!(h.event(Event::System(Change::Lid)));
    h.frame();
    assert_eq!(mark(&h), warns(&format!("{gone} (the lid is closed)")));

    fake.log().devices = None;
    assert!(h.event(Event::System(Change::AudioInputs)));
    h.frame();
    assert_eq!(row(&h), pop("USB Mic"));
    assert_eq!(mark(&h), None);
    assert_eq!(tip(&mut h, "mic-row"), MIC_HELP);
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
    let save = asked.kind == nimbo::FileDialogKind::Save;
    assert!(save && asked.name.unwrap().starts_with("wavo-export-"));
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

  /// Delete All asks first, saying how many dictations the history saved on disk has: Cancel sends
  /// nothing, Delete asks the session; off with nothing saved; its caption says when it waits. The
  /// history it deleted empties the History page and the row.
  #[test]
  fn delete_all_asks_first() {
    let empty = Home::new("window-delete-all-empty", &TWO);
    let fake = Fake::default();
    let mut h = empty.harness(&fake, Settings::default());
    tab(&mut h, Page::Advanced);
    let button = |h: &Harness<Prefs>| a11y_line(h, "button \"Delete All…\"").unwrap_or_default();
    let caption =
      |h: &Harness<Prefs>| h.text(Id::of("delete-row").key("caption")).map(str::to_owned);
    assert!(button(&h).contains("disabled"), "{}", button(&h));
    assert_eq!(caption(&h).as_deref(), Some(NOTHING_SAVED));
    click(&mut h, Id::of("delete-all"));
    assert!(h.app().ask.is_none(), "nothing to delete");

    let home = Home::new("window-delete-all", &TWO);
    home.dictations(&[("Let's move the design review.", 3), ("Send the invoice to Anna.", 2)]);
    for (answer, requests) in [("cancel", vec![]), ("action", vec![Request::DeleteSaved])] {
      let fake = Fake::default();
      let mut h = home.harness(&fake, Settings::default());
      tab(&mut h, Page::Advanced);
      assert!(!button(&h).contains("disabled") && caption(&h).is_none(), "{}", button(&h));
      click(&mut h, Id::of("delete-all"));
      let ask = h.app().ask.as_ref().map(|a| (a.title.as_str(), a.message.as_str()));
      let title = "Delete all 2 dictations and their recordings?";
      assert_eq!(ask, Some((title, "This can’t be undone.")));
      h.wait(Duration::from_millis(200));
      click(&mut h, Id::of(CONFIRM).key(answer));
      assert_eq!(fake.log().requests, requests, "{answer}");
    }

    // The session waits for what runs, then deletes: the page and the row hear of it.
    let waits = Status { warnings: vec![session::DELETE_WAITS.into()], ..Status::default() };
    h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::History);
    assert_eq!(h.app().history.entries.len(), 2);
    tab(&mut h, Page::Advanced);
    h.app_mut().update(&Settings::default(), &waits, 0);
    h.frame();
    assert_eq!(caption(&h).as_deref(), Some(DELETE_WAITS));
    assert_eq!(History::delete_saved(home.dir.path()).unwrap(), Some(2));
    h.app_mut().update(&Settings::default(), &Status::default(), 1);
    h.frame();
    assert!(button(&h).contains("disabled") && caption(&h).as_deref() == Some(NOTHING_SAVED));
    tab(&mut h, Page::History);
    assert!(h.app().history.entries.is_empty() && h.text(Id::of("history-total")).is_none());
  }

  /// Until Wavo quits asks nothing and deletes nothing: the saved history stays, which Delete All
  /// counts and deletes, not the one in memory. Export and Import are off then.
  #[test]
  fn until_quit_leaves_the_saved_history() {
    let home = Home::new("window-until-quit", &TWO);
    home.dictations(&[("Let's move the design review.", 3), ("Send the invoice to Anna.", 2)]);
    let memory = History::open_memory(home.dir.path()).unwrap();
    add_dictations(&memory, &[("Kept in memory.", 1)]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Advanced);
    click(&mut h, Id::of("retention"));
    click(&mut h, Id::of("retention").key(("item", 7)));
    assert_eq!(fake.log().settings.history_retention, Retention::UntilQuit);
    assert!(h.app().ask.is_none() && fake.log().requests.is_empty(), "nothing asked");
    assert_eq!(crate::testing::entry_ids(home.dir.path()).len(), 2, "nothing deleted");

    let status = Status { in_memory: true, ..Status::default() };
    let settings = Settings { history_retention: Retention::UntilQuit, ..Settings::default() };
    h.app_mut().update(&settings, &status, 0);
    h.frame();
    for row in ["export-row", "import-row"] {
      assert_eq!(h.text(Id::of(row).key("caption")), Some(NOT_SAVED), "{row}");
    }

    click(&mut h, Id::of("export"));
    assert!(h.file_dialog(None).is_none(), "export is off");
    click(&mut h, Id::of("delete-all"));
    let title = h.app().ask.as_ref().map(|a| a.title.clone());
    assert_eq!(title.as_deref(), Some("Delete all 2 dictations and their recordings?"));
  }

  /// In memory the page shows the session's dictations, without recordings; back on disk, the
  /// saved ones.
  #[test]
  fn the_history_in_memory_shows_its_dictations() {
    let home = Home::new("window-memory", &TWO);
    let saved = home.dictations(&[("Saved on disk.", 5)]);
    let memory = History::open_memory(home.dir.path()).unwrap();
    let ids = add_dictations(&memory, &[("Kept in memory.", 1), ("Only here.", 0)]);
    let fake = Fake::default();
    let settings = Settings { history_retention: Retention::UntilQuit, ..Settings::default() };
    let mut h = home.harness(&fake, settings.clone());
    tab(&mut h, Page::History);
    // Ids of their own: the saved one's is one of them.
    let texts = |h: &Harness<Prefs>| -> Vec<String> {
      h.app().history.entries.iter().map(|e| e.text.clone()).collect()
    };
    assert_eq!(texts(&h), ["Only here.", "Kept in memory."]);
    assert!(h.rect(row(ids[0])).is_some() && h.rect(row(ids[1])).is_some());
    assert_eq!(h.text(Id::of("history-total")), Some("2 dictations · 4 s · kept until Wavo quits"));
    let play = a11y_line(&h, "button \"Play").unwrap_or_default();
    assert!(play.contains("disabled") && play.contains(NO_RECORDING), "{play}");
    click(&mut h, row(ids[1]));
    let again = a11y_line(&h, "button \"Transcribe Again\"").unwrap_or_default();
    assert!(again.contains("disabled") && again.contains(NO_RECORDING), "{again}");

    let status = Status { in_memory: false, ..Status::default() };
    h.app_mut().update(&Settings::default(), &status, 0);
    h.waker().wake();
    h.frame();
    h.frame();
    assert_eq!(texts(&h), ["Saved on disk."]);
    assert!(h.rect(row(saved[0])).is_some());
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

    // The one in use can't be deleted; the other can.
    click(&mut h, Id::of(("delete", TWO[1])));
    assert!(h.rect(Id::of(CONFIRM).key("card")).is_none(), "the bin of the one in use is off");
    click(&mut h, Id::of(("delete", TWO[0])));
    confirm(&mut h);
    assert_eq!(fake.log().deleted, [TWO[0]]);

    // The search finds by name; the model in use stays shown.
    click(&mut h, Id::of("model-search"));
    h.type_text("moonshine");
    h.frame();
    h.frame();
    let shown = |h: &Harness<Prefs>| -> Vec<&str> {
      let shown = |m: &&ModelInfo| h.rect(Id::of(("card", m.name))).is_some();
      hub::MODELS.iter().filter(shown).map(|m| m.name).collect()
    };
    // Flat while searching: moonshine-tiny, not measured, too.
    assert_eq!(shown(&h), [TWO[1], "moonshine-tiny", "moonshine-base"]);
    assert!(h.rect(Id::of(("other-models", ""))).is_none());
  }

  /// The downloaded model `name`'s row.
  fn row_of(name: &str) -> Id {
    Id::of("downloaded").key(("row", model_key(hub::lookup(name).unwrap())))
  }

  /// Clicks the downloaded model `name`'s row by its name, with `mods`.
  fn click_model(h: &mut Harness<Prefs>, name: &str, mods: Modifiers) {
    let r = h.rect(row_of(name)).unwrap_or_else(|| panic!("no row for {name}"));
    h.press_with(r.x + 60.0, r.y + r.h / 2.0, 1, mods);
    h.release(r.x + 60.0, r.y + r.h / 2.0);
    h.frame();
    h.frame();
  }

  /// The downloaded models selected, by name.
  fn models_chosen(h: &Harness<Prefs>) -> Vec<&'static str> {
    let sel = &h.app().model_sel;
    hub::MODELS.iter().filter(|m| sel.contains(model_key(m))).map(|m| m.name).collect()
  }

  /// Downloaded models are selected as the History page's dictations: a click selects one,
  /// Cmd-click toggles, Shift-click extends, Escape clears; a click on a row's button is the
  /// button's.
  #[test]
  fn downloaded_models_are_selected_by_click() {
    let home = Home::new("window-model-sel", &["parakeet-v3", "parakeet-v2", "whisper-tiny"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    click(&mut h, Id::of(("use", "parakeet-v2")));
    assert!(h.app().model_sel.is_empty(), "the button's click, not the row's");
    assert_eq!(fake.log().settings.model.as_deref(), Some("parakeet-v2"));

    click_model(&mut h, "whisper-tiny", Modifiers::empty());
    assert_eq!(models_chosen(&h), ["whisper-tiny"], "a click selects");
    click_model(&mut h, "parakeet-v3", Modifiers::empty());
    assert_eq!(models_chosen(&h), ["parakeet-v3"], "another click selects that one instead");
    click_model(&mut h, "whisper-tiny", nimbo::CMD);
    assert_eq!(models_chosen(&h).len(), 2, "Cmd-click adds");
    click_model(&mut h, "parakeet-v3", nimbo::CMD);
    assert_eq!(models_chosen(&h), ["whisper-tiny"], "Cmd-click takes away");
    click(&mut h, Id::of(("delete", "parakeet-v3")));
    assert_eq!(models_chosen(&h), ["whisper-tiny"], "the bin's click, not the row's");
    h.wait(Duration::from_millis(200));
    click(&mut h, Id::of(CONFIRM).key("cancel"));
    assert!(h.app().ask.is_none() && fake.log().deleted.is_empty());

    // The rows are by fit, whatever their order: Shift-click from the first to the last takes all.
    let mut order = ["parakeet-v3", "parakeet-v2", "whisper-tiny"];
    order.sort_by(|a, b| {
      let y = |name: &str| h.rect(row_of(name)).unwrap().y;
      y(a).total_cmp(&y(b))
    });
    click_model(&mut h, order[0], Modifiers::empty());
    click_model(&mut h, order[2], Modifiers::SHIFT);
    assert_eq!(models_chosen(&h).len(), 3, "Shift-click extends");
    assert_eq!(a11y_line(&h, "button \"Delete…\""), None, "the header has its note only");

    h.key(NamedKey::Escape, Modifiers::empty());
    h.frame();
    h.frame();
    assert!(h.app().model_sel.is_empty(), "Escape clears");
  }

  /// Clicks the window at `x`, `y` and lets the frames after it settle.
  fn click_at(h: &mut Harness<Prefs>, x: f32, y: f32) {
    h.click(x, y);
    h.frame();
    h.frame();
  }

  /// As in Finder, a click on the page's ground clears the selection: beside the Downloaded
  /// header's title, in the page's margin, between its sections, beside the search; one on a
  /// control keeps it, and the Delete key asks about it.
  #[test]
  fn a_click_on_the_models_pages_ground_clears_the_selection() {
    let names = ["parakeet-v3", "parakeet-v2", "whisper-tiny"];
    let home = Home::new("window-model-ground", &names);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let active = h.app().active().unwrap().0.name;
    let m = *names.iter().find(|&&n| n != active).unwrap();
    click_model(&mut h, m, Modifiers::empty());
    let (list, row) = (h.rect(Id::of("downloaded")).unwrap(), h.rect(row_of(m)).unwrap());
    let search = h.rect(Id::of("model-search")).unwrap();
    // The header over the first row.
    let top = names.iter().map(|&n| h.rect(row_of(n)).unwrap().y).fold(f32::MAX, f32::min);
    let ground = [
      ("the header", list.x + list.w / 2.0, (list.y + top) / 2.0),
      ("the margin", list.x - 10.0, row.y + row.h / 2.0),
      ("between the sections", list.x + list.w / 2.0, list.y + list.h + 4.0),
      ("the search's row", search.x - 10.0, search.y + search.h / 2.0),
    ];
    for (what, x, y) in ground {
      click_model(&mut h, m, Modifiers::empty());
      assert_eq!(models_chosen(&h), [m]);
      click_at(&mut h, x, y);
      assert!(h.app().model_sel.is_empty(), "a click on {what} clears");
    }

    click_model(&mut h, m, Modifiers::empty());
    click(&mut h, Id::of("model-search"));
    let other = *names.iter().find(|&&n| n != active && n != m).unwrap();
    click(&mut h, Id::of(("use", other)));
    assert_eq!(models_chosen(&h), [m], "the controls' clicks keep it");
    h.set_focus(Some(Id::of("downloaded")));
    h.key(NamedKey::Delete, Modifiers::empty());
    h.frame();
    h.frame();
    let ask = h.app().ask.as_ref().map(|a| a.title.clone());
    assert_eq!(ask, Some(format!("Delete {}?", hub::lookup(m).unwrap().name)));
  }

  /// A dictation's row is as high as a model's: both a line over a caption, padded alike.
  #[test]
  fn history_and_models_rows_are_one_height() {
    let home = Home::new("window-row-h", &TWO);
    let ids = home.dictations(&[("Call Anna.", 1)]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::History);
    let entry = h.rect(row(ids[0])).unwrap().h;
    tab(&mut h, Page::Models);
    assert_eq!(entry, h.rect(row_of(TWO[0])).unwrap().h);
  }

  /// Downloaded models selected go at once by Delete or the menu after the dialog, but the one in
  /// use, which stays and says so.
  #[test]
  fn downloaded_models_are_deleted_together() {
    let home = Home::new("window-bulk", &["parakeet-v3", "parakeet-v2", "whisper-tiny"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let list = Id::of("downloaded");
    click(&mut h, Id::of(("use", "parakeet-v2")));
    h.set_focus(Some(list));
    h.key('a', nimbo::CMD);
    h.frame();
    assert_eq!(h.app().model_sel.len(), 3);
    // The menu says how many go.
    h.native_menus();
    let r = h.rect(row_of("whisper-tiny")).unwrap();
    h.right_click(r.x + 60.0, r.y + r.h / 2.0);
    h.frame();
    let (_, menu) = h.context_menu(None).expect("the row's menu");
    assert_eq!((menu.items[0].label(), menu.items[0].is_enabled()), ("Delete 2 Models…", true));
    h.frame();
    assert_eq!(h.app().model_sel.len(), 3, "a right-click inside the selection keeps it");

    h.set_focus(Some(list));
    h.key(NamedKey::Delete, Modifiers::empty());
    h.frame();
    h.frame();
    let ask = h.app().ask.as_ref().expect("the dialog");
    assert_eq!(ask.title, "Delete 2 models?");
    assert!(ask.message.ends_with("parakeet-v2 is in use and stays."), "{}", ask.message);
    confirm(&mut h);
    let mut deleted = fake.log().deleted.clone();
    deleted.sort();
    assert_eq!(deleted, ["parakeet-v3", "whisper-tiny"]);
  }

  /// A switch from the menu bar to a model the dialog asks about keeps it; so does the one a
  /// dictation goes on with after a switch, its bin off.
  #[test]
  fn a_model_in_use_by_the_confirm_stays() {
    let home = Home::new("window-bulk-switch", &["parakeet-v3", "parakeet-v2", "whisper-tiny"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let ask_all = |h: &mut Harness<Prefs>| {
      h.set_focus(Some(Id::of("downloaded")));
      h.key('a', nimbo::CMD);
      h.frame();
      h.key(NamedKey::Delete, Modifiers::empty());
      h.frame();
      h.frame();
      h.app().ask.as_ref().map(|a| a.message.clone()).expect("the dialog")
    };
    let switch = |h: &mut Harness<Prefs>, name: &str, status: Status| {
      let settings = Settings { model: Some(name.into()), ..h.app().settings.clone() };
      h.app_mut().update(&settings, &status, 0);
      h.waker().wake();
      h.frame();
    };
    assert!(ask_all(&mut h).ends_with("parakeet-v3 is in use and stays."));
    switch(&mut h, "parakeet-v2", Status::default());
    confirm(&mut h);
    assert_eq!(fake.log().deleted, ["whisper-tiny"]);

    // Back to parakeet-v3 while a dictation transcribes with parakeet-v2.
    fake.log().deleted.clear();
    let file = hub::lookup("parakeet-v2").unwrap().file();
    let model = file.strip_suffix(".gguf").map(str::to_owned);
    let engine = EngineStatus { model, phase: engine::Phase::Running, ..EngineStatus::default() };
    let working = Status { state: session::Phase::Transcribing, engine, ..Status::default() };
    switch(&mut h, "parakeet-v3", working);
    click(&mut h, Id::of(("delete", "parakeet-v2")));
    assert!(h.app().ask.is_none(), "the bin of the one a dictation has is off");
    let message = ask_all(&mut h);
    assert!(message.ends_with(" are in use and stay."), "{message}");
    confirm(&mut h);
    assert_eq!(fake.log().deleted, ["whisper-tiny"]);
  }

  /// The model chosen, gone from disk, shows to download it again, without punctuation too and
  /// whatever the search; the other ones without punctuation don't.
  #[test]
  fn a_chosen_model_gone_shows_to_download_again() {
    let home = Home::new("window-gone", &["parakeet-v3"]);
    let fake = Fake::default();
    let settings = Settings { model: Some("gigaam-v3-ctc".into()), ..Settings::default() };
    let mut h = home.harness(&fake, settings);
    tab(&mut h, Page::Models);
    let search = |h: &mut Harness<Prefs>, s: &str| {
      h.app_mut().model_search = s.to_owned();
      h.frame();
    };
    let shown = |h: &Harness<Prefs>, name: &str| h.rect(Id::of(("card", name))).is_some();
    search(&mut h, "gigaam");
    assert!(h.rect(Id::of(("download", "gigaam-v3-ctc"))).is_some(), "to download again");
    assert!(shown(&h, "gigaam-v3") && !shown(&h, "gigaam-v3-rnnt"), "without punctuation");
    search(&mut h, "whisper");
    assert!(shown(&h, "gigaam-v3-ctc") && shown(&h, "parakeet-v3"), "whatever the search");
  }

  /// A WER's hover says what it counts: characters in a language written without spaces.
  #[test]
  fn the_errors_count_characters_where_words_have_no_spaces() {
    let per = |lang: &str| errors_per(lang).trim_start_matches("Errors per 100 ");
    assert_eq!(["zh", "yue", "ja", "th"].map(per), ["characters"; 4]);
    assert_eq!(["en", "ru", "ko", ""].map(per), ["words"; 4]);

    let home = Home::new("window-cer", &["parakeet-v3"]);
    let mut h = home.harness(&Fake::default(), Settings::default());
    tab(&mut h, Page::Models);
    h.app_mut().model_lang = LANGUAGES.iter().position(|l| l.1 == "zh").unwrap();
    h.frame();
    let read = r#"static text "WER 8.4%" desc "Errors per 100 characters""#;
    assert!(h.a11y().lines().any(|l| l.trim() == read), "{}", h.a11y());
  }

  /// A model that runs on the Neural Engine says ANE in its caption, downloaded or not, on a Mac
  /// that has one; Whisper's don't.
  #[test]
  fn models_for_the_neural_engine_say_ane() {
    let home = Home::new("window-ane", &["parakeet-v3", "whisper-tiny"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    click(&mut h, Id::of(("use", "whisper-tiny")));
    // What assistive technology reads after the name, which it reads.
    let after = |h: &Harness<Prefs>, name: &str| {
      let tree = h.a11y();
      let mut lines = tree.lines().map(str::trim);
      let named = lines.any(|l| l == format!("static text {name:?}"));
      assert!(named, "no {name} in\n{tree}");
      lines.take(3).collect::<Vec<_>>().join(" | ")
    };
    let ane = r#"static text "ANE" desc "Runs on the Apple Neural Engine""#;
    let caption = |lang: &str, size: &str, ane: &str| {
      format!(r#"static text "{lang}" | static text "{size}" | {ane}"#)
    };
    let v3 = caption("25 European languages", "· 740 MB ·", ane);
    assert_eq!(after(&h, "parakeet-v3"), v3, "downloaded");
    assert_eq!(after(&h, "gigaam-v3"), caption("Russian", "· 274 MB ·", ane), "available");
    // parakeet-v2 below the page's fold: found by the search.
    let search = |h: &mut Harness<Prefs>, s: &str| {
      h.app_mut().model_search = s.to_owned();
      h.frame();
    };
    search(&mut h, "parakeet-v2");
    assert_eq!(after(&h, "parakeet-v2"), caption("English", "· 730 MB ·", ane), "no badge");

    search(&mut h, "");
    click(&mut h, Id::of(("use", "parakeet-v3")));
    let whisper = r#"static text "99 languages" | static text "· 46 MB""#;
    assert!(after(&h, "whisper-tiny").starts_with(whisper), "{}", after(&h, "whisper-tiny"));

    fake.log().no_neural_engine = true;
    search(&mut h, "parakeet-v2");
    let v2 = after(&h, "parakeet-v2");
    assert!(v2.starts_with(r#"static text "English" | static text "· 730 MB""#), "{v2}");
    assert!(!v2.contains("ANE"), "no Neural Engine: {v2}");
  }

  /// Not measured, in Other models, folded; but one downloading, and all of them for a language
  /// with nothing measured.
  #[test]
  fn other_models_fold_but_not_a_download_or_a_language_unmeasured() {
    let home = Home::new("window-other", &[]);
    let fake = Fake::default();
    fake.log().states.insert("whisper-large-v2", Download::Preparing);
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let at = |code: &str| LANGUAGES.iter().position(|l| l.1 == code).unwrap();
    h.app_mut().model_lang = at("ru");
    h.frame();
    assert!(h.rect(Id::of(("other-models", "ru"))).is_some());
    assert!(h.rect(Id::of(("card", "whisper-large-v2"))).is_some(), "the one preparing shown");
    assert!(h.rect(Id::of(("card", "whisper-large"))).is_none(), "folded");

    h.app_mut().model_lang = at("sq");
    h.frame();
    assert!(h.rect(Id::of(("other-models", "sq"))).is_none());
    assert!(h.rect(Id::of(("card", "whisper-large"))).is_some(), "listed");
  }

  /// Models with too many errors in the language folded under their own row, the others listed,
  /// the recommended one usable; with none usable, no badge but a note; Russian and English as
  /// they were.
  #[test]
  fn models_with_too_many_errors_fold() {
    let home = Home::new("window-poor", &["whisper-tiny"]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::Models);
    let lang = |h: &mut Harness<Prefs>, code: &str| {
      h.app_mut().model_lang = LANGUAGES.iter().position(|l| l.1 == code).unwrap();
      h.frame();
    };
    let card = |h: &Harness<Prefs>, name: &str| h.rect(Id::of(("card", name))).is_some();
    let note = |h: &Harness<Prefs>| h.text(Id::of("no-usable-model")).map(str::to_owned);
    let badge = |h: &Harness<Prefs>| h.a11y().contains(r#""Recommended""#);

    // Georgian: every model measured has too many errors.
    lang(&mut h, "ka");
    assert_eq!(note(&h).as_deref(), Some("No model recognizes Georgian well yet."));
    assert!(!badge(&h) && best_for("ka").is_none(), "nothing recommended");
    assert!(h.rect(Id::of(("poor-models", "ka"))).is_some());
    assert!(!card(&h, "whisper-turbo") && !card(&h, "whisper-base"), "folded");
    assert!(card(&h, "whisper-tiny"), "downloaded, shown");
    assert!(!card(&h, "whisper-large-v3"), "not measured, folded too");
    assert!(h.rect(Id::of(("other-models", "ka"))).is_some());
    click(&mut h, Id::of(("poor-models", "ka")));
    assert!(card(&h, "whisper-turbo") && card(&h, "whisper-base"), "unfolded");

    // Greek: parakeet's two have too many errors, whisper-turbo is usable and recommended.
    lang(&mut h, "el");
    let pick = recommended("el").unwrap();
    assert!(pick.name == "whisper-turbo" && pick.wer("el").unwrap() <= USABLE);
    assert!(note(&h).is_none() && badge(&h));
    assert!(card(&h, "whisper-turbo") && !card(&h, "parakeet-v3") && !card(&h, "parakeet-ultra"));
    assert!(h.rect(Id::of(("poor-models", "el"))).is_some());
    assert!(unusable("el", hub::lookup("parakeet-v3").unwrap()), "measured above the cutoff");

    // Found by the search, unfolded.
    h.app_mut().model_search = "parakeet".to_owned();
    h.frame();
    assert!(card(&h, "parakeet-v3") && h.rect(Id::of(("poor-models", "el"))).is_none());
    h.app_mut().model_search.clear();

    // Russian and English: the same pick, no note.
    for (code, name) in [("ru", "gigaam-v3-e2e-ctc"), ("en", "parakeet-v2")] {
      lang(&mut h, code);
      assert_eq!(best_for(code).map(|p| p.0), Some(name), "{code}");
      assert!(note(&h).is_none() && badge(&h), "{code}");
      assert!(card(&h, name), "{code}");
    }

    // Albanian, not measured: whisper-turbo as before, no note.
    lang(&mut h, "sq");
    assert!(note(&h).is_none() && h.rect(Id::of(("poor-models", "sq"))).is_none());
    assert_eq!(best_for("sq").map(|p| p.0), Some("whisper-turbo"));
  }

  /// The recommended model first, the measured ones by their errors, then the rest, those made
  /// for the language alone first.
  #[test]
  fn a_language_lists_its_pick_then_its_own_models_first() {
    let order = |lang: &str| {
      let best = recommended(lang).map(|m| m.name);
      let mut models: Vec<&ModelInfo> =
        hub::MODELS.iter().filter(|m| m.langs.contains(&lang)).collect();
      models.sort_by_key(|m| rank(lang, best, m));
      models.into_iter().map(|m| m.name).collect::<Vec<_>>()
    };
    assert_eq!(order("he")[..3], ["parakeet-he", "whisper-turbo-he", "whisper-turbo"]);
    let en = ["parakeet-v2", RECOMMENDED, "whisper-large-v3", "breeze-asr-25", "whisper-turbo"];
    assert_eq!(order("en")[..6], [&en[..], &["whisper-large-v2"]].concat());
    let ru = ["gigaam-v3-e2e-ctc", "whisper-large-v3", "gigaam-v3", "whisper-turbo", RECOMMENDED];
    assert_eq!(order("ru")[..5], ru);

    // On a tie, the one made for the language alone first, wherever it is in wavo's table.
    let all = *hub::lookup("whisper-turbo").unwrap();
    let mut he = all;
    he.langs = &["he"];
    let mut tie = [&all, &he];
    tie.sort_by_key(|m| rank("he", None, m));
    assert_eq!(tie.map(|m| m.langs.len() == 1), [true, false]);
  }

  /// The fast model with the fewest errors, the fastest of those near it; a slow one only when
  /// far better, as whisper-turbo in Greek; never one with too many errors, none in Bengali.
  #[test]
  fn the_recommended_model_is_fast_and_accurate() {
    let cases = [
      ("ru", "gigaam-v3-e2e-ctc"),
      ("en", "parakeet-v2"),
      ("sv", "parakeet-ultra"),
      ("el", "whisper-turbo"),
      ("he", "parakeet-he"),
    ];
    for (lang, name) in cases {
      assert_eq!(recommended(lang).map(|m| m.name), Some(name), "{lang}");
    }

    assert_eq!(recommended(""), None);

    // With none measured, whisper-turbo where it takes the language.
    let pick = |lang: &str| best_for(lang).map(|p| p.0);
    assert_eq!(
      (pick(""), pick("ru"), pick("sq"), pick("xx")),
      (Some(RECOMMENDED), Some("gigaam-v3-e2e-ctc"), Some("whisper-turbo"), None)
    );
    let sq = best_for("sq").unwrap().1;
    assert_eq!(sq, "No accuracy data for Albanian yet: the best general model");
    assert_eq!(recommended("sq"), None, "none measured");
    // None in Georgian or Bengali, where every model measured has too many errors.
    for lang in ["ka", "bn"] {
      assert_eq!(pick(lang), None, "{lang}: none usable");
      assert!(none_usable(lang), "{lang}");
      assert!(hub::lookup("whisper-turbo").unwrap().wer(lang).is_some_and(|x| x > USABLE));
    }

    assert!(!none_usable("sq") && !none_usable("ru") && !none_usable(""));
    // Never one above the cutoff, in any language.
    for (_, lang) in LANGUAGES.iter() {
      assert!(recommended(lang).is_none_or(|m| m.wer(lang).unwrap() <= USABLE), "{lang}");
    }
  }

  /// WER to a decimal under 10 (rounded first); bolts and the speed's words, whisper-turbo the
  /// baseline; never an unpunctuated model recommended.
  #[test]
  fn models_say_their_errors_and_speed() {
    let m = |name: &str| hub::lookup(name).unwrap();
    assert_eq!(wer_text("ru", m("gigaam-v3")).as_deref(), Some("WER 4.5%"));
    assert_eq!(wer_text("el", m("parakeet-ultra")).as_deref(), Some("WER 32%"));
    assert_eq!(wer_text("", m("parakeet-ultra")), None, "all languages");
    assert_eq!([9.94, 9.96, 31.6].map(figure), ["9.9", "10", "32"]);
    for (_, lang) in LANGUAGES.iter() {
      assert!(recommended(lang).is_none_or(|m| m.punctuated), "{lang}");
    }

    assert_eq!([22.1, 9.96, 8.5, 0.9].map(times), ["22×", "10×", "8.5×", "0.9×"]);
    let speed = |name| speed_text(m(name)).unwrap();
    assert_eq!(speed("gigaam-v3"), "13× faster than whisper-turbo");
    assert_eq!(speed("parakeet-ultra"), "6.7× faster than whisper-turbo");
    assert_eq!(speed("whisper-turbo"), "The speed baseline (1.0×)");
    assert_eq!(speed("whisper-turbo-he"), "As fast as whisper-turbo");
    assert_eq!(speed("whisper-medium"), "0.9× the speed of whisper-turbo");
    let mut unmeasured = *m("whisper-tiny");
    unmeasured.name = "unmeasured";
    assert_eq!(speed_text(&unmeasured), None);
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
    assert_eq!(h.clipboard().read().as_deref(), Some("Send the invoice to Anna."));
    // Said under the panel for a while, the search keeping its width; then the totals again.
    assert_eq!(h.text(Id::of("history-total")), Some("Copied to the clipboard"));
    let search = h.rect(Id::of("history-search")).unwrap();
    h.wait(NOTE_TIME);
    assert_eq!(h.rect(Id::of("history-search")), Some(search));

    // Nothing selected: the panel under the list has no actions; a click on a row selects it.
    assert!(h.rect(Id::of(("retranscribe", ids[0]))).is_none());
    click(&mut h, row(ids[0]));
    assert_eq!(selected(&h), [ids[0]]);
    let total = h.text(Id::of("history-total")).unwrap();
    assert!(total.starts_with("2 dictations · ") && total.contains(" · since "), "{total}");

    // Transcribed again from the panel: its new text came, and the one before comes back.
    click(&mut h, Id::of(("retranscribe", ids[0])));
    let history = History::open(home.dir.path()).unwrap();
    let again = "Move the design review.";
    history.db.update_text(ids[0], again, Some("whisper-tiny-Q8_0"), None).unwrap();
    let settings = h.app().settings.clone();
    let status = Status { restorable: vec![ids[0]], ..Status::default() };
    h.app_mut().update(&settings, &status, 2);
    h.waker().wake();
    h.frame();
    click(&mut h, Id::of(("restore", ids[0])));
    click(&mut h, Id::of(("delete", ids[0])));
    let want = "“Move the design review.” and its recording are removed from the history.";
    assert_eq!(h.app().ask.as_ref().map(|a| a.message.as_str()), Some(want));
    confirm(&mut h);
    let want =
      [Request::Retranscribe(ids[0]), Request::Restore(ids[0]), Request::Delete(vec![ids[0]])];
    assert_eq!(fake.log().requests, want);

    // The search finds by text, in any case, once it is typed: meanwhile the last result stays.
    click(&mut h, Id::of("history-search"));
    h.type_text("INVOICE");
    h.frame();
    assert!(h.rect(Id::of(("copy", ids[0]))).is_some(), "the last result while it is typed");
    assert_eq!(selected(&h), [ids[0]]);
    h.wait(SEARCH_WAIT);
    h.frame();
    assert!(
      h.rect(Id::of(("copy", ids[1]))).is_some() && h.rect(Id::of(("copy", ids[0]))).is_none()
    );
    assert!(selected(&h).is_empty(), "out of the search");
  }

  /// Play goes to the shell, which calls [Prefs::played] when it ends; a recording that could not
  /// play says why under the panel.
  #[test]
  fn play_asks_the_shell() {
    let home = Home::new("window-play", &TWO);
    let ids = home.dictations(&[("Send the invoice to Anna.", 2)]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    tab(&mut h, Page::History);
    click(&mut h, Id::of(("play", ids[0])));
    let file = fake.log().played.clone();
    assert!(file.len() == 1 && file[0].is_file(), "{file:?}");
    assert_eq!(h.text(Id::of("history-total")), Some("Could not play the recording: no speakers"));
    assert!(h.app().history.playing.is_none());
  }

  /// The menu of row `id` by a right-click, answered `answer` (an item's name below): its items'
  /// names, each on or off, a divider `("", true)`.
  fn menu(h: &mut Harness<Prefs>, id: i64, answer: Option<&str>) -> Vec<(String, bool)> {
    let reveal = format!("Show in {}", open::file_manager());
    let names = [
      ("copy", "Copy Text"),
      ("play", "Play the Recording"),
      ("stop", "Stop"),
      ("copy-path", "Copy Recording Path"),
      ("reveal", reveal.as_str()),
      ("retranscribe", "Transcribe Again"),
      ("restore", "Restore the Previous Text"),
      ("delete", "Delete…"),
      ("retranscribe-chosen", "Transcribe Again"),
    ];
    let label = |name: &str| names.iter().find(|n| n.0 == name).map(|n| n.1);
    let r = h.rect(row(id)).unwrap();
    h.right_click(r.x + 20.0, r.y + r.h / 2.0);
    let (_, menu) = h.context_menu(answer.and_then(label)).expect("the row's menu");
    h.frame();
    // A row among several has its own Transcribe Again.
    let several = menu.items.len() == 3;
    let name = |label: &str| {
      let found = names.iter().find(|n| n.1 == label && !(several && n.0 == "retranscribe"));
      found.map_or("", |n| n.0).to_owned()
    };
    let item = |i: &nimbo::Item<usize>| match i {
      nimbo::Item::Divider => (String::new(), true),
      i => (name(i.label()), i.is_enabled()),
    };
    menu.items.iter().map(item).collect()
  }

  /// A right-click on a row selects it and opens its menu, which does what the row's buttons and
  /// the panel under the list do; on the Mac a Control-click opens it too, on Windows it opens as
  /// the button is let go.
  #[test]
  fn a_rows_menu_copies_transcribes_again_and_deletes() {
    let home = Home::new("window-row-menu", &TWO);
    let ids =
      home.dictations(&[("Let's move the design review.", 3), ("Send the invoice to Anna.", 2)]);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    h.native_menus();
    tab(&mut h, Page::History);
    let items = |want: &[(&str, bool)]| -> Vec<(String, bool)> {
      want.iter().map(|&(id, on)| (id.into(), on)).collect()
    };
    let update = |h: &mut Harness<Prefs>, status: Status, n| {
      let settings = h.app().settings.clone();
      h.app_mut().update(&settings, &status, n);
      h.waker().wake();
      h.frame();
    };

    // Selected by the right-click; one on the selected row keeps it selected, as in Finder.
    let all = items(&[
      ("copy", true),
      ("play", true),
      ("copy-path", true),
      ("reveal", true),
      ("", true),
      ("retranscribe", true),
      ("", true),
      ("delete", true),
    ]);
    assert_eq!(menu(&mut h, ids[1], Some("copy")), all);
    assert_eq!(selected(&h), [ids[1]], "selected by the right-click");
    assert_eq!(h.clipboard().read().as_deref(), Some("Send the invoice to Anna."));
    menu(&mut h, ids[1], None);
    assert_eq!(selected(&h), [ids[1]], "kept selected by another one");

    // While it is transcribed again only what reads it is on; then its text before comes back.
    menu(&mut h, ids[0], Some("retranscribe"));
    assert_eq!(selected(&h), [ids[0]], "the other row selected");
    let restorable = vec![ids[0]];
    update(&mut h, Status { retranscribing: vec![ids[0]], restorable, ..Status::default() }, 2);
    let busy = items(&[
      ("copy", true),
      ("play", true),
      ("copy-path", true),
      ("reveal", true),
      ("", true),
      ("retranscribe", false),
      ("restore", false),
      ("", true),
      ("delete", false),
    ]);
    assert_eq!(menu(&mut h, ids[0], None), busy);
    update(&mut h, Status { restorable: vec![ids[0]], ..Status::default() }, 3);
    let got = menu(&mut h, ids[0], Some("restore"));
    assert_eq!(got[5..7], items(&[("retranscribe", true), ("restore", true)]));

    menu(&mut h, ids[0], Some("delete"));
    let want = "“Let's move the design review.” and its recording are removed from the history.";
    assert_eq!(h.app().ask.as_ref().map(|a| a.message.as_str()), Some(want));
    confirm(&mut h);
    let want =
      [Request::Retranscribe(ids[0]), Request::Restore(ids[0]), Request::Delete(vec![ids[0]])];
    assert_eq!(fake.log().requests, want);

    // A Control-click on the Mac asks for it as the button goes down; Windows as it is let go.
    let control = (Os::Mac, MouseButton::Left, Modifiers::CONTROL, ids[1]);
    let windows = (Os::Windows, MouseButton::Right, Modifiers::empty(), ids[0]);
    for (os, button, mods, id) in [control, windows] {
      h.set_os(os);
      let r = h.rect(row(id)).unwrap();
      let pos = Point::new(r.x + 20.0, r.y + r.h / 2.0);
      h.event(Event::MouseDown { pos, button, clicks: 1, mods });
      h.frame();
      let down = h.context_menu(None).is_some();
      h.event(Event::MouseUp { pos, button, mods });
      let up = h.context_menu(None).is_some();
      h.frame();
      assert_eq!((down, up), (os == Os::Mac, os == Os::Windows), "{os:?}: the menu, once");
      assert_eq!(selected(&h), [id], "{os:?}: selected");
    }
  }

  /// History with four dictations, the three oldest selected; their ids, oldest first.
  fn several(home: &Home, fake: &Fake) -> (Harness<Prefs>, Vec<i64>) {
    let texts = [("Let's move the review.", 4), ("Send the invoice.", 3), ("Call Anna.", 2)];
    let ids = home.dictations(&[texts[0], texts[1], texts[2], ("Book a table.", 1)]);
    let mut h = home.harness(fake, Settings::default());
    h.native_menus();
    tab(&mut h, Page::History);
    let sel = &mut h.app_mut().history.selected;
    for (i, &id) in ids[..3].iter().rev().enumerate() {
      sel.toggle(id as u64, i + 1);
    }

    h.frame();
    (h, ids)
  }

  fn update(h: &mut Harness<Prefs>, status: Status) {
    let settings = h.app().settings.clone();
    h.app_mut().update(&settings, &status, 0);
    h.waker().wake();
    h.frame();
  }

  /// Several selected, the panel says how many, with no single one's actions.
  #[test]
  fn the_panel_shows_how_many_are_selected() {
    let (home, fake) = (Home::new("window-several-panel", &TWO), Fake::default());
    let (h, ids) = several(&home, &fake);
    assert_eq!(h.text(Id::of("history-chosen")), Some("3 dictations selected"));
    assert!(h.rect(Id::of(("retranscribe", ids[0]))).is_none());
  }

  /// The menu of a row among several selected has what is done to all of them, keeps them
  /// selected and transcribes each one again.
  #[test]
  fn the_menu_of_several_transcribes_them_again() {
    let (home, fake) = (Home::new("window-several-menu", &TWO), Fake::default());
    let (mut h, ids) = several(&home, &fake);
    let items =
      vec![("retranscribe-chosen".into(), true), ("".into(), true), ("delete".into(), true)];
    assert_eq!(menu(&mut h, ids[1], Some("retranscribe-chosen")), items);
    assert_eq!(selected(&h), ids[..3]);
    let want = ids[..3].iter().rev().map(|&id| Request::Retranscribe(id)).collect::<Vec<_>>();
    assert_eq!(fake.log().requests, want);
  }

  /// Delete, by the list's key or the panel's Delete…, asks with the count, then deletes them all.
  #[test]
  fn delete_asks_with_the_count() {
    let (home, fake) = (Home::new("window-several-delete", &TWO), Fake::default());
    let (mut h, ids) = several(&home, &fake);
    h.set_focus(Some(Id::of("history-list")));
    h.key(NamedKey::Delete, Modifiers::empty());
    let ask = h.app().ask.as_ref().map(|a| (a.title.as_str(), a.message.as_str()));
    let want = "3 dictations and their recordings are removed from the history.";
    assert_eq!(ask, Some(("Delete 3 dictations?", want)));
    h.app_mut().ask = None;
    click(&mut h, Id::of("delete-chosen"));
    confirm(&mut h);
    assert_eq!(fake.log().requests, [Request::Delete(vec![ids[2], ids[1], ids[0]])]);
  }

  /// As on Models, a click on the ground clears the selection: the list's margin, the room under
  /// its rows, beside the search; one on the panel under the list, which shows it, keeps it.
  #[test]
  fn a_click_on_the_history_pages_ground_clears_the_selection() {
    let (home, fake) = (Home::new("window-history-ground", &TWO), Fake::default());
    let (mut h, ids) = several(&home, &fake);
    click(&mut h, Id::of("history-chosen"));
    assert_eq!(selected(&h), ids[..3], "the panel's click keeps it");
    let (list, row) = (h.rect(Id::of("history-list")).unwrap(), h.rect(row(ids[0])).unwrap());
    let search = h.rect(Id::of("history-search")).unwrap();
    let ground = [
      ("the margin", list.x + 10.0, row.y + row.h / 2.0),
      ("under the rows", list.x + list.w / 2.0, list.y + list.h - 10.0),
      ("the search's row", search.x - 10.0, search.y + search.h / 2.0),
    ];
    for (what, x, y) in ground {
      h.app_mut().history.selected.select(ids[0] as u64, 3);
      h.frame();
      click_at(&mut h, x, y);
      assert!(selected(&h).is_empty(), "a click on {what} clears");
    }
  }

  /// One of the selection being transcribed again, none of it is deleted: the menu's and the
  /// panel's Delete are off, the key asks nothing.
  #[test]
  fn a_busy_one_keeps_the_selection_from_delete() {
    let (home, fake) = (Home::new("window-several-busy", &TWO), Fake::default());
    let (mut h, ids) = several(&home, &fake);
    update(&mut h, Status { retranscribing: vec![ids[1]], ..Status::default() });
    let delete = |items: Vec<(String, bool)>| items.into_iter().find(|i| i.0 == "delete").unwrap();
    assert_eq!(delete(menu(&mut h, ids[0], None)), ("delete".into(), false));
    h.set_focus(Some(Id::of("history-list")));
    h.key(NamedKey::Delete, Modifiers::empty());
    assert!(h.app().ask.is_none());
    click(&mut h, Id::of("delete-chosen"));
    assert!(h.app().ask.is_none());
  }

  /// With its recording on disk a row's menu copies the recording's absolute path and shows it in
  /// the file manager; without, it has neither.
  #[test]
  fn a_rows_menu_copies_and_shows_its_recording() {
    let home = Home::new("window-row-recording", &TWO);
    let ids = home.dictations(&[("Send the invoice to Anna.", 2), ("Let's move the review.", 1)]);
    let history = History::open(home.dir.path()).unwrap();
    let file = |id| history.audio_file(&history.db.get_entry(id).unwrap().unwrap()).unwrap();
    let (file, other) = (file(ids[0]), file(ids[1]));
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    h.native_menus();
    tab(&mut h, Page::History);

    let got = menu(&mut h, ids[0], Some("copy-path"));
    assert_eq!(got[2..4], [("copy-path".into(), true), ("reveal".into(), true)]);
    assert!(file.is_absolute());
    assert_eq!(h.clipboard().read(), Some(file.to_string_lossy().into_owned()));
    assert_eq!(h.text(Id::of("history-total")), Some("Copied to the clipboard"));
    menu(&mut h, ids[0], Some("reveal"));
    assert_eq!(fake.log().revealed, [file]);

    // A recording gone from disk: Play stays (it says it is gone), the file's items don't.
    std::fs::remove_file(&other).unwrap();
    let ids_of = |items: Vec<(String, bool)>| items.into_iter().map(|i| i.0).collect::<Vec<_>>();
    let got = ids_of(menu(&mut h, ids[1], None));
    assert!(got.contains(&"play".into()), "{got:?}");
    assert!(!got.iter().any(|id| id == "copy-path" || id == "reveal"), "{got:?}");
  }

  /// The menu's answer acts as the item it chose, whatever the row is by then: its Stop doesn't
  /// play again a recording that ended meanwhile.
  #[test]
  fn a_rows_menu_stops_as_it_says() {
    let e = Entry { id: 7, audio_path: Some("a.wav".into()), ..Entry::default() };
    let st = RowState { playing: true, ..RowState::default() };
    assert_eq!(row_menu(&e, &st, true).items[1].action(), Some(&RowItem::Stop));
    assert!(matches!(row_act(RowItem::Stop, &e), Act::Stop));
    assert!(matches!(row_act(RowItem::Play, &e), Act::Play(7)));
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
    assert!(h.rect(row(ids[299])).is_some(), "the newest, at the top");
    assert!(h.rect(row(ids[0])).is_none(), "the oldest, far below, not built");
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
      needs_admin: false,
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

  /// Cleaning the text is on until the person turns it off, and the switch shows what is stored.
  #[test]
  fn the_clean_text_switch_shows_what_is_stored() {
    let home = Home::new("window-clean", &TWO);
    let fake = Fake::default();
    let state = |h: &Harness<Prefs>| a11y_line(h, r#"switch "Clean up the text""#).unwrap();
    let mut h = home.harness(&fake, Settings::default());
    assert!(state(&h).ends_with("on"), "{}", state(&h));
    click(&mut h, Id::of("clean"));
    assert!(!fake.log().settings.clean_text);
    assert!(state(&h).ends_with("off"), "{}", state(&h));
    click(&mut h, Id::of("clean"));
    assert!(fake.log().settings.clean_text);

    let h = home.harness(&fake, Settings { clean_text: false, ..Settings::default() });
    assert!(state(&h).ends_with("off"), "{}", state(&h));
  }

  /// The Output switches show what is stored and take a click; where only an administrator can
  /// allow Accessibility, the paste one is disabled under one warning line, first on the page.
  #[test]
  fn the_output_switches_show_what_is_stored() {
    let home = Home::new("window-output", &TWO);
    let fake = Fake::default();
    let switches = |h: &Harness<Prefs>| {
      let state = |name: &str| a11y_line(h, &format!(r#"switch "{name}""#)).unwrap();
      [state("Paste the text into the app you dictate in"), state("Keep the text on the clipboard")]
        .map(|l| l.rsplit_once('"').unwrap().1.trim().to_owned())
    };
    let mut h = home.harness(&fake, Settings::default());
    for (id, paste, keep) in [("keep", "on", "on"), ("paste", "off", "on"), ("keep", "off", "off")]
    {
      click(&mut h, Id::of(id));
      let s = &fake.log().settings;
      assert_eq!((s.auto_paste, s.keep_in_clipboard), (paste == "on", keep == "on"), "{id}");
      assert_eq!(switches(&h), [paste, keep], "{id}");
    }

    fake.log().access = Some(Access { accessibility: false, needs_admin: true, ..fake.access() });
    let mut h = home.harness(&fake, Settings::without_paste());
    let lines: Vec<_> = h.a11y().lines().map(|l| l.trim().to_owned()).collect();
    let at = lines.iter().position(|l| *l == format!(r#"static text "{NO_ADMIN}""#)).unwrap();
    assert_eq!(lines[at - 1], r#"tab "About""#, "first on the page, no title: {lines:?}");
    assert!(h.rect(Id::of("open-accessibility")).is_none(), "no button");
    assert_eq!(switches(&h), ["off disabled", "on"]);
    click(&mut h, Id::of("paste"));
    click(&mut h, Id::of("keep"));
    assert_eq!(switches(&h), ["off disabled", "off"]);
    let h = home.harness(&fake, Settings::default());
    assert_eq!(switches(&h), ["on disabled", "off"]);
  }

  /// A single key where only an administrator can allow Accessibility: the Shortcut row's ⓘ is
  /// the warning, saying to use a combination. A combination, or an administrator, keeps the ⓘ.
  #[test]
  fn a_single_key_warns_where_only_an_administrator_can_allow_it() {
    let home = Home::new("window-single-key", &TWO);
    let fake = Fake::default();
    let warning = format!(r#"button "Warning" desc {LONE_KEY_NOTE:?}"#);
    let admin = fake.access();
    let no_admin = Access { accessibility: false, needs_admin: true, ..admin };
    for (access, hotkey, warns) in [
      (no_admin, "left_option", true),
      (no_admin, "fn", true),
      (no_admin, "option+space", false),
      (admin, "fn", false),
    ] {
      fake.log().access = Some(access);
      let h = home.harness(&fake, Settings { hotkey: hotkey.into(), ..Settings::without_paste() });
      let mark = a11y_line(&h, r#"button "Warning""#);
      assert_eq!(mark.as_ref() == Some(&warning), warns, "{hotkey}: {mark:?}");
    }
  }

  /// The devices, the permissions, launch at login and the models are read as the window opens
  /// and comes to the front, at once on the event; never because a view ran.
  #[test]
  fn only_the_window_in_front_reads_the_system() {
    let home = Home::new("window-front", &TWO);
    let fake = Fake::default();
    let mut h = home.harness(&fake, Settings::default());
    assert_eq!(fake.log().looks, 1, "as it opened");
    for page in [Page::Advanced, Page::History, Page::General] {
      tab(&mut h, page);
    }

    h.redraw();
    h.frame();
    assert_eq!(fake.log().looks, 1, "not by the frames");

    h.event(Event::Focus(false));
    h.frame();
    assert_eq!(fake.log().looks, 1, "not in the back");
    h.event(Event::Focus(true));
    assert_eq!(fake.log().looks, 2, "in front again, before a frame");
  }
}
