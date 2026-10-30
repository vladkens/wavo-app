// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar icon and menu: a header with the app, the hotkey and the state; the warnings, most
//! opening where they are fixed; an update found or going on; Copy Last Dictation and Recent; the
//! model in use with the others downloaded; Unload Model, History…, Settings…, Check for Updates…,
//! Quit.

use nimbo::Hotkey;
use nimbo::tray::{Item, LABEL, Menu, TrayIcon};
use nimbo::update::dialog::{self, State};

use crate::audio::{Auto, Device, Microphone};
use crate::engine::Phase as EnginePhase;
use crate::history::{local_time, shorten};
use crate::session::{NoModel, Outcome, Phase, Status};
use crate::window::{
  Found, Pick, bluetooth_only, mic_name, microphones as mic_choices, not_connected,
};

pub const RECENT: usize = 10;
/// The update's row under the warnings while it has news, and Check for Updates… above Quit: both
/// open the update's window.
pub const UPDATE: &str = "update";
pub const CHECK_UPDATES: &str = "update-check";

// MARK: Icon

/// The app icon's wave-"w" (`assets/status-icon/`); a state cuts a disc with a letter out of its
/// right arm: R recording, T transcribing, ! needs attention. A template: the OS draws it at every
/// scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
  Idle,
  Recording,
  Transcribing,
  Warning,
}

impl Icon {
  pub fn tray(self) -> TrayIcon {
    let svg = match self {
      Icon::Idle => include_str!("../assets/status-icon/idle.svg"),
      Icon::Recording => include_str!("../assets/status-icon/recording.svg"),
      Icon::Transcribing => include_str!("../assets/status-icon/transcribing.svg"),
      Icon::Warning => include_str!("../assets/status-icon/warning.svg"),
    };
    TrayIcon::from_svg(svg).expect("the status icons are tested")
  }

  /// The tooltip, also what accessibility says.
  pub fn description(self) -> &'static str {
    match self {
      Icon::Idle => "Wavo",
      Icon::Recording => "Wavo — recording",
      Icon::Transcribing => "Wavo — transcribing",
      Icon::Warning => "Wavo — needs attention",
    }
  }
}

/// The phase, or a warning when idle with any.
pub fn icon(status: &Status, warnings: &[Warning]) -> Icon {
  match status.state {
    Phase::Recording => Icon::Recording,
    Phase::Transcribing => Icon::Transcribing,
    Phase::Idle if warnings.is_empty() => Icon::Idle,
    Phase::Idle => Icon::Warning,
  }
}

// MARK: Warnings

/// What the system lets the app do, as the app last looked.
#[derive(Debug, Clone, Default)]
pub struct System {
  pub accessibility: bool,
  /// The hotkey does not parse, or the OS refused it.
  pub hotkey_error: Option<String>,
  /// The hotkey waits for the person to let the desktop's portal ask them (Wayland).
  pub hotkey_portal: bool,
  /// The paste waits for the person to let the desktop's RemoteDesktop portal ask them (Wayland).
  pub paste_portal: bool,
  pub no_microphone: bool,
  /// The paste restores the clipboard, which the app may not read (Paste from Other Apps).
  pub clipboard_unread: bool,
  pub secure_input: bool,
}

/// `action`: a click opens where it is fixed.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
  pub id: &'static str,
  pub text: String,
  pub action: bool,
}

pub fn warnings(system: &System, hotkey: &str, status: &Status) -> Vec<Warning> {
  let mut list = Vec::new();
  let mut add = |id, text: String, action| list.push(Warning { id, text, action });
  if !system.accessibility {
    add("warn:accessibility", "Allow Accessibility…".into(), true);
  }

  if let Some(e) = &system.hotkey_error {
    // As the platform names its keys (Ctrl+Alt+K, not the stored ctrl+option+k); a click opens
    // General, where it changes.
    let label = Hotkey::parse(hotkey).map_or_else(|_| hotkey.to_owned(), |h| h.label());
    add("warn:hotkey", format!("The hotkey “{label}” does not work: {e}"), true);
  }

  if system.hotkey_portal {
    add("warn:hotkey-portal", "Allow the Hotkey…".into(), true);
  }

  if system.paste_portal {
    add("warn:paste-portal", "Allow Paste…".into(), true);
  }

  if system.no_microphone {
    add("warn:microphone", "Allow Microphone…".into(), true);
  }

  if system.clipboard_unread {
    add("warn:clipboard", "Allow Paste…".into(), true);
  }

  if system.secure_input {
    add("warn:secure-input", "Secure Input is on…".into(), true);
  }

  match &status.no_model {
    Some(NoModel::NoneDownloaded) => add("warn:download", "Download a Speech Model…".into(), true),
    Some(NoModel::Gone { model }) => add("warn:model", format!("{model} is not on disk…"), true),
    Some(NoModel::LoadFailed { model, .. }) => {
      add("warn:model", format!("{model} does not load…"), true)
    }
    None => {}
  }

  let failed = status.last.as_ref().filter(|l| l.outcome == Outcome::Failed);
  let failed = failed.map(|l| l.error.as_deref().unwrap_or("unknown error"));
  let failed = failed.map(|e| format!("The last dictation failed: {e}"));
  let error = status.error.as_ref().map(|e| format!("Error: {e}"));
  for text in error.into_iter().chain(failed).chain(status.warnings.iter().map(|w| capitalized(w)))
  {
    add("warn:status", text, false);
  }

  list
}

fn capitalized(text: &str) -> String {
  let mut chars = text.chars();
  chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

// MARK: Menu

/// A dictation in Recent: the time today, else the day, and its text on one line, shortened.
pub fn recent_title(text: &str, created_at: i64, now: i64) -> String {
  let (Some(t), Some(today)) = (local_time(created_at), local_time(now)) else {
    return shorten(text, LABEL);
  };
  let when = match t.date() == today.date() {
    true => t.hm(),
    false => format!("{} {}", &t.month_name()[..3], t.day),
  };
  // Within the menu's width with its time, so the item isn't cut a second time.
  let preview = shorten(text, LABEL - when.chars().count() - 3);
  format!("{when} · {preview}")
}

/// Item ids: `recent:<i>`, `mic:<i>`, `model:<name>`, `warn:…` and fixed ones, as
/// `app::chosen` handles them. `mics`: the devices, the one saved and whether the lid is closed;
/// `models`: those downloaded and the pick; `update`: the updater's state, `None` in a build that
/// doesn't update.
pub fn menu(
  hotkey: Option<&str>,
  status: &Status,
  warnings: &[Warning],
  recent: &[String],
  mics: (&[Device], &Microphone, bool),
  (found, pick): (&[Found], &Pick),
  update: Option<&State>,
) -> Menu {
  let title = format!("Wavo {}", crate::VERSION);
  let header = [Some(title.as_str()), hotkey, state(status)].into_iter().flatten();
  let header = header.collect::<Vec<_>>().join(" · ");
  // Labels fit the menu's width by themselves (nimbo's Item::new).
  let mut items = vec![Item::new("header", &header).enabled(false)];
  for w in warnings {
    // Downloading a first model is a step of the setup, not a problem.
    let mark = if w.id == "warn:download" { "" } else { "⚠︎ " };
    items.push(Item::new(w.id, &format!("{mark}{}", w.text)).enabled(w.action));
  }

  items.extend(update.and_then(|s| dialog::menu_item(UPDATE, s)));

  let last = status.last.as_ref().is_some_and(|l| !l.text.is_empty());
  let loaded = status.engine.phase != EnginePhase::Stopped && status.state == Phase::Idle;
  let recent: Vec<Item> =
    recent.iter().enumerate().map(|(i, title)| Item::new(&format!("recent:{i}"), title)).collect();
  let (mic, mics, note) = microphones(mics);
  let (model, models) = models(found, pick);
  items.extend([
    Item::separator(),
    Item::new("copy-last", "Copy Last Dictation").enabled(last),
    Item::new("recent", "Recent").enabled(!recent.is_empty()).submenu(Menu::new(recent)),
    Item::separator(),
    Item::new("mic", &mic).submenu(mics),
  ]);
  if let Some(note) = note {
    items.push(Item::new("mic-note", &note).enabled(false));
  }

  items.extend([
    Item::new("model", &model).submenu(models),
    Item::new("unload", "Unload Model").enabled(loaded),
    Item::separator(),
    Item::new("history", "History…"),
    // A tray menu's keys work only in macOS's; Windows' and Linux's would show keys that do
    // nothing.
    with_key(Item::new("settings", "Settings…"), "cmd+comma"),
  ]);
  items.extend(update.map(|s| dialog::check_item(CHECK_UPDATES, s)));
  items.extend([Item::separator(), with_key(Item::new("quit", "Quit Wavo"), "cmd+q")]);
  Menu::new(items)
}

fn with_key(item: Item, key: &str) -> Item {
  if cfg!(target_os = "macos") { item.key(key) } else { item }
}

/// The microphone in use, the submenu (Automatic and the system default, then the devices, the one
/// chosen checked) and a note under it: the one chosen isn't connected (the one in use is the
/// system's input), or Automatic has only a Bluetooth one.
fn microphones(
  (devices, saved, lid_closed): (&[Device], &Microphone, bool),
) -> (String, Menu, Option<String>) {
  let (mics, at) = mic_choices(devices, saved, lid_closed);
  let mut items: Vec<Item> = (mics.iter().enumerate())
    .map(|(i, (label, _))| Item::new(&format!("mic:{i}"), label).checked(i == at))
    .collect();
  items.insert(2, Item::separator());
  let menu = Menu::new(items);
  let Some((name, instead)) = not_connected(devices, saved, lid_closed) else {
    let note = bluetooth_only(devices, saved, lid_closed).map(str::to_owned);
    return (mics[at].0.clone(), menu, note);
  };

  let title = match instead {
    Auto::Device(i) => mic_name(&devices[i], lid_closed),
    Auto::System => "System Default".to_owned(),
    Auto::Nothing => "No Microphone".to_owned(),
  };
  (title, menu, Some(format!("{name} isn’t connected")))
}

/// The model in use, and the submenu: the models downloaded with the one that transcribes checked,
/// then the chosen one when it is none of them.
fn models(found: &[Found], pick: &Pick) -> (String, Menu) {
  let only = pick.other.is_none();
  let mut items: Vec<Item> = (found.iter().enumerate())
    .map(|(i, (m, _))| {
      Item::new(&format!("model:{}", m.name), m.full_name).checked(only && pick.active == Some(i))
    })
    .collect();
  let other = pick.other.as_ref().map(|(name, there)| match there {
    true => name.clone(),
    false => format!("{name} (not on disk)"),
  });
  let title = match (&other, pick.active) {
    (Some(other), _) => other.clone(),
    (None, Some(i)) => found[i].0.full_name.to_owned(),
    (None, None) => "No Model".to_owned(),
  };
  if let Some(other) = other {
    items.push(Item::new("model-chosen", &other).checked(pick.gone().is_none()));
  }

  if items.is_empty() {
    items.push(Item::new("model-none", "No Models Downloaded").enabled(false));
  }

  items.extend([Item::separator(), Item::new("models", "All Models…")]);
  (title, Menu::new(items))
}

/// What the app does now, unless it is idle.
fn state(status: &Status) -> Option<&'static str> {
  let loading = status.engine.phase == EnginePhase::Loading;
  match status.state {
    Phase::Recording => Some("Recording…"),
    Phase::Transcribing if loading => Some("Loading the model…"),
    Phase::Transcribing => Some("Transcribing…"),
    Phase::Idle if loading => Some("Loading the model…"),
    Phase::Idle if status.error.is_some() => Some("Error"),
    Phase::Idle => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::audio::{Choice, Transport};

  #[test]
  fn every_icon_is_an_18_point_even_odd_path() {
    for icon in [Icon::Idle, Icon::Recording, Icon::Transcribing, Icon::Warning] {
      let TrayIcon::Path { size, d, even_odd } = icon.tray() else { unreachable!() };
      assert!(size == 18.0 && even_odd && !d.is_empty(), "{icon:?}");
    }
  }

  #[test]
  fn warnings_open_what_fixes_them_and_turn_the_icon() {
    let mut s = Status::default();
    let fine = System { accessibility: true, ..System::default() };
    assert_eq!((warnings(&fine, "f5", &s), icon(&s, &[])), (vec![], Icon::Idle));

    let system = System {
      hotkey_error: Some("taken".into()),
      hotkey_portal: true,
      paste_portal: true,
      no_microphone: true,
      clipboard_unread: true,
      secure_input: true,
      ..System::default()
    };
    s.no_model = Some(NoModel::NoneDownloaded);
    s.warnings = vec!["no sound from USB Mic".into()];
    let list = warnings(&system, "f5", &s);
    let ids: Vec<_> = list.iter().map(|w| (w.id, w.action)).collect();
    let expected = [
      ("warn:accessibility", true),
      ("warn:hotkey", true),
      ("warn:hotkey-portal", true),
      ("warn:paste-portal", true),
      ("warn:microphone", true),
      ("warn:clipboard", true),
      ("warn:secure-input", true),
      ("warn:download", true),
      ("warn:status", false),
    ];
    assert_eq!(ids, expected);
    assert_eq!(list[8].text, "No sound from USB Mic");
    assert_eq!(icon(&s, &list), Icon::Warning);
    s.state = Phase::Recording;
    assert_eq!(icon(&s, &list), Icon::Recording, "the phase first");

    let device = |name: &str, default, transport| {
      let (id, name) = (Some(name.into()), name.into());
      Device { id, name, default, transport }
    };
    let devices = [
      device("MacBook Pro Microphone", true, Transport::BuiltIn),
      device("Elgato Wave:3 Microphone Array", false, Transport::Usb),
    ];
    let wave = Choice { id: devices[1].id.clone(), name: devices[1].name.clone() };
    let wave = Microphone::Device(wave);
    let mics = (&devices[..], &wave, false);
    let pick = Pick::default();
    let items = menu(Some("⌥Space"), &s, &list, &[], mics, (&[], &pick), None).items;
    // The header as long as the build's version lets it be (a dirty build's runs longer).
    let header = format!("Wavo {} · ⌥Space · Recording…", crate::VERSION);
    let label = &items[0].label;
    match header.chars().count() <= 48 {
      true => assert_eq!(*label, header),
      false => assert!(label.starts_with("Wavo v") && label.contains("⌥Space"), "{label}"),
    }
    assert_eq!(
      (items[1].label.as_str(), items[8].label.as_str()),
      ("⚠︎ Allow Accessibility…", "Download a Speech Model…")
    );
    // The hotkey's warning opens General too, its key as the platform names it.
    assert!(items[1].enabled && items[2].enabled);
    assert!(items[2].label.contains("“F5” does not work"), "{}", items[2].label);
    // One name in the menu and its submenu.
    let mic = items.iter().find(|i| i.id == "mic").unwrap();
    assert_eq!(mic.label, "Elgato Wave:3 Microphone Array");
    let sub = &mic.submenu.as_ref().unwrap().items;
    let got: Vec<_> = sub.iter().map(|i| (i.label.as_str(), i.checked)).collect();
    assert_eq!(got[0], ("Automatic (Elgato Wave:3 Microphone Array)", false));
    assert_eq!(got[1], ("System Default (MacBook Pro Microphone)", false));
    let devices_got = &got[3..];
    assert_eq!(devices_got, [("MacBook Pro Microphone", false), (devices[1].name.as_str(), true)]);

    let mic_label = |devices: &[Device], saved: &Microphone, lid_closed| {
      let items = menu(None, &s, &list, &[], (devices, saved, lid_closed), (&[], &pick), None);
      let label = |id| items.items.iter().find(|i| i.id == id).map(|i| i.label.clone());
      (label("mic").unwrap(), label("mic-note"))
    };
    let system = mic_label(&devices, &Microphone::System, false);
    assert_eq!(system, ("System Default (MacBook Pro Microphone)".into(), None));
    // Automatic: the USB one over the built-in default; the built-in one over a Bluetooth one, but
    // not under a closed lid, and a Bluetooth one left alone has a note; a virtual one only when
    // it is all there is, as the system's.
    let auto = Microphone::Automatic;
    assert_eq!(mic_label(&devices, &auto, false).0, "Automatic (Elgato Wave:3 Microphone Array)");
    let mac = device("MacBook Pro Microphone", true, Transport::BuiltIn);
    let both = [mac, device("AirPods Pro", false, Transport::Bluetooth)];
    assert_eq!(mic_label(&both, &auto, false).0, "Automatic (MacBook Pro Microphone)");
    let note = Some("Only a Bluetooth mic: the sound is worse".into());
    assert_eq!(mic_label(&both, &auto, true), ("Automatic (AirPods Pro)".into(), note.clone()));
    let airpods = [device("AirPods Pro", true, Transport::Bluetooth)];
    assert_eq!(mic_label(&airpods, &auto, false), ("Automatic (AirPods Pro)".into(), note));
    let lid = [device("MacBook Pro Microphone", true, Transport::BuiltIn)];
    assert_eq!(mic_label(&lid, &auto, true).0, "Automatic (none: the lid is closed)");
    let loopback = [device("BlackHole 2ch", true, Transport::Virtual)];
    assert_eq!(mic_label(&loopback, &auto, false), ("Automatic (BlackHole 2ch)".into(), None));
    assert!(!items.iter().any(|i| i.id.starts_with("update")), "a build that doesn't update");
  }

  /// A chosen microphone that isn't connected stays checked in the submenu; the menu names the one
  /// that records in its place, the system's input (not Automatic's pick), a note under it.
  #[test]
  fn a_mic_not_connected_names_the_one_that_records() {
    use Transport::*;

    let (s, pick) = (Status::default(), Pick::default());
    let mics_lid = |devices: &[Device], saved: &Microphone, lid_closed| {
      let mics = (devices, saved, lid_closed);
      let items = menu(None, &s, &[], &[], mics, (&[], &pick), None).items;
      let label = |id| items.iter().find(|i| i.id == id).map(|i| i.label.clone());
      let sub = &items.iter().find(|i| i.id == "mic").unwrap().submenu.as_ref().unwrap().items;
      let checked = sub.iter().find(|i| i.checked).map(|i| i.label.clone());
      (label("mic").unwrap(), label("mic-note"), checked)
    };
    let mics = |devices: &[Device], saved: &Microphone| mics_lid(devices, saved, false);
    let device = |name: &str, default, transport| {
      let (id, name) = (Some(name.into()), name.into());
      Device { id, name, default, transport }
    };
    let wh = device("WH-1000XM5", false, Bluetooth);
    let headphones = Microphone::Device(Choice { id: wh.id.clone(), name: wh.name.clone() });
    let note = Some("WH-1000XM5 isn’t connected".to_owned());
    let checked = Some("WH-1000XM5 (not connected)".to_owned());

    let rode =
      [device("MacBook Pro Microphone", false, BuiltIn), device("RØDE PodMic USB", true, Usb)];
    let want = ("RØDE PodMic USB".into(), note.clone(), checked.clone());
    assert_eq!(mics(&rode, &headphones), want);
    // The system's input, not Automatic's USB one.
    let mac =
      [device("MacBook Pro Microphone", true, BuiltIn), device("Elgato Wave:3", false, Usb)];
    assert_eq!(mics(&mac, &headphones).0, "MacBook Pro Microphone");
    // Under a closed lid that one hears nothing: the menu says so.
    let lid = "MacBook Pro Microphone (the lid is closed)".to_owned();
    assert_eq!(mics_lid(&mac, &headphones, true), (lid, note.clone(), checked.clone()));
    // The system's input named by no listed device (ALSA's), or none listed: Linux has ALSA's
    // `default` all the same, elsewhere there is no input at all.
    let alsa = [device("HDA Intel PCH", false, Unknown)];
    let want = ("System Default".into(), note.clone(), checked.clone());
    assert_eq!(mics(&alsa, &headphones), want);
    let none = if cfg!(target_os = "linux") { "System Default" } else { "No Microphone" };
    assert_eq!(mics(&[], &headphones), (none.into(), note, checked));
    // Back again: in use, no note.
    let back = [wh, device("RØDE PodMic USB", true, Usb)];
    let want = ("WH-1000XM5".into(), None, Some("WH-1000XM5".into()));
    assert_eq!(mics(&back, &headphones), want);
  }

  /// An update shows under the header and the warnings; Check for Updates… sits above Quit.
  #[test]
  fn an_update_shows_under_the_header() {
    let s = Status::default();
    let pick = Pick::default();
    let items = |state: State| {
      let mics = (&[][..], &Microphone::System, false);
      let items = menu(None, &s, &[], &[], mics, (&[], &pick), Some(&state)).items;
      items.into_iter().map(|i| (i.id, i.label)).collect::<Vec<_>>()
    };
    let idle = items(State::Idle);
    let ids: Vec<_> = idle[idle.len() - 4..].iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["settings", CHECK_UPDATES, "", "quit"]);
    assert_eq!(idle[idle.len() - 3].1, "Check for Updates…");
    assert!(!idle.iter().any(|(id, _)| id == UPDATE), "no news");
    let available = items(State::Available(nimbo::update::Release::test("1.2.0")));
    assert_eq!(available[1], (UPDATE.into(), "Update Available: v1.2.0…".into()));
  }
}
