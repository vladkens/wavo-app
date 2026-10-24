// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `wavo-app gui`: the menu bar app on nimbo's app platform ([nimbo::app]), an accessory app with
//! no window: the [Tray] with the [menu], the [Hotkeys] that feed the session's input, the HUD in
//! an [Overlay]. The session and the control socket are `wavo-app daemon`'s.
//!
//! The main thread owns the app ([App], in a thread local). Other threads reach it only through
//! nimbo's [Proxy] ([proxy]): the session's observer posts every [Update] there, which goes on to
//! the icon, the menu and the HUD. The main thread never waits for the session: what needs an
//! answer from it runs on a thread of its own.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use nimbo::hotkey::{Hotkeys, Listener};
use nimbo::permissions::{self, Pane};
use nimbo::tray::{Menu, Tray};
use nimbo::{AppInfo, Clipboard, Main, Presence, Proxy, SystemClipboard, Window, autostart};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSApplication};
use objc2_foundation::NSString;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::microphone::{self, Access as Microphone};
use super::output::MacOutput;
use super::paste;
use crate::audio::devices;
use crate::ctl::{self, Autostart, Claim, Reply, Request, Serving};
use crate::session::controller::{Handle, Phase, Update};
use crate::session::{self, Session};
use crate::settings::HudPosition;
use crate::ui::hud::{self, Mode};
use crate::ui::icons::Icon;
use crate::ui::menu::{self, Warning};
use crate::ui::overlay::{self, Overlay, Show};

/// The app as the system knows it: its menu bar presence, its login item.
const INFO: AppInfo =
  AppInfo { id: "me.vladkens.wavo", name: "Wavo", presence: Presence::Accessory };
/// How often Accessibility access is checked while it is missing (and only then).
const ACCESS_POLL: Duration = Duration::from_secs(2);
/// How long `wavo-app ctl status` waits for the main thread for the shell's part.
const STATUS_WAIT: Duration = Duration::from_secs(1);
/// Paste from Other Apps (macOS 15.4+), where the app may read the clipboard to restore it.
const PASTEBOARD_PANE: &str =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_Pasteboard";

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
  /// The session's last; none before its first.
  update: Option<Update>,
  accessibility: bool,
  microphone: Microphone,
  /// The paste may read the clipboard to restore it ([paste::readable]).
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
      alert("Wavo could not start", &format!("{e:#}"));
      started = Err(e);
      main.quit();
    }
  });
  started
}

/// A modal alert with one button: a failed start shows even without a terminal.
fn alert(title: &str, text: &str) {
  let mtm = MainThreadMarker::new().expect("the main thread");
  let alert = NSAlert::new(mtm);
  alert.setMessageText(&NSString::from_str(title));
  alert.setInformativeText(&NSString::from_str(text));
  NSApplication::sharedApplication(mtm).activate();
  alert.runModal();
}

fn start(main: &mut Main, config: session::Config, server: ctl::Server) -> Result<()> {
  let _ = PROXY.set(main.proxy());
  let fake_mic = config.fake_mic.is_some();
  let observer = Box::new(|update: Update| {
    proxy().post(move |main| _ = with_app(|app| app.on_update(main, update)))
  });
  let session = Session::start_observed(config, MacOutput, observer)?;
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
    update: None,
    accessibility: false,
    microphone: microphone::access(),
    clipboard: paste::readable(),
    asked_microphone: false,
    fake_mic,
    session,
    serving,
  };
  // No system dialog at launch: the menu's warning asks for Accessibility.
  app.check_access();
  APP.with(|cell| *cell.borrow_mut() = Some(app));
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
    Request::OpenSettings => Reply::error("there is no settings window yet"),
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
  match id {
    "copy-last" => _ = with_app(App::copy_last),
    "unload" => _ = with_app(App::unload),
    "quit" => main.quit(),
    // The system's dialog adds the app to the Accessibility list, which the pane alone does not.
    "warn:accessibility" => _ = permissions::accessibility(true),
    "warn:microphone" => opened(permissions::open_pane(Pane::Microphone)),
    "warn:clipboard" => opened(nimbo::open::url(PASTEBOARD_PANE)),
    _ => {}
  }
}

impl App {
  fn on_update(&mut self, main: &mut Main, update: Update) {
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
    self.update = Some(update);
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
    let unload = move || {
      if let Some(e) = handle.request(Request::Unload).error {
        warn!("unload the model: {e}");
      }
    };
    if let Err(e) = std::thread::Builder::new().name("unload".into()).spawn(unload) {
      warn!("unload the model: {e}");
    }
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
    self.microphone = microphone::access();
    if !self.fake_mic && self.microphone == Microphone::NotDetermined && !self.asked_microphone {
      self.asked_microphone = true;
      microphone::request();
    }
  }

  /// The menu opens: what the system does not tell the app is looked at again.
  fn menu_opening(&mut self) {
    self.microphone = microphone::access();
    self.clipboard = paste::readable();
    self.refresh();
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
      microphone_denied: matches!(self.microphone, Microphone::Denied | Microphone::Restricted),
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
    let menu = menu::menu(hotkey.as_deref(), &update.status, &warnings);
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
      "microphone": self.microphone,
      "hud": self.hud.0.with(|o| json!({ "mode": o.mode(), "text": o.text() })),
      "warnings": warnings.iter().map(|w| &w.text).collect::<Vec<_>>(),
    })
  }
}
