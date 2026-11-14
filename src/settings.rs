// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `settings.json` in the data home: a missing file or field is the default; a file that does not
//! parse is copied to `settings.json.bad` and the defaults are used.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use isolang::Language;
use serde::{Deserialize, Serialize};

use crate::audio::Microphone;
use crate::history::Retention;

pub const FILE: &str = "settings.json";
/// [Settings::language] that follows the keyboard layout: its language, its row's model.
pub const KEYBOARD: &str = "keyboard";
// Option-Space on macOS, as dictation apps there have it; Ctrl+Alt+Space elsewhere, where Alt+Space
// opens a window's menu.
const DEFAULT_HOTKEY: &str =
  if cfg!(target_os = "macos") { "option+space" } else { "ctrl+alt+space" };

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub schema_version: u32,
  /// A chord (`option+space`) or a modifier alone (`left_option`, `fn`), nimbo's hotkey grammar.
  pub hotkey: String,
  pub activation: Activation,
  pub microphone: Microphone,
  /// A language code, `auto` for the model to detect it, or [KEYBOARD].
  pub language: String,
  /// A model name or a GGUF file path; `None` is the first model found.
  pub model: Option<String>,
  /// With [KEYBOARD], the model of a layout's language (`ru`: `gigaam-v3`); a language without
  /// one is `model`'s.
  pub keyboard_models: BTreeMap<String, String>,
  pub auto_paste: bool,
  /// Leave the text on the clipboard instead of restoring it after the paste.
  pub keep_in_clipboard: bool,
  /// The text comes without hesitation sounds, and its paste ends with a space and goes on a
  /// sentence the paste before it left open.
  pub clean_text: bool,
  /// Free the idle model after this many minutes: 0 right after each dictation, `None` never.
  pub unload_after_minutes: Option<u32>,
  /// Parakeet and GigaAM run on the Neural Engine once prepared for it, else on the GPU.
  pub neural_engine: bool,
  pub history_retention: Retention,
  /// Launch at login.
  pub autostart: bool,
  pub hud_position: HudPosition,
  pub hud_style: HudStyle,
  /// After each dictation the HUD says how long the text took: a debugging aid.
  pub show_transcription_time: bool,
  /// Wayland's GlobalShortcuts portal took the hotkey once: later launches bind it without a click
  /// (the desktop remembers the person's answer).
  pub hotkey_portal: bool,
  /// Check for a new version at launch and once a day, and offer it in the menu (a release only).
  pub auto_update: bool,
  /// The version Skip This Version passed over: automatic checks don't offer it.
  pub skipped_update: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
  /// Records while the hotkey is held; a tap starts a recording the next press stops.
  #[default]
  HoldOrToggle,
  PushToTalk,
  Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudPosition {
  #[default]
  Bottom,
  Top,
}

/// The HUD's pill under a dictation: the icon's colours flowing, or the plain pill of the system's
/// appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudStyle {
  #[default]
  Gradient,
  Plain,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      schema_version: 1,
      hotkey: DEFAULT_HOTKEY.into(),
      activation: Activation::HoldOrToggle,
      microphone: Microphone::Automatic,
      language: "auto".into(),
      model: None,
      keyboard_models: BTreeMap::new(),
      auto_paste: true,
      keep_in_clipboard: false,
      clean_text: true,
      unload_after_minutes: Some(15),
      neural_engine: true,
      history_retention: Retention::Unlimited,
      autostart: false,
      hud_position: HudPosition::Bottom,
      hud_style: HudStyle::Gradient,
      show_transcription_time: false,
      hotkey_portal: false,
      auto_update: true,
      skipped_update: None,
    }
  }
}

impl Settings {
  /// The defaults where pasting can't work (a Mac account that isn't an administrator).
  pub fn without_paste() -> Self {
    Self { auto_paste: false, keep_in_clipboard: true, ..Self::default() }
  }

  /// The settings at `path`, and a warning when they could not be used.
  pub fn load(path: &Path) -> (Self, Option<String>) {
    let bytes = match fs::read(path) {
      Ok(bytes) => bytes,
      Err(e) if e.kind() == ErrorKind::NotFound => return (Self::default(), None),
      // Moved aside, as one that does not parse: the next save must not write over it.
      Err(e) => {
        let bad = path.with_extension("json.bad");
        let kept = fs::rename(path, &bad)
          .map_or_else(|_| String::new(), |_| format!(", kept as {}", bad.display()));
        return (Self::default(), Some(format!("{}: {e}; using defaults{kept}", path.display())));
      }
    };

    match serde_json::from_slice(&bytes) {
      Ok(settings) => (settings, None),
      Err(e) => {
        let bad = path.with_extension("json.bad");
        let _ = fs::write(&bad, &bytes);
        let warning =
          format!("{} is invalid ({e}); using defaults, kept as {}", path.display(), bad.display());
        (Self::default(), Some(warning))
      }
    }
  }

  /// Writes through a temporary file renamed into place: the old file or the new one, never half.
  pub fn save(&self, path: &Path) -> Result<()> {
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let mut json = serde_json::to_vec_pretty(self)?;
    json.push(b'\n');

    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
  }

  /// The paste puts the clipboard back afterwards, which needs reading it.
  pub fn restores_clipboard(&self) -> bool {
    self.auto_paste && !self.keep_in_clipboard
  }

  /// The language chosen for the engine; `None` (empty, `auto` or [KEYBOARD]) leaves it to the
  /// model, or to the layout.
  pub fn engine_language(&self) -> Option<String> {
    let code = self.language.trim();
    let chosen = !code.is_empty() && !code.eq_ignore_ascii_case("auto") && code != KEYBOARD;
    chosen.then(|| code.to_owned())
  }

  pub fn follows_keyboard(&self) -> bool {
    self.language == KEYBOARD
  }

  /// The model choice and the language of a dictation while the keyboard layout's language is
  /// `keyboard`: with [KEYBOARD] its row's model, else `model`, asked in that language (detected
  /// for a layout without one); otherwise the ones chosen.
  pub fn route(&self, keyboard: Option<&str>) -> (Option<String>, Option<String>) {
    if !self.follows_keyboard() {
      return (self.model.clone(), self.engine_language());
    }

    let model = keyboard.and_then(|k| self.keyboard_models.get(k)).or(self.model.as_ref());
    (model.cloned(), keyboard.map(str::to_owned))
  }

  pub fn unload_after(&self) -> Option<Duration> {
    self.unload_after_minutes.map(|m| Duration::from_secs(u64::from(m) * 60))
  }
}

// MARK: Languages

/// A language's English name by the code wavo's models list it under (`ru`: `Russian`): ISO 639-1,
/// else 639-3, and Whisper's `jw` for Javanese; an unknown code as it is.
pub fn language_name(code: &str) -> String {
  let lower = code.to_ascii_lowercase();
  let lower = if lower == "jw" { "jv" } else { lower.as_str() };
  let language = Language::from_639_1(lower).or_else(|| Language::from_639_3(lower));
  language.map_or_else(|| code.to_owned(), |l| l.to_name().to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::audio::Choice;
  use crate::testing::TempDir;

  #[test]
  fn saves_and_loads_back() {
    let dir = TempDir::new("settings-save");
    let path = dir.path().join("nested").join(FILE);
    assert_eq!(Settings::load(&path), (Settings::default(), None));

    let settings = Settings {
      hotkey: "fn".into(),
      activation: Activation::Toggle,
      microphone: Microphone::Device(Choice {
        id: Some("uid-1".into()),
        name: "RØDE PodMic USB".into(),
      }),
      language: "ru".into(),
      unload_after_minutes: None,
      history_retention: Retention::Count(500),
      hud_position: HudPosition::Top,
      hud_style: HudStyle::Plain,
      ..Settings::default()
    };
    settings.save(&path).unwrap();
    assert_eq!(Settings::load(&path), (settings, None));
    assert!(fs::read_to_string(&path).unwrap().contains(r#""hud_style": "plain""#));
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temporary file left");

    // An older file: missing fields are the defaults, a retired one is dropped.
    fs::write(&path, r#"{"hotkey": "fn", "start_in_tray": true}"#).unwrap();
    let older = Settings { hotkey: "fn".into(), ..Settings::default() };
    assert_eq!(Settings::load(&path), (older, None));

    // The microphone as stored: `null` is the system default, as before Automatic.
    for (stored, mic) in [("null", Microphone::System), (r#""automatic""#, Microphone::Automatic)] {
      fs::write(&path, format!(r#"{{"microphone": {stored}}}"#)).unwrap();
      assert_eq!(Settings::load(&path).0.microphone, mic, "{stored}");
      Settings { microphone: mic, ..Settings::default() }.save(&path).unwrap();
      assert!(fs::read_to_string(&path).unwrap().contains(&format!(r#""microphone": {stored}"#)));
    }

    // The history kept until Wavo quits.
    let memory = Settings { history_retention: Retention::UntilQuit, ..Settings::default() };
    memory.save(&path).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains(r#""history_retention": "until_quit""#));
    assert_eq!(Settings::load(&path), (memory, None));
  }

  /// The layout's row, else the model, in the layout's language; any other choice as chosen,
  /// whatever the layout.
  #[test]
  fn a_dictation_routes_by_the_keyboard_layout() {
    let rows = BTreeMap::from([("ru".to_owned(), "gigaam-v3".to_owned())]);
    let model = Some("parakeet-v3".to_owned());
    let keyboard =
      Settings { language: KEYBOARD.into(), model, keyboard_models: rows, ..Settings::default() };
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(keyboard.route(Some("ru")), (some("gigaam-v3"), some("ru")));
    assert_eq!(keyboard.route(Some("en")), (some("parakeet-v3"), some("en")));
    assert_eq!(keyboard.route(None), (some("parakeet-v3"), None), "a layout without a language");
    assert_eq!(keyboard.engine_language(), None);

    let fixed = Settings { language: "de".into(), ..keyboard.clone() };
    assert_eq!(fixed.route(Some("ru")), (some("parakeet-v3"), some("de")));
    let auto = Settings { language: "auto".into(), ..keyboard };
    assert_eq!(auto.route(Some("ru")), (some("parakeet-v3"), None));
  }

  #[test]
  fn a_file_that_does_not_parse_is_kept_aside() {
    let dir = TempDir::new("settings-bad");
    let path = dir.path().join(FILE);
    for text in [r#"{"activation": "#, r#"{"activation": "sometimes"}"#] {
      fs::write(&path, text).unwrap();
      let (settings, warning) = Settings::load(&path);
      assert_eq!(settings, Settings::default());
      assert!(warning.unwrap().contains("settings.json.bad"));
      assert_eq!(fs::read_to_string(dir.path().join("settings.json.bad")).unwrap(), text);
    }
  }

  #[test]
  fn every_language_of_wavos_models_has_a_name() {
    assert_eq!((language_name("EN"), language_name("jw")), ("English".into(), "Javanese".into()));
    for code in wavo::hub::MODELS.iter().flat_map(|m| m.langs) {
      assert_ne!(language_name(code), *code, "{code} has no name");
    }
  }
}
