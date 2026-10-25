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

use crate::audio::Choice;
use crate::history::Retention;

pub const FILE: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub schema_version: u32,
  /// A chord (`option+space`) or a modifier alone (`left_option`, `fn`), nimbo's hotkey grammar.
  pub hotkey: String,
  pub activation: Activation,
  /// `None` is the system default input.
  pub microphone: Option<Choice>,
  /// A language code, or `auto` for the model to detect it.
  pub language: String,
  /// A model name or a GGUF file path; `None` is the first model found.
  pub model: Option<String>,
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
      hotkey: "option+space".into(),
      activation: Activation::HoldOrToggle,
      microphone: None,
      language: "auto".into(),
      model: None,
      auto_paste: true,
      keep_in_clipboard: false,
      unload_after_minutes: Some(5),
      neural_engine: true,
      history_retention: Retention::Unlimited,
      autostart: false,
      hud_position: HudPosition::Bottom,
    }
  }
}

impl Settings {
  /// The settings at `path`, and a warning when they could not be used.
  pub fn load(path: &Path) -> (Self, Option<String>) {
    let bytes = match fs::read(path) {
      Ok(bytes) => bytes,
      Err(e) if e.kind() == ErrorKind::NotFound => return (Self::default(), None),
      Err(e) => return (Self::default(), Some(format!("{}: {e}; using defaults", path.display()))),
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
  use crate::testing::TempDir;

  #[test]
  fn saves_and_loads_back() {
    let dir = TempDir::new("settings-save");
    let path = dir.path().join("nested").join(FILE);
    assert_eq!(Settings::load(&path), (Settings::default(), None));

    let settings = Settings {
      hotkey: "fn".into(),
      activation: Activation::Toggle,
      microphone: Some(Choice { id: Some("uid-1".into()), name: "RØDE PodMic USB".into() }),
      language: "ru".into(),
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
