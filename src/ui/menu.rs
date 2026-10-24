// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar menu and icon, from the session's status and what the system says ([System]):
//! a header with the app, the hotkey and the state; the warnings that need the person, some
//! opening where to fix them; Copy Last Dictation, Unload Model and Quit.

use nimbo::tray::{Item, Menu};

use super::icons::Icon;
use crate::engine::supervisor::Phase as EnginePhase;
use crate::session::controller::{NoModel, Outcome, Phase, Status};

/// The app and its version, as the menu's header says it.
pub const TITLE: &str = concat!("Wavo ", env!("CARGO_PKG_VERSION"));

/// What the system lets the app do, as the shell last looked.
#[derive(Debug, Clone, Default)]
pub struct System {
  /// Accessibility access: the event tap and the paste need it.
  pub accessibility: bool,
  /// The event tap could not be made though access is there.
  pub tap_failed: bool,
  /// Why the hotkey does not work: it does not parse, or the OS refused it.
  pub hotkey_error: Option<String>,
  /// Why the hotkey's fallback (Carbon's, under Secure Input) is not registered.
  pub fallback_error: Option<String>,
  pub microphone_denied: bool,
  /// The paste restores the clipboard, which the app may not read (Paste from Other Apps).
  pub clipboard_unread: bool,
  pub secure_input: bool,
}

/// A warning in the menu (and the icon): its item's id, its text, and whether a click opens where
/// it is fixed.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
  pub id: &'static str,
  pub text: String,
  pub action: bool,
}

/// What needs the person's attention now, the system's first.
pub fn warnings(system: &System, hotkey: &str, status: &Status) -> Vec<Warning> {
  let mut list = Vec::new();
  let mut add = |id, text: String, action| list.push(Warning { id, text, action });
  if !system.accessibility {
    let text = "No Accessibility access: the text is not pasted. Open System Settings…";
    add("warn:accessibility", text.into(), true);
  } else if system.tap_failed {
    add("warn:tap", "The hotkey is not intercepted: the event tap was not created".into(), false);
  }

  if let Some(e) = &system.hotkey_error {
    add("warn:hotkey", format!("The hotkey “{hotkey}” does not work: {e}"), false);
  }

  if let Some(e) = &system.fallback_error {
    add("warn:fallback", format!("The backup hotkey is not registered: {e}"), false);
  }

  if system.microphone_denied {
    add("warn:microphone", "No microphone access. Open System Settings…".into(), true);
  }

  if system.clipboard_unread {
    let text = "The clipboard is not restored after a paste: allow Wavo in Paste from Other Apps…";
    add("warn:clipboard", text.into(), true);
  }

  if system.secure_input {
    let text = "Secure Input is on: the hotkey works through the backup path, Esc may not";
    add("warn:secure-input", text.into(), false);
  }

  match &status.no_model {
    Some(NoModel::NoneDownloaded) => add("warn:model", "No speech model yet".into(), false),
    Some(NoModel::Gone { model }) => add("warn:model", format!("{model} is not on disk"), false),
    Some(NoModel::LoadFailed { model, .. }) => {
      add("warn:model", format!("{model} does not load"), false)
    }
    None => {}
  }

  if let Some(e) = &status.error {
    add("warn:error", format!("Error: {e}"), false);
  }

  if let Some(last) = status.last.as_ref().filter(|l| l.outcome == Outcome::Failed) {
    let e = last.error.as_deref().unwrap_or("unknown error");
    add("warn:last", format!("The last dictation failed: {e}"), false);
  }

  for w in &status.warnings {
    add("warn:session", capitalized(w), false);
  }

  list
}

/// The menu bar icon: the phase, or a warning when idle with any.
pub fn icon(status: &Status, warnings: &[Warning]) -> Icon {
  match status.state {
    Phase::Recording => Icon::Recording,
    Phase::Transcribing | Phase::Pasting => Icon::Transcribing,
    Phase::Idle if warnings.is_empty() => Icon::Idle,
    Phase::Idle => Icon::Warning,
  }
}

/// The menu: the header, the warnings, Copy Last Dictation, Unload Model, Quit.
pub fn menu(hotkey: Option<&str>, status: &Status, warnings: &[Warning]) -> Menu {
  let header = [Some(TITLE), hotkey, state(status)].into_iter().flatten();
  let header = header.collect::<Vec<_>>().join(" · ");
  let mut items = vec![Item::new("header", &header).enabled(false)];
  for w in warnings {
    items.push(Item::new(w.id, &format!("⚠︎ {}", w.text)).enabled(w.action));
  }

  let last = status.last.as_ref().is_some_and(|l| !l.text.is_empty());
  let loaded = status.engine.phase != EnginePhase::Stopped && status.state == Phase::Idle;
  items.extend([
    Item::separator(),
    Item::new("copy-last", "Copy Last Dictation").enabled(last),
    Item::separator(),
    Item::new("unload", "Unload Model").enabled(loaded),
    Item::separator(),
    Item::new("quit", "Quit Wavo").key("cmd+q"),
  ]);
  Menu::new(items)
}

/// What the app does now, unless it is idle: `Recording…`, `Loading the model…`, `Error`.
fn state(status: &Status) -> Option<&'static str> {
  let loading = status.engine.phase == EnginePhase::Loading;
  match status.state {
    Phase::Recording => Some("Recording…"),
    Phase::Transcribing if loading => Some("Loading the model…"),
    Phase::Transcribing => Some("Transcribing…"),
    Phase::Pasting => Some("Pasting…"),
    Phase::Idle if loading => Some("Loading the model…"),
    Phase::Idle if status.error.is_some() => Some("Error"),
    Phase::Idle => None,
  }
}

/// A session warning as the menu says it: without the `dictation N: ` it is kept under, from a
/// capital (`No sound from MacBook Microphone: check the microphone`).
fn capitalized(text: &str) -> String {
  let rest = text
    .strip_prefix("dictation ")
    .and_then(|r| r.split_once(": "))
    .filter(|(n, _)| n.chars().all(|c| c.is_ascii_digit()))
    .map_or(text, |(_, rest)| rest);
  let mut chars = rest.chars();
  chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::controller::{EngineStatus, Last};
  use crate::settings::Activation;

  fn status(state: Phase) -> Status {
    Status {
      state,
      activation: Activation::HoldOrToggle,
      entry: None,
      recording: None,
      engine: EngineStatus::default(),
      last: None,
      retranscribing: Vec::new(),
      error: None,
      warnings: Vec::new(),
      no_model: None,
    }
  }

  fn labels(menu: &Menu) -> Vec<(String, bool)> {
    let items = menu.items.iter().filter(|i| !i.separator);
    items.map(|i| (i.label.clone(), i.enabled)).collect()
  }

  #[test]
  fn the_header_says_the_state_only_when_not_idle() {
    let header = |s: &Status| menu(Some("⌥Space"), s, &[]).items[0].label.clone();
    let mut s = status(Phase::Idle);
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space"));
    s.state = Phase::Recording;
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space · Recording…"));
    (s.state, s.engine.phase) = (Phase::Transcribing, EnginePhase::Loading);
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space · Loading the model…"));
    (s.state, s.engine.phase, s.error) = (Phase::Idle, EnginePhase::Stopped, Some("x".into()));
    assert_eq!(menu(None, &s, &[]).items[0].label, format!("{TITLE} · Error"));
  }

  #[test]
  fn copy_and_unload_only_when_there_is_something() {
    let mut s = status(Phase::Idle);
    let quit = ("Quit Wavo".to_owned(), true);
    let idle = labels(&menu(None, &s, &[]));
    assert_eq!(
      idle[1..],
      [("Copy Last Dictation".into(), false), ("Unload Model".into(), false), quit.clone()]
    );

    s.engine.phase = EnginePhase::Idle;
    s.last = Some(Last {
      take: 1,
      entry: Some(1),
      outcome: Outcome::Done,
      text: "Hi".into(),
      error: None,
      pasted: true,
      not_pasted: None,
      seconds: 1.0,
    });
    let ready = labels(&menu(None, &s, &[]));
    assert_eq!(
      ready[1..],
      [("Copy Last Dictation".into(), true), ("Unload Model".into(), true), quit]
    );

    s.state = Phase::Recording;
    assert!(!labels(&menu(None, &s, &[]))[2].1, "no unload while it works");
  }

  #[test]
  fn warnings_open_what_fixes_them_and_turn_the_icon() {
    let mut s = status(Phase::Idle);
    let fine = System { accessibility: true, ..System::default() };
    assert_eq!(warnings(&fine, "f5", &s), []);
    assert_eq!(icon(&s, &[]), Icon::Idle);

    let system = System {
      accessibility: false,
      tap_failed: true,
      hotkey_error: Some("taken".into()),
      fallback_error: None,
      microphone_denied: true,
      clipboard_unread: true,
      secure_input: true,
    };
    s.no_model = Some(NoModel::Gone { model: "whisper-turbo".into() });
    s.warnings = vec!["dictation 3: no sound from USB Mic".into(), "the settings: x".into()];
    let list = warnings(&system, "f5", &s);
    let ids: Vec<_> = list.iter().map(|w| (w.id, w.action)).collect();
    assert_eq!(
      ids,
      [
        ("warn:accessibility", true),
        ("warn:hotkey", false),
        ("warn:microphone", true),
        ("warn:clipboard", true),
        ("warn:secure-input", false),
        ("warn:model", false),
        ("warn:session", false),
        ("warn:session", false),
      ]
    );
    assert_eq!(list[1].text, "The hotkey “f5” does not work: taken");
    assert_eq!(list[5].text, "whisper-turbo is not on disk");
    assert_eq!(
      (list[6].text.as_str(), list[7].text.as_str()),
      ("No sound from USB Mic", "The settings: x")
    );
    assert_eq!(icon(&s, &list), Icon::Warning);
    s.state = Phase::Recording;
    assert_eq!(icon(&s, &list), Icon::Recording, "the phase first");

    let items = menu(None, &s, &list).items;
    assert_eq!(items[1].label, format!("⚠︎ {}", list[0].text));
    assert!(items[1].enabled && !items[2].enabled);
    assert_eq!(capitalized("dictation x: y"), "Dictation x: y");
  }
}
