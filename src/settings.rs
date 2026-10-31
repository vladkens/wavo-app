// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `settings.json` in the data home: a missing file or field is the default; a file that does not
//! parse is copied to `settings.json.bad` and the defaults are used.

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
  /// A language code, or `auto` for the model to detect it.
  pub language: String,
  /// A model name or a GGUF file path; `None` is the first model found.
  pub model: Option<String>,
  /// Models a dictation can go to instead, each by its key.
  pub keys: Vec<ModelKey>,
  pub auto_paste: bool,
  /// Leave the text on the clipboard instead of restoring it after the paste.
  pub keep_in_clipboard: bool,
  /// Free the idle model after this many minutes: 0 right after each dictation, `None` never.
  pub unload_after_minutes: Option<u32>,
  /// Parakeet and GigaAM run on the Neural Engine once prepared for it, else on the GPU.
  pub neural_engine: bool,
  pub history_retention: Retention,
  /// Launch at login.
  pub autostart: bool,
  pub hud_position: HudPosition,
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

/// A model's key: one letter or digit pressed while a hotkey is held (`z`), or a hotkey of the
/// model's own in nimbo's grammar (`f1`, `fn+1`, `ctrl+option+r`), held or tapped as the main one.
/// Either way the dictation goes to the model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelKey {
  pub key: String,
  /// A model name or a GGUF file path, as [Settings::model].
  pub model: String,
}

impl ModelKey {
  /// The letter or digit held with a hotkey, lowercase; `None` for a hotkey of the model's own.
  pub fn held(&self) -> Option<char> {
    let mut chars = self.key.trim().chars();
    match (chars.next(), chars.next()) {
      (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_lowercase()),
      _ => None,
    }
  }
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

impl Default for Settings {
  fn default() -> Self {
    Self {
      schema_version: 1,
      hotkey: DEFAULT_HOTKEY.into(),
      activation: Activation::HoldOrToggle,
      microphone: Microphone::Automatic,
      language: "auto".into(),
      model: None,
      keys: Vec::new(),
      auto_paste: true,
      keep_in_clipboard: false,
      unload_after_minutes: Some(15),
      neural_engine: true,
      history_retention: Retention::Unlimited,
      autostart: false,
      hud_position: HudPosition::Bottom,
      show_transcription_time: false,
      hotkey_portal: false,
      auto_update: true,
      skipped_update: None,
    }
  }
}

impl Settings {
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

  /// The language for the engine; `None` (empty or `auto`) leaves it to the model.
  pub fn engine_language(&self) -> Option<String> {
    let code = self.language.trim();
    (!code.is_empty() && !code.eq_ignore_ascii_case("auto")).then(|| code.to_owned())
  }

  pub fn unload_after(&self) -> Option<Duration> {
    self.unload_after_minutes.map(|m| Duration::from_secs(u64::from(m) * 60))
  }

  /// The model keys in use, in order: those with a key and a model, each key (in any case) and
  /// each model once, the first one.
  pub fn model_keys(&self) -> Vec<&ModelKey> {
    let mut keys: Vec<&ModelKey> = Vec::new();
    for k in &self.keys {
      let (key, model) = (k.key.trim(), k.model.trim());
      let taken =
        keys.iter().any(|u| u.key.trim().eq_ignore_ascii_case(key) || u.model.trim() == model);
      if !key.is_empty() && !model.is_empty() && !taken {
        keys.push(k);
      }
    }

    keys
  }

  /// The model keys that are hotkeys of their own, in the order the hotkey listener gets them
  /// after the main one: hotkey 1 is the first.
  pub fn own_hotkeys(&self) -> Vec<&ModelKey> {
    self.model_keys().into_iter().filter(|k| k.held().is_none()).collect()
  }

  /// The model of hotkey `which`: `None` for the main one (0), else one of
  /// [own_hotkeys](Self::own_hotkeys).
  pub fn hotkey_model(&self, which: usize) -> Option<&str> {
    let own = self.own_hotkeys().into_iter().nth(which.checked_sub(1)?)?;
    Some(own.model.as_str())
  }

  /// The model keys that are a letter or digit held with a hotkey, in the order the hotkey
  /// listener gets them: key 0 is the first.
  pub fn held_keys(&self) -> Vec<&ModelKey> {
    self.model_keys().into_iter().filter(|k| k.held().is_some()).collect()
  }

  /// The model of key `index` of [held_keys](Self::held_keys).
  pub fn held_key_model(&self, index: usize) -> Option<&str> {
    let key = self.held_keys().into_iter().nth(index)?;
    Some(key.model.as_str())
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
      keys: vec![ModelKey { key: "z".into(), model: "gigaam-v3".into() }],
      unload_after_minutes: None,
      history_retention: Retention::Count(500),
      hud_position: HudPosition::Top,
      ..Settings::default()
    };
    settings.save(&path).unwrap();
    assert_eq!(Settings::load(&path), (settings, None));
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temporary file left");

    // An older file: missing fields are the defaults, a retired one is dropped.
    fs::write(&path, r#"{"hotkey": "fn", "start_in_tray": true}"#).unwrap();
    let older = Settings { hotkey: "fn".into(), ..Settings::default() };
    assert_eq!(Settings::load(&path), (older, None));
    assert!(Settings::load(&path).0.keys.is_empty(), "no model keys");

    // The microphone as stored: `null` is the system default, as before Automatic.
    for (stored, mic) in [("null", Microphone::System), (r#""automatic""#, Microphone::Automatic)] {
      fs::write(&path, format!(r#"{{"microphone": {stored}}}"#)).unwrap();
      assert_eq!(Settings::load(&path).0.microphone, mic, "{stored}");
      Settings { microphone: mic, ..Settings::default() }.save(&path).unwrap();
      assert!(fs::read_to_string(&path).unwrap().contains(&format!(r#""microphone": {stored}"#)));
    }
  }

  /// Settings with these model keys: (key, model).
  fn with_keys(keys: &[(&str, &str)]) -> Settings {
    let keys = keys.iter().map(|&(key, model)| ModelKey { key: key.into(), model: model.into() });
    Settings { keys: keys.collect(), ..Settings::default() }
  }

  /// The keys in use, in the order they come: a key taken (in any case), a model with one, an
  /// empty key or model are passed over.
  #[test]
  fn model_keys_take_each_key_and_model_once() {
    let s = with_keys(&[
      ("Z", "gigaam-v3"),
      ("f1", "parakeet-v2"),
      ("z", "whisper-turbo"),
      ("x", "gigaam-v3"),
      (" ", "moonshine-tiny"),
      ("ctrl+option+r", "whisper-turbo"),
      ("7", ""),
    ]);
    let keys: Vec<&str> = s.model_keys().into_iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["Z", "f1", "ctrl+option+r"]);
  }

  /// A letter or digit is held with a hotkey; anything else is a hotkey of its own, numbered after
  /// the main one.
  #[test]
  fn model_keys_are_held_or_hotkeys_of_their_own() {
    let s = with_keys(&[
      ("Z", "gigaam-v3"),
      ("f1", "parakeet-v2"),
      ("7", "whisper-turbo"),
      ("ctrl+option+r", "moonshine-tiny"),
    ]);
    let held: Vec<Option<char>> = s.held_keys().into_iter().map(ModelKey::held).collect();
    assert_eq!(held, [Some('z'), Some('7')]);
    assert_eq!((s.held_key_model(1), s.held_key_model(2)), (Some("whisper-turbo"), None));
    let hotkeys: Vec<Option<&str>> = (0..4).map(|which| s.hotkey_model(which)).collect();
    assert_eq!(hotkeys, [None, Some("parakeet-v2"), Some("moonshine-tiny"), None]);
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
