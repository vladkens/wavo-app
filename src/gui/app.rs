// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `wavo-app gui`: the menu bar app on nimbo's app platform ([nimbo::app]), an accessory app: the
//! [Tray] with the [menu], the [Hotkeys] that feed the session's input, the HUD in an [Overlay],
//! the settings window ([Prefs]) on demand: at launch unless the person starts in the menu bar
//! only, from the menu, `wavo-app ctl open-settings` and `open-history`, a second launch and the
//! app opened again (Finder, Spotlight). The session and the control socket are `wavo-app
//! daemon`'s.
//!
//! The main thread owns the app ([App], in a thread local). Other threads reach it only through
//! nimbo's [Proxy] ([proxy]): the session's observer posts every [Update] there, which goes on to
//! the icon, the menu and the HUD. The main thread never waits for the session: what needs an
//! answer from it runs on a thread of its own.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use nimbo::hotkey::{Hotkeys, Listener};
use nimbo::permissions::{self, Access, Pane};
use nimbo::tray::{Menu, Tray};
use nimbo::{AppInfo, Clipboard, Main, Presence, Proxy, SystemClipboard, Window, autostart};
use serde_json::{Value, json};
use tracing::{info, warn};
use wavo::hub::{Hub, ModelInfo};

use super::output::GuiOutput;
use super::paste;
use crate::audio::devices;
use crate::ctl::{self, Autostart, Claim, Reply, Request, Serving};
use crate::history::{self, History};
use crate::session::controller::{Handle, Phase, Update};
use crate::session::{self, Session};
use crate::settings::{self, HudPosition, Settings};
use crate::ui::hud::{self, Mode};
use crate::ui::icons::Icon;
use crate::ui::menu::{self, Models, Warning};
use crate::ui::overlay::{self, Overlay, Show};
use crate::window::downloads::{Download, Downloads};
use crate::window::models::{Found, Pick};
use crate::window::{self, Access, Page, Prefs};

/// The app as the system knows it: its menu bar presence, its login item.
const INFO: AppInfo =
  AppInfo { id: "me.vladkens.wavo", name: "Wavo", presence: Presence::Accessory };
/// How often Accessibility access is checked while it is missing (and only then).
const ACCESS_POLL: Duration = Duration::from_secs(2);
/// How long `wavo-app ctl status` waits for the main thread for the shell's part.
const STATUS_WAIT: Duration = Duration::from_secs(1);

thread_local! {
  static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

static PROXY: OnceLock<Proxy> = OnceLock::new();

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
  /// The models downloaded and the recent dictations (their texts and times), as the menu last
  /// opened.
  found: Vec<Found>,
  /// Which of them transcribes, for the menu's check.
  pick: Pick,
  recent: Vec<(String, i64)>,
  /// The history, for «Recent».
  history: Option<History>,
  accessibility: bool,
  microphone: Access,
  /// The paste may read the clipboard to restore it ([SystemClipboard::readable]).
  clipboard: bool,
  asked_microphone: bool,
  fake_mic: bool,
  session: Session,
  serving: Serving,
}

/// `wavo-app gui`. Returns at once if another instance runs for the same home (it is asked to
/// open its settings), else when the app quits.
pub fn run() -> Result<()> {
  let config = session::Config::from_env();
  let home = config.home.clone();
  std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
  let socket = ctl::socket_path(&home)?;
  let server = match ctl::claim(&home).with_context(|| format!("claim {}", socket.display()))? {
    Claim::Server(server) => server,
    Claim::Running => {
      let reply = ctl::request(&socket, &Request::OpenSettings)?;
      let answer = reply.error.unwrap_or_else(|| "settings opened".into());
      eprintln!("wavo-app is already running for {} ({answer})", home.display());
      return Ok(());
    }
  };

  if config.fake_mic.is_none() {
    devices::warm_up();
  }

  info!("wavo-app for {}: control socket {}", home.display(), socket.display());
  let mut started = Ok(());
  nimbo::app(INFO, |main| {
    // Also on a quit the system asks for (logout), after which `nimbo::app` does not return.
    main.on_quit(|_| shutdown());
    if let Err(e) = start(main, config, server) {
      main.alert("Wavo could not start", &format!("{e:#}"));
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
  let (saved, _) = Settings::load(&home.join(settings::FILE));
  let observer = Box::new(|update: Update| {
    proxy().post(move |main| _ = with_app(|app| app.on_update(main, update)))
  });
  let session = Session::start_observed(config, GuiOutput, observer)?;
  let handle = session.handle();
  let hotkeys = {
    let handle = handle.clone();
    Hotkeys::new(move |edge| handle.input(edge.into()))
  };

  let serving = {
    let handle = handle.clone();
    server.spawn(move |request| serve(request, &handle))?
  };

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
    serving,
  };
  // No system dialog at launch: the menu's warning asks for Accessibility.
  app.check_access();
  app.read_models();
  // Without Accessibility the window opens on Advanced, whose button asks for it.
  let page = (!app.accessibility).then_some(Page::Advanced);
  APP.with(|cell| *cell.borrow_mut() = Some(app));
  // Opened again while it runs: the settings window, as for a second launch.
  main.on_reopen(|main| open_window(main, None));
  if !saved.start_in_tray || page.is_some() {
    proxy().post(move |main| open_window(main, page));
  }

  Ok(())
}

/// The HUD at `position`, hidden until it shows; its overlay tells the app once it has hidden.
fn open_hud(main: &mut Main, position: HudPosition) -> Window<Overlay> {
  let hidden = || proxy().post(|_| _ = with_app(App::hud_hidden));
  main.open(&overlay::config(position), Overlay::new(hidden))
}

/// A control request, on the control thread.
fn serve(request: Request, handle: &Handle) -> Reply {
  match request {
    Request::Status => {
      let mut reply = handle.request(Request::Status);
      if let Value::Object(map) = &mut reply.result {
        let shell = proxy().call(|_| with_app(|app| app.status()), STATUS_WAIT).flatten();
        map.insert("shell".into(), shell.unwrap_or_default());
      }

      reply
    }
    Request::OpenSettings | Request::OpenHistory => {
      let page = (request == Request::OpenHistory).then_some(Page::History);
      proxy().post(move |main| open_window(main, page));
      Reply::ok(json!({ "state": "opened" }))
    }
    // Here: the system takes a while, and the main thread draws meanwhile.
    Request::Autostart { state } => autostart(state, handle),
    Request::Quit => {
      proxy().post(|main| main.quit());
      Reply::ok(json!({ "state": "quitting" }))
    }
    request => handle.request(request),
  }
}

/// `wavo-app ctl autostart`: launch at login as the system has it after the request; an error
/// outside the app bundle.
fn autostart(request: Autostart, handle: &Handle) -> Reply {
  let result = match request {
    Autostart::Status => autostart::state(&INFO),
    Autostart::On | Autostart::Off => {
      let on = request == Autostart::On;
      let result = autostart::set(&INFO, on);
      if result.is_ok() {
        handle.edit_settings(move |s| s.autostart = on);
      }

      result
    }
  };

  let state = match result {
    Ok(autostart::State::Enabled) => "enabled",
    Ok(autostart::State::RequiresApproval) => "requires_approval",
    Ok(autostart::State::NotRegistered) => "not_registered",
    Err(e) => return Reply::error(e),
  };

  Reply::ok(json!({ "state": state }))
}

/// As the app quits: the clipboard a paste holds comes back; the session stops its engine and
/// keeps a recording in progress, then the socket goes (with the lock: the next instance's
/// recovery must not find this one's dictation unfinished).
fn shutdown() {
  let Some(app) = APP.with(|cell| cell.borrow_mut().take()) else { return };
  info!("wavo-app: quit");
  paste::finish();
  drop(app.session);
  drop(app.serving);
}

/// A menu item was chosen, after the menu closed.
fn chosen(main: &mut Main, id: &str) {
  let opened = |done: Result<(), String>| done.unwrap_or_else(|e| warn!("{id}: {e}"));
  let index = |prefix: &str| id.strip_prefix(prefix).and_then(|i| i.parse::<usize>().ok());
  if let Some(i) = index("recent:") {
    with_app(|app| app.copy_recent(i));
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
    // The system's dialog adds the app to the Accessibility list, which the pane alone does not.
    "warn:accessibility" => _ = permissions::accessibility(true),
    "warn:microphone" => opened(permissions::open_pane(Pane::Microphone)),
    "warn:clipboard" => opened(permissions::open_pane(Pane::Pasteboard)),
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
      handle: app.handle.clone(),
      hub: app.hub.clone(),
      downloads: app.downloads.clone(),
    };
    (shell, app.update.clone(), app.hub.clone(), app.home.clone())
  }) else {
    return;
  };

  let settings = update.as_ref().map(|u| u.settings.clone()).unwrap_or_default();
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
      if ended {
        proxy().post(|_| _ = with_app(App::read_models));
      }

      with_window(move |prefs| {
        if ended {
          prefs.models_changed();
        }
      });
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

  fn access(&self) -> Access {
    let microphone = match microphone::access() {
      Microphone::Authorized => Some(true),
      Microphone::Denied | Microphone::Restricted => Some(false),
      Microphone::NotDetermined => None,
    };
    Access { accessibility: permissions::accessibility(false), microphone }
  }

  fn folder_size(&self, dir: PathBuf, done: Box<dyn FnOnce(u64) + Send>) {
    spawn("folder-size", move || done(window::folder_size(&dir)));
  }

  /// The system's dialog adds the app to the Accessibility list, which opening the pane alone
  /// does not; the person asked for it with the button.
  fn open_pane(&self, pane: Pane) {
    if pane == Pane::Accessibility && !permissions::accessibility(false) {
      info!("no Accessibility access: asking for it");
      permissions::accessibility(true);
      return;
    }

    if let Err(e) = permissions::open_pane(pane) {
      warn!("open {pane:?}: {e}");
    }
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
}

impl App {
  fn on_update(&mut self, main: &mut Main, update: Update) {
    if let Some(w) = &self.window {
      w.with(|prefs| prefs.update(&update.settings, &update.status, update.history));
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

        let r = status.recording.as_ref();
        let show = Show::Recording {
          meter: update.meter.clone(),
          device: r.map_or_else(String::new, |r| r.device.clone()),
          seconds: r.map_or(0.0, |r| r.seconds),
          no_model: hud::during(status),
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

  /// A recording starts: the system's prompt, the first time it was never answered.
  fn ask_microphone(&mut self) {
    self.microphone = permissions::microphone();
    if !self.fake_mic && self.microphone == Access::NotDetermined && !self.asked_microphone {
      self.asked_microphone = true;
      info!("asking for microphone access");
      permissions::request_microphone(|granted| {
        info!("microphone access {}", if granted { "granted" } else { "denied" });
      });
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
    let found = self.hub.downloaded();
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
      Ok(entries) => entries.into_iter().map(|e| (e.text, e.created_at)).collect(),
      Err(e) => {
        warn!("the menu's recent dictations: {e:#}");
        Vec::new()
      }
    };
  }

  /// Copies «Recent»'s item `i`.
  fn copy_recent(&mut self, i: usize) {
    if let Some((text, _)) = self.recent.get(i) {
      SystemClipboard.write(text);
      info!("a recent dictation is on the clipboard");
    }
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
    let restores = settings.auto_paste && !settings.keep_in_clipboard;
    let system = menu::System {
      accessibility: self.accessibility,
      tap_failed: hotkeys.listener == Listener::Failed,
      hotkey_error: hotkeys.error,
      fallback_error: hotkeys.fallback_error,
      microphone_denied: matches!(self.microphone, Access::Denied | Access::Restricted),
      clipboard_unread: restores && !self.clipboard,
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
    let recent: Vec<String> =
      self.recent.iter().map(|(text, at)| menu::recent_title(text, *at, now)).collect();
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

  /// What `wavo-app ctl status` shows of the shell.
  fn status(&self) -> Value {
    let hotkeys = self.hotkeys.status();
    let warnings = self.update.as_ref().map(|u| self.warnings(u)).unwrap_or_default();
    json!({
      "icon": self.icon,
      "hotkey": hotkeys.hotkey.map(|h| h.label()),
      "tap": format!("{:?}", hotkeys.listener).to_lowercase(),
      "accessibility": self.accessibility,
      "microphone": access(self.microphone),
      "hud": self.hud.0.with(|o| json!({ "mode": o.mode(), "text": o.text() })),
      "warnings": warnings.iter().map(|w| &w.text).collect::<Vec<_>>(),
    })
  }
}

/// The microphone's access as `wavo-app ctl status` shows it.
fn access(access: Access) -> &'static str {
  match access {
    Access::Granted => "authorized",
    Access::Denied => "denied",
    Access::Restricted => "restricted",
    Access::NotDetermined => "not_determined",
  }
}
