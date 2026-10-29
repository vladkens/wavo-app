// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar app on nimbo: the tray with its menu, the hotkeys that feed the session, the HUD,
//! and the settings window when the person asks for it (the menu, a second launch, the app opened
//! again) or, at a launch that is not at login, when a permission or a model is missing. Nothing
//! else opens a window or takes the focus: problems show in the HUD, the icon, the menu and the
//! log.
//!
//! The main thread owns the app (`APP`); other threads reach it through nimbo's [Proxy]. The main
//! thread never waits for the session: what needs its answer runs on a thread of its own, as do
//! the update's check, download and install ([updater]).

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use nimbo::focus::{self, Focused};
use nimbo::hotkey::{Hotkeys, Listener};
use nimbo::instance::{self, Claim, Instance};
use nimbo::paste::{self, PORTAL_WAIT, Paste};
use nimbo::permissions::{self, Access, Pane};
use nimbo::tray::{Menu, Tray};
use nimbo::update::dialog::{self, About, Act, State, Updater};
use nimbo::update::{self, Release, Staged};
use nimbo::{Clipboard, Main, Png, Proxy, SystemClipboard, Window, autostart};
use tracing::{error, info, warn};
use wavo::hub::{self, Hub, ModelInfo};

use crate::history::{self, Entry, History};
use crate::hud::{self, Hud, Mode, Show};
use crate::menu::{self, Icon, Warning};
use crate::session::{self, Handle, NotPasted, Phase, Request, Session, TextOutput, Update};
use crate::settings::{self, HudPosition, Settings};
use crate::window::{
  self, Download, Downloads, Export, Found, Import, Page, Pick, Prefs, Transfer, no_text,
};
use crate::{INFO, audio, transfer, updater};

// How often Accessibility is checked while it is missing or the event tap is not running.
const ACCESS_POLL: Duration = Duration::from_secs(2);
// How long the session waits for the main thread to paste or copy; on Wayland nimbo's PORTAL_WAIT,
// as the first paste waits for the desktop's dialog there.
const MAIN_WAIT: Duration = Duration::from_secs(3);

thread_local! {
  static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

static PROXY: OnceLock<Proxy> = OnceLock::new();
// Accessibility's prompt showed in this run: Allow opens System Settings from then on.
static ASKED_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);

fn proxy() -> &'static Proxy {
  PROXY.get().unwrap()
}

/// Runs `f` with the app, unless it is gone (quitting) or borrowed further up the stack.
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
  hud: (Window<Hud>, HudPosition),
  handle: Handle,
  home: PathBuf,
  hub: Hub,
  update: Option<Update>,
  window: Option<Window<Prefs>>,
  downloads: Downloads,
  export: Arc<Mutex<Option<Export>>>,
  import: Arc<Mutex<Option<Import>>>,
  // The models downloaded and the recent dictations, as the menu last opened.
  found: Vec<Found>,
  pick: Pick,
  devices: Vec<audio::Device>,
  lid_closed: bool,
  recent: Vec<Entry>,
  /// What the menu's Recent and the window read: one connection on the main thread, besides the
  /// session's.
  history: Option<Rc<History>>,
  accessibility: bool,
  // The hotkey listener may start: everywhere but where it asks the person
  // ([nimbo::hotkey::start_asks]) before they chose the menu's Allow the Hotkey… once.
  may_listen: bool,
  microphone: Access,
  asked_microphone: bool,
  // The paste may read the clipboard to restore it.
  clipboard: bool,
  fake_mic: bool,
  /// Shared with the update's window.
  updater: Arc<Mutex<Updater>>,
  update_window: Option<Window<UpdateWindow>>,
  /// The update downloaded, put in place once no dictation runs.
  staged: Option<Staged>,
  /// The update goes in: no dictation starts, and the app quits once its installer runs.
  installing: bool,
  /// The installer runs: the app quits once no dictation runs, [QUIT_WAIT] at the latest.
  quitting: bool,
  /// The settings window records a shortcut: the hotkey is paused for it.
  recording_shortcut: bool,
  session: Session,
}

/// How long a dictation the installer found running may go on: nimbo's installer waits 30 s for
/// the app to quit.
const QUIT_WAIT: Duration = Duration::from_secs(25);

/// Returns at once when another instance runs for the same home (it opens its settings), else when
/// the app quits. `updated`: the failure of the update that started this app, from nimbo's
/// [update::finish].
pub fn run(updated: Option<io::Error>) -> Result<()> {
  let config = session::Config::from_env();
  let home = config.home.clone();
  std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
  let claim = instance::claim_in(&home).with_context(|| format!("claim {}", home.display()))?;
  let instance = match claim {
    Claim::Primary(instance) => instance,
    Claim::Running => {
      let opened = instance::send_in(&home, instance::OPEN).is_ok();
      eprintln!("Wavo is already running for {} (settings opened: {opened})", home.display());
      return Ok(());
    }
  };

  if config.fake_mic.is_none() {
    audio::warm_up();
  }

  info!("wavo-app {} for {}", crate::VERSION, home.display());
  let mut started = Ok(());
  nimbo::app(INFO, |main| {
    // Also on a quit the system asks for (logout), after which `nimbo::app` does not return.
    main.on_quit(|_| shutdown());
    if let Err(e) = start(main, config, instance, updated) {
      error!("Wavo could not start: {e:#}");
      started = Err(e);
      main.quit();
    }
  });
  started
}

fn start(
  main: &mut Main,
  config: session::Config,
  instance: Instance,
  updated: Option<io::Error>,
) -> Result<()> {
  let _ = PROXY.set(main.proxy());
  // Linux only: the dock, Wayland's window icon and the portals' dialogs find the app by it.
  if installed() {
    match nimbo::desktop::install(&INFO, &crate::LAUNCHER) {
      Ok(true) => info!("the launcher entry written"),
      Ok(false) => {}
      Err(e) => warn!("the launcher entry: {e}"),
    }
  }

  // Asked once: nimbo tries a staging folder beside the app.
  let supported = updater::fake().is_some()
    || installed()
      && !update::dev(crate::VERSION)
      && update::supported().inspect_err(|e| info!("updates: {e}")).is_ok();
  let fake_mic = config.fake_mic.is_some();
  let (home, hub) = (config.home.clone(), config.hub.clone());
  let observer = Box::new(|update: Update| {
    proxy().post(move |main| _ = with_app(|app| app.on_update(main, update)))
  });
  let session = Session::start(config, GuiOutput, observer)?;
  let handle = session.handle();
  // After the session's: it creates and migrates the history.
  let history = History::open(&home).inspect_err(|e| warn!("the history: {e:#}")).ok().map(Rc::new);
  let hotkeys = {
    let handle = handle.clone();
    Hotkeys::new(move |edge| handle.input(edge.into()))
  };

  // `open` shows the settings (the reopen hook below) and `quit` quits: nimbo's.
  let toggle = handle.clone();
  main.serve(instance, move |_, cmd| match cmd.name.as_str() {
    instance::OPEN | instance::QUIT => {}
    // None while an update goes in: the app quits after it.
    "toggle" if with_app(|app| app.installing) != Some(true) => {
      toggle.input(session::Input::Toggle)
    }
    "toggle" => info!("toggle: an update goes in"),
    other => warn!("unknown command {other:?}"),
  })?;

  // Windows and Linux start the program their entry names: after an update or a move that is the
  // old one, or none, while General still says On. The entry is written again for this one.
  if !cfg!(target_os = "macos") && installed() {
    spawn("autostart", || {
      let wanted = autostart::state(&INFO).is_ok_and(|s| s.wanted());
      if wanted && let Err(e) = autostart::set(&INFO, true) {
        warn!("launch at login: {e}");
      }
    });
  }

  // The session has not told its settings yet: they are read from the file.
  let saved = Settings::load(&home.join(settings::FILE)).0;
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
    export: Arc::default(),
    import: Arc::default(),
    found: Vec::new(),
    pick: Pick::default(),
    devices: audio::devices(),
    lid_closed: audio::lid_closed(),
    recent: Vec::new(),
    history,
    accessibility: false,
    may_listen: !nimbo::hotkey::start_asks() || saved.hotkey_portal,
    microphone: permissions::microphone(),
    asked_microphone: false,
    clipboard: SystemClipboard.readable(),
    fake_mic,
    updater: Arc::new(Mutex::new(Updater::new(supported))),
    update_window: None,
    staged: None,
    installing: false,
    quitting: false,
    recording_shortcut: false,
    session,
  };
  if let Some(e) = updated {
    warn!("update: {e}");
    app.updater.lock().unwrap().relaunched(&e);
  }

  app.check_access();
  app.read_models();
  if supported {
    proxy().post(|_| _ = with_app(App::update_tick));
  }

  // Nothing is asked at launch: General's buttons ask. Never at a launch at login.
  let pick = Pick::new(&app.hub, saved.model.as_deref(), &app.found);
  let no_model = pick.active.is_none() && pick.other.as_ref().is_none_or(|(_, on)| !on);
  let missing = !app.accessibility || app.mic() != Access::Granted || no_model;
  let missing = missing && !may_launch_at_login();
  APP.with(|cell| *cell.borrow_mut() = Some(app));
  main.on_reopen(|main| open_window(main, None));
  if missing {
    proxy().post(|main| open_window(main, Some(Page::General)));
  }

  Ok(())
}

/// nimbo does not tell a launch at login from the person's: a login item may be one.
fn may_launch_at_login() -> bool {
  matches!(autostart::state(&INFO), Ok(autostart::State::Enabled))
}

fn open_hud(main: &mut Main, position: HudPosition) -> Window<Hud> {
  let hidden = || proxy().post(|_| _ = with_app(App::hud_hidden));
  main.open(&hud::config(position), Hud::new(hidden))
}

/// The session keeps a recording in progress; nimbo then lets the instance's lock go as the loop
/// ends, so the next instance does not find this one's dictation unfinished, and puts back the
/// clipboard a paste holds.
fn shutdown() {
  let Some(app) = APP.with(|cell| cell.borrow_mut().take()) else { return };
  info!("wavo-app: quit");
  drop(app.session);
}

fn chosen(main: &mut Main, id: &str) {
  // One without text has nothing to copy: History shows it, with Transcribe Again.
  if let Some(i) = id.strip_prefix("recent:").and_then(|i| i.parse().ok()) {
    if with_app(|app| app.copy_recent(i)) == Some(false) {
      open_window(main, Some(Page::History));
    }

    return;
  }

  if let Some(i) = id.strip_prefix("mic:").and_then(|i| i.parse().ok()) {
    with_app(|app| app.choose_mic(i));
    return;
  }

  if id == menu::UPDATE || id == menu::CHECK_UPDATES {
    return open_update(main);
  }

  if let Some(name) = id.strip_prefix("model:") {
    let name = name.to_owned();
    info!("model {name} chosen in the menu");
    with_app(|app| app.handle.edit_settings(move |s| s.model = Some(name)));
    return;
  }

  match id {
    "copy-last" => _ = with_app(App::copy_last),
    "unload" => _ = with_app(App::unload),
    "models" | "warn:download" | "warn:model" => open_window(main, Some(Page::Models)),
    "history" => open_window(main, Some(Page::History)),
    "settings" => open_window(main, None),
    "quit" => main.quit(),
    "warn:accessibility" | "warn:microphone" | "warn:secure-input" | "warn:hotkey" => {
      open_window(main, Some(Page::General))
    }
    "warn:clipboard" => allow(Pane::Pasteboard),
    "warn:hotkey-portal" => _ = with_app(App::allow_portal),
    "warn:paste-portal" => allow_paste(),
    _ => {}
  }
}

/// The menu's Allow Paste… on Wayland: the desktop's RemoteDesktop portal asks the person now, not
/// at the first paste. Its session starts with the first look at the clipboard, which waits for the
/// answer in a nested main loop: outside the app's borrow.
fn allow_paste() {
  let _ = SystemClipboard.snapshot();
  with_app(App::refresh);
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

  let Some((shell, update, hub, home, history)) = with_app(|app| {
    let shell = AppShell {
      home: app.home.clone(),
      handle: app.handle.clone(),
      hub: app.hub.clone(),
      downloads: app.downloads.clone(),
      export: app.export.clone(),
      import: app.import.clone(),
      updater: app.updater.clone(),
    };
    (shell, app.update.clone(), app.hub.clone(), app.home.clone(), app.history.clone())
  }) else {
    return;
  };

  // Before the session's first update (the window at launch), the saved settings.
  let settings = match &update {
    Some(u) => u.settings.clone(),
    None => Settings::load(&home.join(settings::FILE)).0,
  };
  let mut prefs = Prefs::new(Box::new(shell), settings, hub, &home, history);
  if let Some(u) = &update {
    prefs.update(&u.settings, &u.status, u.history);
  }

  prefs.page = page.unwrap_or(Page::General);
  let w = main.open(&window::config(prefs.page), prefs);
  // The shortcut recorder may have paused the hotkey.
  w.on_close(|_| {
    with_app(|app| {
      app.window = None;
      app.recording_shortcut = false;
      app.pause_hotkeys();
    });
  });
  w.front();
  with_app(|app| app.window = Some(w));
}

/// The system's prompt while it can still ask, else the pane in System Settings. Accessibility's
/// prompt (once per run) adds the app to the list, which the pane alone does not.
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
    Pane::Microphone if permissions::microphone() == Access::NotDetermined => request_microphone(),
    _ => {
      if let Err(e) = permissions::open_pane(pane) {
        warn!("open {pane:?}: {e}");
      }
    }
  }
}

fn request_microphone() {
  info!("asking for microphone access");
  permissions::request_microphone(|granted| {
    info!("microphone access {}", if granted { "granted" } else { "denied" });
    proxy().post(|main| {
      with_app(|app| app.microphone_answered(main));
      with_window(Prefs::access_changed);
    });
  });
}

/// The person's Wavo, not a test or a build run with its own `WAVO_HOME`: only it writes the
/// desktop's entries for the app (launcher, login), which a build would take over.
fn installed() -> bool {
  std::env::var_os("WAVO_HOME").is_none()
}

/// No dictation runs.
fn idle(update: &Update) -> bool {
  update.status.state == Phase::Idle
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
  std::thread::Builder::new().name(name.into()).spawn(f).unwrap();
}

/// Redraws the settings window if it is open, for progress: not retried while the window is busy,
/// where it piles up.
fn redraw() {
  proxy().post(|_| {
    if let Some(w) = with_app(|app| app.window.clone()).flatten() {
      w.redraw();
    }
  });
}

/// Runs `f` on the settings window if it is open; while the window is busy further up the stack (a
/// pop-up's menu runs its own loop), again a moment later.
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

impl App {
  fn on_update(&mut self, main: &mut Main, update: Update) {
    // The latest update when it runs: a retry must not put the window back to an older one.
    if self.window.is_some() {
      with_window(|prefs| {
        if let Some(u) = with_app(|app| app.update.clone()).flatten() {
          prefs.update(&u.settings, &u.status, u.history);
        }
      });
    }

    let status = &update.status;
    let _ = self.hotkeys.set(&update.settings.hotkey); // its error shows in the menu
    self.hotkeys.escape(status.state != Phase::Idle);
    let position = update.settings.hud_position;
    let shown = self.hud.0.with(|h| h.mode()).unwrap_or(Mode::Hidden);
    let show = match status.state {
      Phase::Recording => {
        let recording = self.update.as_ref().is_some_and(|u| u.status.state == Phase::Recording);
        if !recording {
          self.ask_microphone();
        }

        // A permission missing shows here and in the menu: no window opens by itself. A denied
        // microphone sends silence: no level to show.
        let denied = matches!(self.mic(), Access::Denied | Access::Restricted);
        let hint = match () {
          _ if denied => Some("allow the microphone to record".to_owned()),
          _ if update.settings.auto_paste && !self.accessibility => {
            Some("allow Accessibility to paste".to_owned())
          }
          _ => hud::during(status),
        };
        let r = status.recording.as_ref();
        Show::Recording {
          meter: update.meter.clone().filter(|_| !denied),
          device: r.map_or_else(String::new, |r| r.device.clone()),
          seconds: r.map_or(0.0, |r| r.seconds),
          warning: hint,
        }
      }
      Phase::Transcribing => Show::Transcribing(status.engine.progress),
      Phase::Idle => {
        let before = self.update.as_ref().map(|u| &u.status);
        let (paste, took) = (update.settings.auto_paste, update.settings.show_transcription_time);
        match hud::after(status, before, paste, took) {
          Some(show) => show,
          // An error or the time shown stays for its time.
          None if matches!(shown, Mode::Error | Mode::Done) => return self.keep(update),
          None => Show::Collapse,
        }
      }
    };

    self.show(main, shown, show, position);
    self.keep(update);
  }

  fn keep(&mut self, update: Update) {
    let model = |u: &Option<Update>| u.as_ref().map(|u| u.settings.model.clone());
    let chosen = model(&self.update);
    self.update = Some(update);
    if model(&self.update) != chosen {
      self.repick();
    }

    self.install_staged();
    self.quit_when_idle();
    self.refresh();
  }

  /// From hidden only a recording or an error shows, on the screen with the pointer. Never where
  /// the overlay would take the keyboard focus ([Main::overlay_takes_focus]: GNOME on Wayland
  /// without XWayland): the person's window would lose it, and the paste would go into the HUD.
  /// The tray icon tells the state there too.
  fn show(&mut self, main: &mut Main, shown: Mode, show: Show, position: HudPosition) {
    if main.overlay_takes_focus() {
      return;
    }

    let hidden = shown == Mode::Hidden;
    if hidden && !matches!(show, Show::Recording { .. } | Show::Error(_)) {
      return;
    }

    if hidden && position != self.hud.1 {
      self.hud.0.close();
      self.hud = (open_hud(main, position), position);
    }

    self.hud.0.with(|h| h.show(show));
    if hidden {
      self.hud.0.front();
    }
  }

  /// The HUD's collapse ended: its window goes, unless it shows again already.
  fn hud_hidden(&mut self) {
    if self.hud.0.with(|h| h.mode()) == Some(Mode::Hidden) {
      self.hud.0.hide();
    }
  }

  fn copy_last(&mut self) {
    let last = self.update.as_ref().and_then(|u| u.status.last.as_ref());
    if let Some(last) = last.filter(|l| !l.text.is_empty()) {
      SystemClipboard.write(&last.text);
    }
  }

  fn unload(&mut self) {
    let handle = self.handle.clone();
    spawn("unload", move || {
      if let Err(e) = handle.request(Request::Unload) {
        warn!("unload the model: {e}");
      }
    });
  }

  /// Without Accessibility the event tap cannot start (chords still work through Carbon, pasting
  /// does not): on macOS it is checked again until the tap runs. It never asks: the menu's warning
  /// does. Elsewhere the listener starts once: on Linux it may be the GlobalShortcuts portal, whose
  /// dialog a retry would bring back after the person cancelled it.
  fn check_access(&mut self) {
    self.accessibility = permissions::accessibility(false);
    // The tap starts in the background: how it went shows at the next poll.
    if self.accessibility
      && self.may_listen
      && self.hotkeys.status().listener != Listener::Active
      && let Err(e) = self.hotkeys.start()
    {
      warn!("the hotkey listener: {e}");
    }

    let waiting = !self.accessibility || self.hotkeys.status().listener != Listener::Active;
    if cfg!(target_os = "macos") && waiting {
      proxy().after(ACCESS_POLL, |_| _ = with_app(App::poll_access));
    }

    if !cfg!(target_os = "macos") {
      self.watch_listener();
    }
  }

  /// Off macOS the listener starts once, in the background (Windows' hook, the portal's answer):
  /// only watched until it settles, so the tray tells how it went.
  fn watch_listener(&mut self) {
    match self.hotkeys.status().listener {
      Listener::Starting => proxy().after(ACCESS_POLL, |_| _ = with_app(App::watch_listener)),
      _ => self.refresh(),
    }
  }

  /// The menu's Allow the Hotkey…: the listener starts, and the desktop's portal asks the person.
  fn allow_portal(&mut self) {
    self.may_listen = true;
    self.check_access();
    self.refresh();
  }

  fn poll_access(&mut self) {
    let (had, tap) = (self.accessibility, self.hotkeys.status().listener);
    self.check_access();
    if self.accessibility != had || self.hotkeys.status().listener != tap {
      self.refresh();
    }
  }

  fn mic(&self) -> Access {
    if self.fake_mic { Access::Granted } else { self.microphone }
  }

  /// A recording starts: the system's prompt, once per run, while it was never answered.
  fn ask_microphone(&mut self) {
    self.microphone = permissions::microphone();
    if self.mic() == Access::NotDetermined && !self.asked_microphone {
      self.asked_microphone = true;
      request_microphone();
    }
  }

  /// Denied during the recording that asked: the HUD says so, as for a press without access.
  fn microphone_answered(&mut self, main: &mut Main) {
    self.microphone = permissions::microphone();
    self.refresh();
    let recording = self.update.clone().filter(|u| u.status.state == Phase::Recording);
    if let Some(update) =
      recording.filter(|_| matches!(self.mic(), Access::Denied | Access::Restricted))
    {
      self.on_update(main, update);
    }
  }

  /// What the system does not tell the app is looked at again: the microphones, the models and
  /// the history.
  fn menu_opening(&mut self) {
    self.microphone = permissions::microphone();
    self.devices = audio::devices();
    self.lid_closed = audio::lid_closed();
    self.clipboard = SystemClipboard.readable();
    self.read_models();
    self.read_recent();
    self.refresh();
  }

  /// The session looks again when they changed.
  fn read_models(&mut self) {
    let found = window::ready(self.hub.downloaded(), |name| self.downloads.get(name));
    if found != self.found {
      self.found = found;
      self.handle.check_model();
    }

    self.repick();
  }

  fn read_recent(&mut self) {
    let Some(history) = &self.history else { return };
    let entries = history.db.get_page("", menu::RECENT);
    self.recent =
      entries.inspect_err(|e| warn!("the recent dictations: {e:#}")).unwrap_or_default();
  }

  /// False if it has no text.
  fn copy_recent(&mut self, i: usize) -> bool {
    let Some(e) = self.recent.get(i).filter(|e| !e.text.is_empty()) else { return false };
    SystemClipboard.write(&e.text);
    true
  }

  /// The menu's microphone `i`, as [App::refresh] listed them.
  fn choose_mic(&mut self, i: usize) {
    let saved = self.update.as_ref().map(|u| u.settings.microphone.clone()).unwrap_or_default();
    let (mics, _) = window::microphones(&self.devices, &saved, self.lid_closed);
    if let Some((name, choice)) = mics.into_iter().nth(i) {
      info!("microphone {name} chosen in the menu");
      self.handle.edit_settings(move |s| s.microphone = choice);
    }
  }

  fn repick(&mut self) {
    let query = self.update.as_ref().and_then(|u| u.settings.model.as_deref());
    self.pick = Pick::new(&self.hub, query, &self.found);
  }

  fn warnings(&self, update: &Update) -> Vec<Warning> {
    let hotkeys = self.hotkeys.status();
    // A listener that failed with nothing else holding the hotkey leaves it dead (Windows' hook,
    // X11's grab, the portal missing or refused): why shows, and with the portal Allow the
    // Hotkey… again.
    let listening = matches!(hotkeys.listener, Listener::Starting | Listener::Active);
    let dead = hotkeys.listener == Listener::Failed && !hotkeys.fallback;
    let system = menu::System {
      accessibility: self.accessibility,
      hotkey_error: hotkeys.error.or(hotkeys.listener_error.filter(|_| dead)),
      hotkey_portal: nimbo::hotkey::start_asks() && !listening,
      paste_portal: focus::hidden() && !nimbo::keys::ready(),
      no_microphone: self.mic() != Access::Granted,
      clipboard_unread: update.settings.restores_clipboard() && !self.clipboard,
      secure_input: hotkeys.secure_input,
    };
    menu::warnings(&system, &update.settings.hotkey, &update.status)
  }

  /// The icon and the menu.
  fn refresh(&mut self) {
    let Some(update) = &self.update else { return };
    // The portal took the hotkey: later launches bind it without a click.
    if nimbo::hotkey::start_asks()
      && !update.settings.hotkey_portal
      && self.hotkeys.status().listener == Listener::Active
    {
      self.handle.edit_settings(|s| s.hotkey_portal = true);
    }

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
    let mics = (&self.devices[..], &update.settings.microphone, self.lid_closed);
    let (status, found, pick) = (&update.status, &self.found, &self.pick);
    let updating = self.updater.lock().unwrap().showing().cloned();
    let models = (&found[..], pick);
    let menu =
      menu::menu(hotkey.as_deref(), status, &warnings, &recent, mics, models, updating.as_ref());
    if menu != self.menu {
      self.tray.set_menu(&menu);
      self.menu = menu;
    }
  }
}

// MARK: Updates

/// The menu's items: the update's window ([UpdateWindow]), brought to the front if open, with a
/// check when there is nothing else to show.
fn open_update(main: &mut Main) {
  let Some((start, open, updater)) = with_app(|app| {
    let start = app.updater.lock().unwrap().open();
    let open = app.update_window.clone().filter(Window::is_open);
    (start, open, app.updater.clone())
  }) else {
    return;
  };

  match open {
    Some(w) => w.front(),
    None => {
      let w = main.open(&update_config(), UpdateWindow { updater });
      w.on_close(|_| {
        with_app(|app| {
          app.update_window = None;
          app.updater.lock().unwrap().closed();
          app.update_changed();
        });
      });
      with_app(|app| app.update_window = Some(w));
    }
  }

  with_app(|app| {
    if start {
      App::spawn_check();
    }

    app.update_changed();
  });
}

/// The update's window: a fixed size, the app's icon in its title bar off macOS.
fn update_config() -> nimbo::Config {
  crate::with_icon(dialog::config("Software Update"))
}

/// The update's window: nimbo's [dialog::view] over the app's [Updater]; what the person chooses
/// goes to the app.
struct UpdateWindow {
  updater: Arc<Mutex<Updater>>,
}

impl nimbo::App for UpdateWindow {
  fn view(&mut self, ui: &mut nimbo::Ui) {
    let state = self.updater.lock().unwrap().state.clone();
    let about = About { name: "Wavo", version: crate::VERSION, icon: Some(Png::new(crate::ICON)) };
    let Some(act) = dialog::view(ui, &about, &state) else { return };
    proxy().post(move |_| _ = with_app(|app| app.update_act(act)));
  }
}

impl App {
  /// At launch and every [dialog::TICK]: an automatic check when one is due and they are on.
  fn update_tick(&mut self) {
    let due = self.updater.lock().unwrap().due(SystemTime::now());
    if self.settings().auto_update && due && self.updater.lock().unwrap().check(false) {
      App::spawn_check();
    }

    proxy().after(dialog::TICK, |_| _ = with_app(App::update_tick));
  }

  /// The check, on a thread, its answer to [App::update_checked].
  fn spawn_check() {
    spawn("update-check", || {
      let found = match updater::fake() {
        Some(version) => {
          std::thread::sleep(Duration::from_secs(1));
          updater::fake_check(&version)
        }
        None => update::check(&updater::FEED, crate::VERSION),
      };
      proxy().post(|_| _ = with_app(|app| app.update_checked(found)));
    });
  }

  fn update_checked(&mut self, found: io::Result<Option<Release>>) {
    match &found {
      Ok(Some(r)) => info!("update: {} is out", r.version),
      Ok(None) => info!("update: none"),
      Err(e) if e.kind() == io::ErrorKind::Unsupported => info!("update: {e}"),
      Err(e) => warn!("update check: {e}"),
    }

    let skipped = self.settings().skipped_update;
    self.updater.lock().unwrap().checked(found, skipped.as_deref(), SystemTime::now());
    self.update_changed();
  }

  /// From the update's window.
  fn update_act(&mut self, act: Act) {
    let mut updater = self.updater.lock().unwrap();
    let mut close = false;
    match act {
      Act::Install | Act::Retry => {
        if let Some(release) = updater.install() {
          info!("update: downloading {}", release.version);
          spawn("update", move || {
            // Told to the main thread only when the whole percent changes.
            let mut told = 0;
            let progress = |done, total| {
              let done = dialog::percent(done, total);
              if done != told {
                told = done;
                proxy().post(move |_| _ = with_app(|app| app.update_progress(done)));
              }
            };
            if updater::fake().is_some() {
              updater::fake_download(progress);
              return proxy().post(|_| _ = with_app(App::update_fake_downloaded));
            }

            let staged = update::download(&release, progress);
            proxy().post(|_| _ = with_app(|app| app.update_downloaded(staged)));
          });
        } else if updater.check(true) {
          // Retry after a check that failed.
          App::spawn_check();
        }
      }
      Act::Later => {
        updater.later();
        close = true;
      }
      Act::Skip => {
        if let Some(version) = updater.skip() {
          info!("update: {version} skipped");
          self.handle.edit_settings(move |s| s.skipped_update = Some(version));
        }

        close = true;
      }
      Act::Notes => {
        if let State::Available(r) | State::Failed(Some(r), _) = &updater.state
          && let Err(e) = nimbo::open::url(&updater::notes_url(&r.version))
        {
          warn!("the release notes: {e}");
        }
      }
      Act::Close => close = true,
    }

    drop(updater);
    if close {
      proxy().post(|_| {
        if let Some(w) = with_app(|app| app.update_window.clone()).flatten() {
          w.close();
        }
      });
    }

    self.update_changed();
  }

  fn update_progress(&mut self, done: u8) {
    self.updater.lock().unwrap().progress(done);
    self.update_changed();
  }

  fn update_downloaded(&mut self, staged: io::Result<Staged>) {
    match staged {
      Ok(staged) => {
        self.updater.lock().unwrap().downloaded();
        self.staged = Some(staged);
        self.install_staged();
      }
      Err(e) => {
        warn!("update download: {e}");
        self.updater.lock().unwrap().failed(&e);
      }
    }

    self.update_changed();
  }

  /// [updater::fake]'s download done: Restarting… for 3 s, then its install fails.
  fn update_fake_downloaded(&mut self) {
    self.updater.lock().unwrap().downloaded();
    self.update_changed();
    proxy().after(Duration::from_secs(3), |_| {
      _ = with_app(|app| {
        app.updater.lock().unwrap().failed(&io::Error::other(updater::FAKE_INSTALL));
        app.update_changed();
      })
    });
  }

  /// The update downloaded goes in place once no dictation runs, and none starts meanwhile.
  fn install_staged(&mut self) {
    let Some(staged) = self.staged.take_if(|_| self.update.as_ref().is_none_or(idle)) else {
      return;
    };

    self.installing = true;
    self.pause_hotkeys();
    spawn("update", || {
      let installed = update::install(staged);
      proxy().post(|_| _ = with_app(|app| app.update_installed(installed)));
    });
  }

  /// Its installer runs: it waits for this app to quit, puts the new one in and starts it. Else
  /// why not, and dictations start again.
  fn update_installed(&mut self, installed: io::Result<()>) {
    if let Err(e) = installed {
      warn!("update install: {e}");
      self.installing = false;
      self.pause_hotkeys();
      self.updater.lock().unwrap().failed(&e);
      return self.update_changed();
    }

    info!("update: the installer runs, quitting once no dictation runs");
    self.quitting = true;
    self.quit_when_idle();
    proxy().after(QUIT_WAIT, |main| main.quit());
  }

  /// A dictation that started as the update went in (its key was on its way) ends first.
  fn quit_when_idle(&self) {
    if self.quitting && self.update.as_ref().is_none_or(idle) {
      proxy().post(|main| main.quit());
    }
  }

  /// The hotkey paused while the update goes in or a shortcut is recorded, and only then.
  fn pause_hotkeys(&mut self) {
    self.hotkeys.pause(self.installing || self.recording_shortcut);
  }

  /// The menu's items, and the update's window if it shows.
  fn update_changed(&mut self) {
    self.refresh();
    proxy().post(|_| {
      if let Some(w) = with_app(|app| app.update_window.clone()).flatten() {
        w.redraw();
      }
    });
  }

  /// As the session last told them, else as saved (before its first update).
  fn settings(&self) -> Settings {
    match &self.update {
      Some(u) => u.settings.clone(),
      None => Settings::load(&self.home.join(settings::FILE)).0,
    }
  }
}

// MARK: Shell

/// The settings window's way to the app. What may wait (the system's login items, a deletion, an
/// export) runs on a thread of its own: the window keeps drawing. A download's, the export's, the
/// import's and the update's state are the app's: a window opened again shows them.
struct AppShell {
  home: PathBuf,
  handle: Handle,
  hub: Hub,
  downloads: Downloads,
  export: Arc<Mutex<Option<Export>>>,
  import: Arc<Mutex<Option<Import>>>,
  updater: Arc<Mutex<Updater>>,
}

impl window::Shell for AppShell {
  fn edit(&self, edit: Box<dyn FnOnce(&mut Settings) + Send>) {
    self.handle.edit_settings(edit);
  }

  fn pause_hotkey(&self, paused: bool) {
    proxy().post(move |_| {
      with_app(|app| {
        app.recording_shortcut = paused;
        app.pause_hotkeys();
      });
    });
  }

  fn devices(&self) -> Vec<audio::Device> {
    audio::devices()
  }

  fn lid_closed(&self) -> bool {
    audio::lid_closed()
  }

  fn download(&self, model: &'static ModelInfo) {
    // A model for the Neural Engine is prepared for it right after the download, unless it is off.
    let ane = Settings::load(&self.home.join(settings::FILE)).0.neural_engine;
    self.downloads.pull(self.hub.clone().prepare(ane), model, |ended| {
      if !ended {
        return redraw();
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

  /// `Hub::remove` waits while another process downloads it. A choice of it goes back to the first
  /// model left.
  fn delete_model(&self, model: &'static ModelInfo) {
    let (handle, hub) = (self.handle.clone(), self.hub.clone());
    spawn("model-delete", move || {
      match hub.remove(model.name) {
        Ok(freed) => {
          info!("deleted {} ({freed:?} bytes)", model.name);
          let chosen = |q: &str| hub::lookup(q).is_ok_and(|m| m.name == model.name);
          handle.edit_settings(move |s| s.model = s.model.take().filter(|q| !chosen(q)));
          handle.check_model();
        }
        Err(e) => warn!("delete {}: {e}", model.name),
      }

      with_window(Prefs::models_changed);
    });
  }

  fn history(&self, req: Request) {
    let handle = self.handle.clone();
    spawn("history", move || {
      if let Err(e) = handle.request(req) {
        warn!("{req:?}: {e}");
        with_window(move |prefs| prefs.history_failed(req, &e));
      }
    });
  }

  fn autostart(&self) -> Result<autostart::State, String> {
    autostart::state(&INFO)
  }

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
      microphone: permissions::microphone(),
      secure_input: permissions::secure_input(),
      clipboard_unread: !SystemClipboard.readable(),
    }
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

  fn has_neural_engine(&self) -> bool {
    crate::engine::has_neural_engine()
  }

  /// Into the archive the save panel picked.
  fn export(&self, file: PathBuf) {
    if !begin(&self.export) {
      return;
    }

    let (home, state) = (self.home.clone(), self.export.clone());
    spawn("export", move || {
      let _awake = nimbo::activity::begin("Exporting the history");
      user_initiated();
      let started = Instant::now();
      let result = transfer::export(&home, &file, progress(&state));
      let end = match result {
        Ok(done) => {
          info!("exported {}: {}", file.display(), rate(done.bytes, started));
          Transfer::Done((file, done.bytes))
        }
        Err(e) => {
          warn!("export to {}: {e:#}", file.display());
          Transfer::Failed(format!("{e:#}"))
        }
      };
      *state.lock().unwrap() = Some(end);
      with_window(|_| ());
    });
  }

  fn export_state(&self) -> Option<Export> {
    self.export.lock().unwrap().clone()
  }

  /// Beside the running app, which holds the history's lock; the session hears of it after, so
  /// the history window and the menu show the new dictations.
  fn import(&self, file: PathBuf) {
    if !begin(&self.import) {
      return;
    }

    let (home, handle, state) = (self.home.clone(), self.handle.clone(), self.import.clone());
    spawn("import", move || {
      let _awake = nimbo::activity::begin("Importing a history");
      user_initiated();
      let started = Instant::now();
      let end = match transfer::import(&home, &file, progress(&state)) {
        Ok(done) => {
          let bytes = std::fs::metadata(&file).map_or(0, |m| m.len());
          info!("imported {}: {done:?}, {}", file.display(), rate(bytes, started));
          Transfer::Done(done)
        }
        Err(e) => {
          warn!("import {}: {e:#}", file.display());
          Transfer::Failed(format!("{e:#}"))
        }
      };
      *state.lock().unwrap() = Some(end);
      if let Err(e) = handle.request(Request::Imported) {
        warn!("import: {e}");
      }

      with_window(|_| ());
    });
  }

  fn import_state(&self) -> Option<Import> {
    self.import.lock().unwrap().clone()
  }

  fn updates(&self) -> bool {
    self.updater.lock().unwrap().showing().is_some()
  }

  fn check_updates(&self) {
    proxy().post(open_update);
  }
}

/// The calling thread runs as the person's own action, not as background work: macOS gives it
/// the disk and the cores a background thread waits for. The process's side, no App Nap while
/// Wavo is out of view, is nimbo's activity the transfer holds. Other systems don't tell them
/// apart.
fn user_initiated() {
  // SAFETY: it sets the calling thread's own class.
  #[cfg(target_os = "macos")]
  unsafe {
    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0)
  };
}

/// `16.0 GB in 52.3 s, 306 MB/s` since `started`.
fn rate(bytes: u64, started: Instant) -> String {
  let secs = started.elapsed().as_secs_f64();
  let mbs = bytes as f64 / 1e6 / secs.max(0.001);
  format!("{} in {secs:.1} s, {mbs:.0} MB/s", window::size_text(bytes))
}

/// Marks a transfer running, unless one is: one at a time.
fn begin<T>(state: &Mutex<Option<Transfer<T>>>) -> bool {
  let mut state = state.lock().unwrap();
  if matches!(*state, Some(Transfer::Running { .. })) {
    return false;
  }

  *state = Some(Transfer::Running { done: 0, total: 0 });
  true
}

/// A transfer's progress into `state`, redrawing the window at most every 100 ms.
fn progress<T>(state: &Mutex<Option<Transfer<T>>>) -> impl FnMut(u64, u64) {
  let mut told = None::<Instant>;
  move |done, total| {
    *state.lock().unwrap() = Some(Transfer::Running { done, total });
    if told.is_none_or(|t: Instant| t.elapsed() >= Duration::from_millis(100)) {
      told = Some(Instant::now());
      redraw();
    }
  }
}

// MARK: Paste

struct GuiOutput;

impl TextOutput for GuiOutput {
  type Target = Option<Focused>;

  fn target(&mut self) -> Option<Focused> {
    focus::current()
  }

  /// nimbo's paste, on the main thread (see [nimbo::paste]): refused when the focus moved from
  /// `target` or sits in a password field; `keep` leaves the text on the clipboard.
  fn paste(&mut self, text: &str, target: &Option<Focused>, keep: bool) -> Result<(), NotPasted> {
    // Given up after MAIN_WAIT (PORTAL_WAIT on Wayland): when the main thread gets to it, it is not
    // pasted any more, and nimbo's paste stops just before then, as a clipboard or a focus that
    // doesn't answer holds it (no ⌘V a minute late).
    let wait = if focus::hidden() { PORTAL_WAIT } else { MAIN_WAIT };
    let deadline = Some(Instant::now() + wait - Duration::from_millis(100));
    let opts = Paste { restore: !keep, target: target.clone(), deadline };
    let (text, late) = (text.to_owned(), Arc::new(AtomicBool::new(false)));
    let gave_up = late.clone();
    let paste = move |_: &mut Main| {
      if gave_up.load(Ordering::Acquire) {
        return Err(NotPasted::Failed("too late".into()));
      }

      paste::text(&text, opts).map_err(not_pasted)?;
      info!("paste: {} chars", text.chars().count());
      Ok(())
    };
    proxy().call(paste, wait).unwrap_or_else(|| {
      late.store(true, Ordering::Release);
      Err(busy())
    })
  }

  fn copy(&mut self, text: &str) -> Result<()> {
    let (text, late) = (text.to_owned(), Arc::new(AtomicBool::new(false)));
    let gave_up = late.clone();
    let copy = move |_: &mut Main| {
      if !gave_up.load(Ordering::Acquire) {
        SystemClipboard.write(&text);
      }
    };
    proxy().call(copy, MAIN_WAIT).ok_or_else(|| {
      late.store(true, Ordering::Release);
      anyhow::anyhow!("the app was too busy")
    })
  }
}

/// nimbo's reason as the session keeps it: the moved focus and a password field have their own.
fn not_pasted(e: paste::NotPasted) -> NotPasted {
  match e {
    paste::NotPasted::FocusMoved { from, to } => {
      info!("not pasted: the focus moved from {from} to {to}");
      NotPasted::FocusChanged
    }
    paste::NotPasted::SecureField => NotPasted::SecureField,
    paste::NotPasted::Late => busy(),
    e => NotPasted::Failed(e.to_string()),
  }
}

/// The main thread didn't get to the paste in time, or the paste ran out of it.
fn busy() -> NotPasted {
  NotPasted::Failed("the app was too busy to paste".into())
}
