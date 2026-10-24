// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar menu and icon, from the session's status and what the system says ([System]):
//! a header with the app, the hotkey and the state; the warnings that need the person, some
//! opening where to fix them; Copy Last Dictation and the recent dictations; the model in use
//! with the others downloaded, Unload Model; History…, Settings… and Quit.

use nimbo::tray::{Item, Menu};

use super::icons::Icon;
use crate::engine::supervisor::Phase as EnginePhase;
use crate::history::{Local, local_time, shorten};
use crate::session::controller::{NoModel, Outcome, Phase, Status};
use crate::window::models::{Found, Pick, single_language, size_text};

/// The most dictations «Recent» lists.
pub const RECENT: usize = 10;
/// The longest preview of a dictation in «Recent», in characters.
const PREVIEW: usize = 48;

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

/// The model submenu: the models downloaded, each a choice (`model:<name>`, wavo's name) with the
/// one that transcribes checked, then the chosen one when it is none of them; and its item's title,
/// the model in use.
#[derive(Debug, Clone, PartialEq)]
pub struct Models {
  /// `(id, label, checked)`.
  pub items: Vec<(String, String, bool)>,
  pub title: String,
}

impl Models {
  pub fn new(found: &[Found], pick: &Pick) -> Self {
    let mut items: Vec<(String, String, bool)> = (found.iter().enumerate())
      .map(|(i, (m, _))| {
        // A model meant for one language says so before it is picked.
        let mut label = format!("{} — {}", m.full_name, size_text(m.size));
        if let Some(only) = single_language(m) {
          label += &format!(" · {only}");
        }

        (format!("model:{}", m.name), label, pick.active == Some(i) && pick.other.is_none())
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
      items.push(("model-chosen".into(), other, pick.gone().is_none()));
    }

    Self { items, title }
  }
}

/// A dictation in «Recent»: when (the time today, else the day) and its text on one line,
/// shortened: `14:05 · Hello there, this is…`.
pub fn recent_title(text: &str, created_at: i64, now: i64) -> String {
  let when = match (local_time(created_at), local_time(now)) {
    (Some(t), Some(today)) => when(t, today),
    _ => String::new(),
  };
  let preview = shorten(text, PREVIEW);
  if when.is_empty() { preview } else { format!("{when} · {preview}") }
}

fn when(t: Local, today: Local) -> String {
  match t.date() == today.date() {
    true => t.hm(),
    false => format!("{} {}", &t.month_name()[..3], t.day),
  }
}

/// The menu: the header, the warnings, Copy Last Dictation, Recent ▸ (`recent:<i>`), the model ▸,
/// Unload Model, History…, Settings…, Quit.
pub fn menu(
  hotkey: Option<&str>,
  status: &Status,
  warnings: &[Warning],
  recent: &[String],
  models: &Models,
) -> Menu {
  let header = [Some(TITLE), hotkey, state(status)].into_iter().flatten();
  let header = header.collect::<Vec<_>>().join(" · ");
  let mut items = vec![Item::new("header", &header).enabled(false)];
  for w in warnings {
    items.push(Item::new(w.id, &format!("⚠︎ {}", w.text)).enabled(w.action));
  }

  let last = status.last.as_ref().is_some_and(|l| !l.text.is_empty());
  let loaded = status.engine.phase != EnginePhase::Stopped && status.state == Phase::Idle;
  let recent: Vec<Item> = (recent.iter().enumerate())
    .map(|(i, title)| Item::new(&format!("recent:{i}"), title))
    .collect();
  let mut choices: Vec<Item> =
    (models.items.iter()).map(|(id, label, on)| Item::new(id, label).checked(*on)).collect();
  if choices.is_empty() {
    choices.push(Item::new("model-none", "No Models Downloaded").enabled(false));
  }

  choices.extend([Item::separator(), Item::new("models", "All Models…")]);
  items.extend([
    Item::separator(),
    Item::new("copy-last", "Copy Last Dictation").enabled(last),
    Item::new("recent", "Recent").enabled(!recent.is_empty()).submenu(Menu::new(recent)),
    Item::separator(),
    Item::new("model", &models.title).submenu(Menu::new(choices)),
    Item::new("unload", "Unload Model").enabled(loaded),
    Item::separator(),
    Item::new("history", "History…"),
    Item::new("settings", "Settings…").key("cmd+comma"),
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
    let header = |s: &Status| {
      menu(Some("⌥Space"), s, &[], &[], &Models::new(&[], &Pick::default())).items[0].label.clone()
    };
    let mut s = status(Phase::Idle);
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space"));
    s.state = Phase::Recording;
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space · Recording…"));
    (s.state, s.engine.phase) = (Phase::Transcribing, EnginePhase::Loading);
    assert_eq!(header(&s), format!("{TITLE} · ⌥Space · Loading the model…"));
    (s.state, s.engine.phase, s.error) = (Phase::Idle, EnginePhase::Stopped, Some("x".into()));
    assert_eq!(
      menu(None, &s, &[], &[], &Models::new(&[], &Pick::default())).items[0].label,
      format!("{TITLE} · Error")
    );
  }

  #[test]
  fn copy_and_unload_only_when_there_is_something() {
    let mut s = status(Phase::Idle);
    let item = |label: &str, on| (label.to_owned(), on);
    let idle = labels(&menu(None, &s, &[], &[], &Models::new(&[], &Pick::default())));
    assert_eq!(
      idle[1..],
      [
        item("Copy Last Dictation", false),
        item("Recent", false),
        item("No Model", true),
        item("Unload Model", false),
        item("History…", true),
        item("Settings…", true),
        item("Quit Wavo", true),
      ]
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
    let ready =
      labels(&menu(None, &s, &[], &["09:05 · Hi".into()], &Models::new(&[], &Pick::default())));
    assert_eq!(
      ready[1..5],
      [
        item("Copy Last Dictation", true),
        item("Recent", true),
        item("No Model", true),
        item("Unload Model", true),
      ]
    );

    s.state = Phase::Recording;
    assert!(
      !labels(&menu(None, &s, &[], &[], &Models::new(&[], &Pick::default())))[4].1,
      "no unload while it works"
    );
  }

  #[test]
  fn the_model_submenu_checks_the_one_in_use() {
    let found = ["parakeet-v3", "parakeet-v2"].map(|n| (wavo::hub::lookup(n).unwrap(), n.into()));
    let pick = Pick { active: Some(1), other: None };
    let models = Models::new(&found, &pick);
    assert_eq!(models.title, "parakeet-tdt-0.6b-v2");
    assert!(!models.items[0].2 && models.items[1].2);
    assert_eq!(models.items[1].1, "parakeet-tdt-0.6b-v2 — 730 MB · English only");
    let pick = Pick { active: Some(0), other: Some(("whisper-turbo".into(), false)) };
    let gone = Models::new(&found, &pick);
    assert_eq!(gone.title, "whisper-turbo (not on disk)");
    assert_eq!(gone.items.len(), 3);
    assert!(gone.items.iter().all(|i| !i.2), "none is the choice");

    let s = status(Phase::Idle);
    let menu = menu(None, &s, &[], &[], &models);
    let model = menu.items.iter().find(|i| i.id == "model").unwrap();
    let ids: Vec<&str> =
      model.submenu.as_ref().unwrap().items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["model:parakeet-v3", "model:parakeet-v2", "", "models"]);
  }

  #[test]
  fn recent_items_are_short_and_say_when() {
    let local = |day, hour, minute| Local { year: 2026, month: 10, day, hour, minute };
    assert_eq!(when(local(15, 9, 5), local(15, 18, 0)), "09:05");
    assert_eq!(when(local(14, 23, 59), local(15, 0, 1)), "Oct 14");
    let preview = |t: &str| shorten(t, PREVIEW);
    assert_eq!(preview("  Hello\n\tthere  "), "Hello there");
    let long = "This is a long dictation, which goes on and on about the weather today";
    assert_eq!(preview(long), "This is a long dictation, which goes on and on…");
    assert_eq!(preview(&"x".repeat(60)).chars().count(), PREVIEW + 1, "cut inside a long word");
    assert!(recent_title("Hi", 0, 0).ends_with("Hi"));
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

    let items = menu(None, &s, &list, &[], &Models::new(&[], &Pick::default())).items;
    assert_eq!(items[1].label, format!("⚠︎ {}", list[0].text));
    assert!(items[1].enabled && !items[2].enabled);
    assert_eq!(capitalized("dictation x: y"), "Dictation x: y");
  }
}
