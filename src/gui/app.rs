// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `wavo-app gui`: the menu bar app on nimbo's app platform ([nimbo::app]), an accessory app: the
//! [Tray] with the [menu], the [Hotkeys] that feed the session's input, the HUD in an [Overlay],
//! the settings window ([Prefs]) when the person asks for it: at a launch only when something is
//! missing (a permission or a model: the window opens on General, nothing is asked until the
//! person says so) and never at a launch at login, from the menu, a second launch and the app
//! opened again (Finder, Spotlight). Nothing else opens a window or takes the focus: an error or a
//! permission missing shows in the HUD, the icon, the menu and the log.
//!
//! The main thread owns the app ([App], in a thread local). Other threads reach it only through
//! nimbo's [Proxy] ([proxy]): the session's observer posts every [Update] there, which goes on to
//! the icon, the menu and the HUD. The main thread never waits for the session: what needs an
//! answer from it runs on a thread of its own.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use nimbo::hotkey::{Hotkeys, Listener};
use nimbo::permissions::{self, Access, Pane};
use nimbo::tray::{Menu, Tray};
use nimbo::{Clipboard, Main, Proxy, SystemClipboard, Window, autostart};
use tracing::{error, info, warn};
use wavo::hub::{Hub, ModelInfo};

use super::output::GuiOutput;
use super::paste;
use crate::INFO;
use crate::audio::devices;
use crate::ctl::{self, Claim};
use crate::history::db::Entry;
use crate::history::{self, History};
use crate::session::controller::{Handle, Phase, Request, Update};
use crate::session::{self, Session};
use crate::settings::{self, HudPosition, Settings};
use crate::transfer::archive::{self, Exported};
use crate::ui::hud::{self, Mode};
use crate::ui::icons::Icon;
use crate::ui::menu::{self, Models, Warning};
use crate::ui::overlay::{self, Overlay, Show};
use crate::window::downloads::{Download, Downloads};
use crate::window::history::no_text;
use crate::window::models::{Found, Pick};
use crate::window::{self, Page, Prefs};

/// How often Accessibility access is checked while it is missing (and only then).
const ACCESS_POLL: Duration = Duration::from_secs(2);

thread_local! {
  static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

static PROXY: OnceLock<Proxy> = OnceLock::new();
/// Accessibility's prompt showed in this run: Allow opens System Settings from then on.
static ASKED_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);

/// The main thread, for other threads and for what runs after the AppKit call in progress.
pub fn proxy() -> &'static Proxy {
  PROXY.get().expect("wavo-app gui runs")
}

/// Runs `f` with the app, unless it is gone (quitting) or already borrowed further up the stack.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
  APP.with(|cell| match cell.try_borrow_mut() {
    Ok(mut app) => app.as_mut().map(f),
    Err(_) => {
      warn!("the app is busy: a nested call was dropped");
      None
    }
  })
}

struct App {
  tray: Tray,
  icon: Icon,
  menu: Menu,
  hotkeys: Hotkeys,
  /// The HUD's window and where it shows.
  hud: (Window<Overlay>, HudPosition),
  handle: Handle,
  home: PathBuf,
  hub: Hub,
  /// The session's last; none before its first.
  update: Option<Update>,
  /// The settings window while it is open.
  window: Option<Window<Prefs>>,
  downloads: Downloads,
  /// The models downloaded and the recent dictations, as the menu last opened.
  found: Vec<Found>,
  /// Which of them transcribes, for the menu's check.
  pick: Pick,
  recent: Vec<Entry>,
  /// The history, for «Recent».
  history: Option<History>,
  accessibility: bool,
  microphone: Access,
  /// The paste may read the clipboard to restore it ([SystemClipboard::readable]).
  clipboard: bool,
  asked_microphone: bool,
  fake_mic: bool,
  session: Session,
  server: ctl::Server,
}

/// `wavo-app gui`. Returns at once if another instance runs for the same home (it is asked to
/// open its settings), else when the app quits.
pub fn run() -> Result<()> {
  let config = session::Config::from_env();
  let home = config.home.clone();
  std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
  let server = match ctl::claim(&home).with_context(|| format!("claim {}", home.display()))? {
    Claim::Server(server) => server,
    Claim::Running => {
      let opened = ctl::send(&home, "open").is_ok();
      eprintln!("Wavo is already running for {} (settings opened: {opened})", home.display());
      return Ok(());
    }
  };

  if config.fake_mic.is_none() {
    devices::warm_up();
  }

  info!("wavo-app for {}", home.display());
  let mut started = Ok(());
  nimbo::app(INFO, |main| {
    // Also on a quit the system asks for (logout), after which `nimbo::app` does not return.
    main.on_quit(|_| shutdown());
    if let Err(e) = start(main, config, server) {
      error!("Wavo could not start: {e:#}");
      started = Err(e);
      main.quit();
    }
  });
  started
}

fn start(main: &mut Main, config: session::Config, server: ctl::Server) -> Result<()> {
  let _ = PROXY.set(main.proxy());
  let fake_mic = config.fake_mic.is_some();
  let (home, hub) = (config.home.clone(), config.hub.clone());
  // A model chosen by its path, outside the Hugging Face cache: the session has it before the
  // app's first update, so the check at launch reads it from the file.
  let saved_file = std::fs::read(home.join(crate::settings::FILE))
    .ok()
    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    .and_then(|v| v.get("model")?.as_str().map(PathBuf::from))
    .is_some_and(|p| p.is_file());
  let observer = Box::new(|update: Update| {
    proxy().post(move |main| _ = with_app(|app| app.on_update(main, update)))
  });
  let session = Session::start_observed(config, GuiOutput, observer)?;
  let handle = session.handle();
  let hotkeys = {
    let handle = handle.clone();
    Hotkeys::new(move |edge| handle.input(edge.into()))
  };

  server.serve(|cmd| match cmd {
    "open" => proxy().post(|main| open_window(main, None)),
    "quit" => proxy().post(|main| main.quit()),
    _ => warn!("unknown command {cmd:?}"),
  })?;

  let tray = Tray::new(Icon::Idle.tray(), Icon::Idle.description());
  tray.on_open(|_, _| _ = with_app(App::menu_opening));
  tray.on_item(|main, _, id| chosen(main, id));
  let position = HudPosition::default();
  let mut app = App {
    tray,
    icon: Icon::Idle,
    menu: Menu::default(),
    hotkeys,
    hud: (open_hud(main, position), position),
    handle,
    home,
    hub,
    update: None,
    window: None,
    downloads: Downloads::default(),
    found: Vec::new(),
    pick: Pick::default(),
    recent: Vec::new(),
    history: None,
    accessibility: false,
    microphone: permissions::microphone(),
    clipboard: SystemClipboard.readable(),
    asked_microphone: false,
    fake_mic,
    session,
    server,
  };
  app.check_access();
  app.read_models();
  // Nothing is asked at launch: with a permission or a model missing the window opens on General,
  // whose buttons ask; else the app starts in the menu bar.
  // Never at a launch at login: one the system may have started is left in the menu bar.
  let no_model = app.missing_model() && !saved_file;
  let missing = (app.missing_permission() || no_model) && !may_launch_at_login();
  APP.with(|cell| *cell.borrow_mut() = Some(app));
  // Opened again while it runs: the settings window, as for a second launch.
  main.on_reopen(|main| open_window(main, None));
  if missing {
    proxy().post(|main| open_window(main, Some(Page::General)));
  }

  Ok(())
}

/// The app is a login item, so this launch may be the system's at login: nimbo does not tell a
/// launch at login from the person's (Finder, the Dock), and the window must not open at login.
fn may_launch_at_login() -> bool {
  matches!(autostart::state(&INFO), Ok(autostart::State::Enabled))
}

/// The HUD at `position`, hidden until it shows; its overlay tells the app once it has hidden.
fn open_hud(main: &mut Main, position: HudPosition) -> Window<Overlay> {
  let hidden = || proxy().post(|_| _ = with_app(App::hud_hidden));
  main.open(&overlay::config(position), Overlay::new(hidden))
}

/// As the app quits: the clipboard a paste holds comes back; the session stops its engine and
/// keeps a recording in progress, then the socket goes (with the lock: the next instance's
/// recovery must not find this one's dictation unfinished).
fn shutdown() {
  let Some(app) = APP.with(|cell| cell.borrow_mut().take()) else { return };
  info!("wavo-app: quit");
  paste::finish();
  drop(app.session);
  drop(app.server);
}

/// A menu item was chosen, after the menu closed.
fn chosen(main: &mut Main, id: &str) {
  let index = |prefix: &str| id.strip_prefix(prefix).and_then(|i| i.parse::<usize>().ok());
  // One without text has nothing to copy: History shows it, with Transcribe Again.
  if let Some(i) = index("recent:") {
    if with_app(|app| app.copy_recent(i)) == Some(false) {
      open_window(main, Some(Page::History));
    }

    return;
  }

  if let Some(name) = id.strip_prefix("model:") {
    with_app(|app| app.select_model(name));
    return;
  }

  match id {
    "copy-last" => _ = with_app(App::copy_last),
    "unload" => _ = with_app(App::unload),
    "models" => open_window(main, Some(Page::Models)),
    "history" => open_window(main, Some(Page::History)),
    "settings" => open_window(main, None),
    "quit" => main.quit(),
    // General says why and asks; Models downloads one.
    "warn:accessibility" | "warn:microphone" => open_window(main, Some(Page::General)),
    "warn:clipboard" => allow(Pane::Pasteboard),
    // General explains it in the shortcut's ⓘ.
    "warn:secure-input" => open_window(main, Some(Page::General)),
    "warn:download" | "warn:model" => open_window(main, Some(Page::Models)),
    _ => {}
  }
}

/// Opens the settings window on `page` (or where it was), or brings it to the front.
fn open_window(main: &mut Main, page: Option<Page>) {
  let open = with_app(|app| app.window.clone()).flatten().filter(Window::is_open);
  if let Some(w) = open {
    w.with(|prefs| {
      if let Some(page) = page {
        prefs.page = page;
      }

      prefs.refresh();
    });
    w.front();
    return;
  }

  let Some((shell, update, hub, home)) = with_app(|app| {
    let shell = MacShell {
      home: app.home.clone(),
      handle: app.handle.clone(),
      hub: app.hub.clone(),
      downloads: app.downloads.clone(),
    };
    (shell, app.update.clone(), app.hub.clone(), app.home.clone())
  }) else {
    return;
  };

  // Before the session's first update (the window at launch), the saved settings.
  let settings = match &update {
    Some(u) => u.settings.clone(),
    None => Settings::load(&home.join(settings::FILE)).0,
  };
  let mut prefs = Prefs::new(Box::new(shell), settings, hub, &home);
  if let Some(u) = &update {
    prefs.update(&u.settings, &u.status, u.history);
  }

  prefs.page = page.unwrap_or(Page::General);
  let w = main.open(&window::config(prefs.page), prefs);
  // The recorder may have paused the hotkey; a playback stops with the window.
  w.on_close(|_| {
    with_app(|app| {
      app.window = None;
      app.hotkeys.pause(false);
    });
  });
  w.front();
  with_app(|app| app.window = Some(w));
}

/// Allow… for `pane`: the system's prompt while it can still ask (Accessibility not given nor
/// asked for in this run, whose dialog adds the app to the list, which the pane alone does not;
/// the microphone never asked), else the pane in System Settings.
fn allow(pane: Pane) {
  match pane {
    Pane::Accessibility
      if !permissions::accessibility(false)
        && !ASKED_ACCESSIBILITY.swap(true, Ordering::Relaxed) =>
    {
      info!("no Accessibility access: asking for it");
      permissions::accessibility(true);
      with_window(Prefs::access_changed);
    }
    Pane::Microphone if permissions::microphone() == Access::NotDetermined => {
      request_microphone(|| {})
    }
    _ => {
      if let Err(e) = permissions::open_pane(pane) {
        warn!("open {pane:?}: {e}");
      }
    }
  }
}

/// Shows the system's microphone prompt; once it is answered the app and the window look again,
/// then `then` runs, on the main thread.
fn request_microphone(then: impl FnOnce() + Send + 'static) {
  info!("asking for microphone access");
  permissions::request_microphone(|granted| {
    info!("microphone access {}", if granted { "granted" } else { "denied" });
    proxy().post(|main| {
      with_app(|app| app.microphone_answered(main));
      with_window(Prefs::access_changed);
      then();
    });
  });
}

/// Runs `f` on a thread of its own named `name`.
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
  if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
    warn!("start a thread for {name}: {e}");
  }
}

/// Runs `f` on the settings window on the main thread, if it is open; while the window is busy
/// further up the stack (a pop-up's menu runs its own loop), again a moment later: `f` brings a
/// result the window waits for.
fn with_window(f: impl FnOnce(&mut Prefs) + Send + 'static) {
  proxy().post(|_| {
    let Some(w) = with_app(|app| app.window.clone()).flatten() else { return };
    let mut f = Some(f);
    w.with(|prefs| f.take().map(|f| f(prefs)));
    if let Some(f) = f.filter(|_| w.is_open()) {
      proxy().after(Duration::from_millis(50), |_| with_window(f));
    }
  });
}

/// The settings window's way to the app: the session's handle, the downloads, and the main
/// thread's proxy for whatever touches the app's state (the window runs inside a borrow of it at
/// times). What may wait (the system's login items, a folder's size, a deletion) runs on a
/// thread of its own: the window keeps drawing.
struct MacShell {
  home: PathBuf,
  handle: Handle,
  hub: Hub,
  downloads: Downloads,
}

impl window::Shell for MacShell {
  fn edit(&self, edit: Box<dyn FnOnce(&mut Settings) + Send>) {
    self.handle.edit_settings(edit);
  }

  fn pause_hotkey(&self, paused: bool) {
    proxy().post(move |_| _ = with_app(|app| app.hotkeys.pause(paused)));
  }

  fn devices(&self) -> Vec<devices::Info> {
    devices::list()
  }

  /// The window shows how it goes; once it ends the models are looked at again: the menu's (and
  /// so the session's) and the window's.
  fn download(&self, model: &'static ModelInfo) {
    self.downloads.pull(self.hub.clone(), model, |ended| {
      if !ended {
        // Progress only redraws: not retried while the window is busy, where it piles up.
        return proxy().post(|_| {
          if let Some(w) = with_app(|app| app.window.clone()).flatten() {
            w.with(|_| ());
          }
        });
      }

      proxy().post(|_| _ = with_app(App::read_models));
      with_window(Prefs::models_changed);
    });
  }

  fn cancel_download(&self, name: &str) {
    self.downloads.cancel(name);
  }

  fn download_state(&self, name: &str) -> Option<Download> {
    self.downloads.get(name)
  }

  /// Once it is gone, the session forgets a choice of it (the first model left takes its
  /// place) and lets go of its file.
  fn delete_model(&self, model: &'static ModelInfo) {
    let (handle, hub) = (self.handle.clone(), self.hub.clone());
    spawn("model-delete", move || {
      let result = window::models::delete(&hub, model);
      if result.is_ok() {
        let root = hub.clone();
        handle.edit_settings(move |s| window::models::forget(s, &root, model));
        handle.check_model();
      }

      with_window(move |prefs| prefs.model_deleted(model, result));
    });
  }

  fn check_model(&self) {
    self.handle.check_model();
  }

  fn retranscribe(&self, id: i64) {
    let handle = self.handle.clone();
    spawn("retranscribe", move || {
      if let Some(e) = handle.request(Request::Retranscribe { id }).error {
        warn!("transcribe history entry {id} again: {e}");
      }
    });
  }

  fn undo_transcribe(&self, id: i64) {
    let handle = self.handle.clone();
    spawn("undo-transcribe", move || {
      if let Some(e) = handle.request(Request::UndoTranscribe { id }).error {
        warn!("undo the transcription of history entry {id}: {e}");
      }
    });
  }

  fn delete_entry(&self, id: i64) {
    let handle = self.handle.clone();
    spawn("history-delete", move || {
      if let Some(e) = handle.request(Request::Delete { id }).error {
        warn!("delete history entry {id}: {e}");
        with_window(move |prefs| prefs.entry_not_deleted(&e));
      }
    });
  }

  fn autostart(&self) -> Result<autostart::State, String> {
    autostart::state(&INFO)
  }

  /// The system's register and unregister wait for it (and show its notification).
  fn set_autostart(&self, on: bool) {
    let handle = self.handle.clone();
    spawn("autostart", move || {
      let result = autostart::set(&INFO, on);
      if result.is_ok() {
        handle.edit_settings(move |s| s.autostart = on);
      }

      with_window(move |prefs| prefs.autostart_set(on, result));
    });
  }

  fn access(&self) -> window::Access {
    window::Access {
      accessibility: permissions::accessibility(false),
      accessibility_asked: ASKED_ACCESSIBILITY.load(Ordering::Relaxed),
      microphone: permissions::microphone(),
      secure_input: permissions::secure_input(),
      clipboard_unread: !SystemClipboard.readable(),
    }
  }

  fn folder_size(&self, dir: PathBuf, done: Box<dyn FnOnce(u64) + Send>) {
    spawn("folder-size", move || done(window::folder_size(&dir)));
  }

  fn allow(&self, pane: Pane) {
    allow(pane);
  }

  fn open_url(&self, url: &str) {
    if let Err(e) = nimbo::open::url(url) {
      warn!("open {url}: {e}");
    }
  }

  fn reveal(&self, dir: &Path) {
    if let Err(e) = nimbo::open::reveal(dir) {
      warn!("show {}: {e}", dir.display());
    }
  }

  fn export(&self, file: PathBuf, done: Box<dyn FnOnce(Result<Exported, String>) + Send>) {
    let home = self.home.clone();
    spawn("export", move || {
      let result = archive::export(&home, &file);
      match &result {
        Ok(_) => _ = nimbo::open::reveal(&file).inspect_err(|e| warn!("show the archive: {e}")),
        Err(e) => warn!("export to {}: {e:#}", file.display()),
      }

      done(result.map_err(|e| format!("{e:#}")));
    });
  }
}

impl App {
  fn on_update(&mut self, main: &mut Main, update: Update) {
    // Retried while the window is busy (a pop-up's menu): an update dropped would leave it behind
    // until the session's next one. A retry a newer update overtook is dropped: it would put the
    // window back to an older state.
    static LATEST: AtomicU64 = AtomicU64::new(0);
    if self.window.is_some() {
      let (settings, status, history) =
        (update.settings.clone(), update.status.clone(), update.history);
      let at = LATEST.fetch_add(1, Ordering::Relaxed) + 1;
      with_window(move |prefs| {
        if LATEST.load(Ordering::Relaxed) == at {
          prefs.update(&settings, &status, history);
        }
      });
    }

    let status = &update.status;
    // Its error shows in the menu.
    let _ = self.hotkeys.set(&update.settings.hotkey);
    self.hotkeys.escape(status.state == Phase::Recording);
    let position = update.settings.hud_position;
    let shown = self.hud.0.with(|o| o.mode()).unwrap_or(Mode::Hidden);
    match status.state {
      Phase::Recording => {
        let recording = self.update.as_ref().is_some_and(|u| u.status.state == Phase::Recording);
        if !recording {
          self.ask_microphone();
        }

        // A permission missing: the HUD says which, and only the HUD (the menu too): no window
        // opens by itself. A denied microphone sends silence, which would say "no sound" instead.
        let hint = self.permission_hint(update.settings.auto_paste);
        let r = status.recording.as_ref();
        let show = Show::Recording {
          meter: update.meter.clone().filter(|_| !self.mic_denied()),
          device: r.map_or_else(String::new, |r| r.device.clone()),
          seconds: r.map_or(0.0, |r| r.seconds),
          no_model: hint.map(str::to_owned).or_else(|| hud::during(status)),
        };
        self.show(main, shown, show, position);
      }
      Phase::Transcribing | Phase::Pasting => {
        self.show(main, shown, Show::Transcribing(status.engine.progress), position)
      }
      Phase::Idle => {
        let before = self.update.as_ref().map(|u| &u.status);
        let show = match hud::after(status, before, update.settings.auto_paste) {
          Some(hud::Outcome::Error(reason)) => Show::Error(reason),
          None if shown == Mode::Error => return self.keep(update),
          _ => Show::Collapse,
        };

        self.show(main, shown, show, position);
      }
    }

    self.keep(update);
  }

  /// Keeps `update` as the last and shows it in the icon and the menu.
  fn keep(&mut self, update: Update) {
    let model = |u: &Option<Update>| u.as_ref().map(|u| u.settings.model.clone());
    let chosen = model(&self.update);
    self.update = Some(update);
    if model(&self.update) != chosen {
      self.repick();
    }

    self.refresh();
  }

  /// Shows `show` in the HUD, which shows `shown`; from hidden only a recording or an error, at
  /// `position` on the screen with the pointer.
  fn show(&mut self, main: &mut Main, shown: Mode, show: Show, position: HudPosition) {
    let hidden = shown == Mode::Hidden;
    if hidden && !matches!(show, Show::Recording { .. } | Show::Error(_)) {
      return;
    }

    if hidden && position != self.hud.1 {
      self.hud.0.close();
      self.hud = (open_hud(main, position), position);
    }

    self.hud.0.with(|o| o.show(show));
    if hidden {
      self.hud.0.front();
    }
  }

  /// The HUD's collapse ended: its window goes, unless it shows again already.
  fn hud_hidden(&mut self) {
    if self.hud.0.with(|o| o.mode()) == Some(Mode::Hidden) {
      self.hud.0.hide();
    }
  }

  fn copy_last(&mut self) {
    let last = self.update.as_ref().and_then(|u| u.status.last.as_ref());
    if let Some(last) = last.filter(|l| !l.text.is_empty()) {
      SystemClipboard.write(&last.text);
      info!("the last text is on the clipboard");
    }
  }

  fn unload(&mut self) {
    let handle = self.handle.clone();
    spawn("unload", move || {
      if let Some(e) = handle.request(Request::Unload).error {
        warn!("unload the model: {e}");
      }
    });
  }

  /// Checks Accessibility; without it the event tap cannot start (chords still work through
  /// Carbon, pasting does not), so it is checked again every [ACCESS_POLL] until the tap runs. It
  /// never asks: the menu's warning does.
  fn check_access(&mut self) {
    self.accessibility = permissions::accessibility(false);
    // The tap starts in the background: how it went shows at the next poll.
    let listener = self.hotkeys.status().listener;
    if self.accessibility
      && listener != Listener::Active
      && let Err(e) = self.hotkeys.start()
    {
      warn!("the event tap: {e}");
    }

    if !self.accessibility || self.hotkeys.status().listener != Listener::Active {
      proxy().after(ACCESS_POLL, |_| _ = with_app(App::poll_access));
    }
  }

  fn poll_access(&mut self) {
    let (had, tap) = (self.accessibility, self.hotkeys.status().listener);
    self.check_access();
    if self.accessibility != had {
      info!("Accessibility access granted");
    }

    if self.accessibility != had || self.hotkeys.status().listener != tap {
      self.refresh();
    }
  }

  /// Accessibility or the microphone is not allowed (or not yet).
  fn missing_permission(&self) -> bool {
    !self.accessibility || self.no_microphone()
  }

  /// No model transcribes: none downloaded, and no file of its own chosen.
  fn missing_model(&self) -> bool {
    self.pick.active.is_none() && self.pick.other.as_ref().is_none_or(|(_, there)| !there)
  }

  /// The microphone is denied: the system won't ask again.
  fn mic_denied(&self) -> bool {
    !self.fake_mic && matches!(self.microphone, Access::Denied | Access::Restricted)
  }

  /// What the HUD says while recording without a permission dictation needs: the microphone's,
  /// else Accessibility's for the paste.
  fn permission_hint(&self, auto_paste: bool) -> Option<&'static str> {
    match (self.mic_denied(), self.accessibility || !auto_paste) {
      (true, _) => Some("allow the microphone to record"),
      (false, false) => Some("allow Accessibility to paste"),
      (false, true) => None,
    }
  }

  /// A recording starts: the system's prompt, the first time in this run it was never answered.
  fn ask_microphone(&mut self) {
    self.microphone = permissions::microphone();
    if self.may_ask_microphone() {
      self.asked_microphone = true;
      request_microphone(|| {});
    }
  }

  /// The microphone was never asked for in this run, nor answered.
  fn may_ask_microphone(&self) -> bool {
    !self.fake_mic && self.microphone == Access::NotDetermined && !self.asked_microphone
  }

  /// Dictation can't record: the microphone is not allowed (or not yet).
  fn no_microphone(&self) -> bool {
    !self.fake_mic && self.microphone != Access::Granted
  }

  /// The microphone's prompt was answered. Denied during the recording that asked for it: the HUD
  /// says so, as for a press without it.
  fn microphone_answered(&mut self, main: &mut Main) {
    self.microphone = permissions::microphone();
    self.refresh();
    let recording = self.update.clone().filter(|u| u.status.state == Phase::Recording);
    if let Some(update) = recording.filter(|_| self.mic_denied()) {
      self.on_update(main, update);
    }
  }

  /// The menu opens: what the system does not tell the app is looked at again, and the models
  /// and the history.
  fn menu_opening(&mut self) {
    self.microphone = permissions::microphone();
    self.clipboard = SystemClipboard.readable();
    self.read_models();
    self.read_recent();
    self.refresh();
  }

  /// The models downloaded, for the menu; the session looks too when they changed.
  fn read_models(&mut self) {
    let found = window::models::ready(self.hub.downloaded(), |name| self.downloads.get(name));
    if found != self.found {
      self.found = found;
      self.handle.check_model();
    }

    self.repick();
  }

  /// The latest dictations, for «Recent», from the history opened the first time.
  fn read_recent(&mut self) {
    if self.history.is_none() {
      self.history = History::open(&self.home).inspect_err(|e| warn!("the history: {e:#}")).ok();
    }

    let Some(history) = &self.history else { return };
    let entries = history.db.get_page("", menu::RECENT);
    self.recent = match entries {
      Ok(entries) => entries,
      Err(e) => {
        warn!("the menu's recent dictations: {e:#}");
        Vec::new()
      }
    };
  }

  /// Copies «Recent»'s item `i`; false if it has no text.
  fn copy_recent(&mut self, i: usize) -> bool {
    let Some(e) = self.recent.get(i).filter(|e| !e.text.is_empty()) else { return false };
    SystemClipboard.write(&e.text);
    info!("a recent dictation is on the clipboard");
    true
  }

  /// The menu's model `i` is used from now on.
  fn select_model(&mut self, name: &str) {
    info!("model {name} chosen in the menu");
    let name = name.to_owned();
    self.handle.edit_settings(move |s| s.model = Some(name));
  }

  /// Which model transcribes, after the models on disk or the choice changed.
  fn repick(&mut self) {
    let query = self.update.as_ref().and_then(|u| u.settings.model.as_deref());
    self.pick = Pick::new(&self.hub, query, &self.found);
  }

  /// What needs the person's attention, as the session and the system have it now.
  fn warnings(&self, update: &Update) -> Vec<Warning> {
    let hotkeys = self.hotkeys.status();
    let settings = &update.settings;
    let system = menu::System {
      accessibility: self.accessibility,
      tap_failed: hotkeys.listener == Listener::Failed,
      hotkey_error: hotkeys.error,
      fallback_error: hotkeys.fallback_error,
      no_microphone: self.no_microphone(),
      clipboard_unread: settings.restores_clipboard() && !self.clipboard,
      secure_input: hotkeys.secure_input,
    };
    menu::warnings(&system, &settings.hotkey, &update.status)
  }

  /// The icon and the menu.
  fn refresh(&mut self) {
    let Some(update) = &self.update else { return };
    let warnings = self.warnings(update);
    let icon = menu::icon(&update.status, &warnings);
    if icon != self.icon {
      self.icon = icon;
      self.tray.set_icon(icon.tray());
      self.tray.set_tooltip(icon.description());
    }

    let hotkey = self.hotkeys.status().hotkey.map(|h| h.label());
    let now = history::now_ms();
    let recent: Vec<String> = (self.recent.iter())
      .map(|e| {
        let empty = e.text.is_empty().then(|| no_text(e.tried_model.as_deref()));
        menu::recent_title(empty.as_deref().unwrap_or(&e.text), e.created_at, now)
      })
      .collect();
    let menu = menu::menu(
      hotkey.as_deref(),
      &update.status,
      &warnings,
      &recent,
      &Models::new(&self.found, &self.pick),
    );
    if menu != self.menu {
      self.tray.set_menu(&menu);
      self.menu = menu;
    }
  }
}
