// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar icon and menu: a header with the app, the hotkey and the state; the warnings, most
//! opening where they are fixed; Copy Last Dictation and Recent; the model in use with the others
//! downloaded; Unload Model, History…, Settings…, Quit.

use nimbo::tray::{Item, Menu, TrayIcon};

use crate::audio::{Choice, Device};
use crate::engine::Phase as EnginePhase;
use crate::history::{local_time, shorten};
use crate::session::{NoModel, Outcome, Phase, Status};
use crate::window::{Found, Pick, microphones as mic_choices};

pub const RECENT: usize = 10;
const TITLE: &str = concat!("Wavo ", env!("CARGO_PKG_VERSION"));

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
    add("warn:hotkey", format!("The hotkey “{hotkey}” does not work: {e}"), false);
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
  let preview = shorten(text, 48);
  let (Some(t), Some(today)) = (local_time(created_at), local_time(now)) else { return preview };
  let when = match t.date() == today.date() {
    true => t.hm(),
    false => format!("{} {}", &t.month_name()[..3], t.day),
  };
  format!("{when} · {preview}")
}

/// Item ids: `recent:<i>`, `mic:<i>`, `model:<name>`, `warn:…` and fixed ones, as `app::chosen`
/// handles them. `mics`: the devices and the one saved.
pub fn menu(
  hotkey: Option<&str>,
  status: &Status,
  warnings: &[Warning],
  recent: &[String],
  mics: (&[Device], Option<&Choice>),
  found: &[Found],
  pick: &Pick,
) -> Menu {
  let header = [Some(TITLE), hotkey, state(status)].into_iter().flatten();
  let header = header.collect::<Vec<_>>().join(" · ");
  let mut items = vec![Item::new("header", &header).enabled(false)];
  for w in warnings {
    // Downloading a first model is a step of the setup, not a problem.
    let mark = if w.id == "warn:download" { "" } else { "⚠︎ " };
    items.push(Item::new(w.id, &format!("{mark}{}", w.text)).enabled(w.action));
  }

  let last = status.last.as_ref().is_some_and(|l| !l.text.is_empty());
  let loaded = status.engine.phase != EnginePhase::Stopped && status.state == Phase::Idle;
  let recent: Vec<Item> =
    recent.iter().enumerate().map(|(i, title)| Item::new(&format!("recent:{i}"), title)).collect();
  let (mic, mics) = microphones(mics);
  let (model, models) = models(found, pick);
  items.extend([
    Item::separator(),
    Item::new("copy-last", "Copy Last Dictation").enabled(last),
    Item::new("recent", "Recent").enabled(!recent.is_empty()).submenu(Menu::new(recent)),
    Item::separator(),
    Item::new("mic", &mic).submenu(mics),
    Item::new("model", &model).submenu(models),
    Item::new("unload", "Unload Model").enabled(loaded),
    Item::separator(),
    Item::new("history", "History…"),
    Item::new("settings", "Settings…").key("cmd+comma"),
    Item::separator(),
    Item::new("quit", "Quit Wavo").key("cmd+q"),
  ]);
  Menu::new(items)
}

/// The longest microphone or model in the menu.
const NAME: usize = 32;

/// A microphone or a model as the menu and its submenus name it: as Settings does, a longer one
/// shortened at a word, inside its brackets when it has some: `System Default (MacBook Pro…)`.
fn short(name: &str) -> String {
  if name.chars().count() <= NAME {
    return name.to_owned();
  }

  match name.strip_suffix(')').and_then(|n| n.split_once(" (")) {
    Some((head, inner)) if head.chars().count() + 12 <= NAME => {
      format!("{head} ({})", shorten(inner, NAME - head.chars().count() - 4))
    }
    _ => shorten(name, NAME - 1),
  }
}

/// The microphone in use, and the submenu: the system default, then the devices, the one in use
/// checked.
fn microphones((devices, saved): (&[Device], Option<&Choice>)) -> (String, Menu) {
  let (mics, at) = mic_choices(devices, saved);
  let mut items: Vec<Item> = (mics.iter().enumerate())
    .map(|(i, (label, _))| Item::new(&format!("mic:{i}"), &short(label)).checked(i == at))
    .collect();
  if items.len() > 1 {
    items.insert(1, Item::separator());
  }

  (short(&mics[at].0), Menu::new(items))
}

/// The model in use, and the submenu: the models downloaded with the one that transcribes checked,
/// then the chosen one when it is none of them.
fn models(found: &[Found], pick: &Pick) -> (String, Menu) {
  let only = pick.other.is_none();
  let mut items: Vec<Item> = (found.iter().enumerate())
    .map(|(i, (m, _))| {
      let label = short(m.full_name);
      Item::new(&format!("model:{}", m.name), &label).checked(only && pick.active == Some(i))
    })
    .collect();
  let other = pick.other.as_ref().map(|(name, there)| match there {
    true => name.clone(),
    false => format!("{name} (not on disk)"),
  });
  let title = match (&other, pick.active) {
    (Some(other), _) => short(other),
    (None, Some(i)) => short(found[i].0.full_name),
    (None, None) => "No Model".to_owned(),
  };
  if let Some(other) = other {
    items.push(Item::new("model-chosen", &short(&other)).checked(pick.gone().is_none()));
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
      ("warn:hotkey", false),
      ("warn:microphone", true),
      ("warn:clipboard", true),
      ("warn:secure-input", true),
      ("warn:download", true),
      ("warn:status", false),
    ];
    assert_eq!(ids, expected);
    assert_eq!(list[6].text, "No sound from USB Mic");
    assert_eq!(icon(&s, &list), Icon::Warning);
    s.state = Phase::Recording;
    assert_eq!(icon(&s, &list), Icon::Recording, "the phase first");

    let device = |name: &str, default| Device { id: Some(name.into()), name: name.into(), default };
    let devices =
      [device("MacBook Pro Microphone", true), device("Elgato Wave:3 Microphone Array", false)];
    let wave = Choice { id: devices[1].id.clone(), name: devices[1].name.clone() };
    let mics = (&devices[..], Some(&wave));
    let items = menu(Some("⌥Space"), &s, &list, &[], mics, &[], &Pick::default()).items;
    assert!(items[0].label.ends_with("⌥Space · Recording…"), "{}", items[0].label);
    assert_eq!(
      (items[1].label.as_str(), items[6].label.as_str()),
      ("⚠︎ Allow Accessibility…", "Download a Speech Model…")
    );
    assert!(items[1].enabled && !items[2].enabled);
    // One name in the menu and its submenu, a long one shortened.
    let mic = items.iter().find(|i| i.id == "mic").unwrap();
    assert_eq!(mic.label, "Elgato Wave:3 Microphone Array");
    let sub = &mic.submenu.as_ref().unwrap().items;
    let got: Vec<_> = sub.iter().map(|i| (i.label.as_str(), i.checked)).collect();
    assert_eq!(got[0], ("System Default (MacBook Pro…)", false));
    let devices_got = &got[2..];
    assert_eq!(devices_got, [("MacBook Pro Microphone", false), (devices[1].name.as_str(), true)]);
    assert_eq!(short("a-very-long-model-name-for-the-menu-q8"), "a-very-long-model-name-for-the…");

    let items = menu(None, &s, &list, &[], (&devices[..], None), &[], &Pick::default()).items;
    let mic = items.iter().find(|i| i.id == "mic").unwrap();
    assert_eq!(mic.label, "System Default (MacBook Pro…)");
  }
}
