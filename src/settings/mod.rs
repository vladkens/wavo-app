// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Settings: `settings.json` in the data home ([`home`](crate::home)).
//!
//! - A missing file, or a missing field, is the default.
//! - A file that does not parse is copied to `settings.json.bad` and the defaults are used, with a
//!   warning: the next save would otherwise overwrite what the person wrote.
//! - Writes are atomic: a temporary file next to it, `fsync`, then `rename`.
//! - `schema_version` says which layout a file has. Migrations go into [`Settings::load`]; a file
//!   from a newer version loads what this version knows, with a warning.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio::devices::Choice;
use crate::history::Retention;

pub const FILE: &str = "settings.json";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
  pub schema_version: u32,
  /// A chord such as `option+space`, or a modifier key alone such as `left_option` or `fn`
  /// (macOS), in nimbo's hotkey grammar.
  pub hotkey: String,
  pub activation: Activation,
  /// `None` is the system default input.
  pub microphone: Option<Choice>,
  /// A language code, or `auto` for the model to detect it.
  pub language: String,
  /// A model name or a GGUF file path; `None` is the first model found.
  pub model: Option<String>,
  /// Paste the text into the app the dictation started in.
  pub auto_paste: bool,
  /// Leave the text on the clipboard; otherwise the clipboard is restored after pasting.
  pub keep_in_clipboard: bool,
  /// Stop the idle engine (and free the model) after this many minutes; 0 is right after each
  /// dictation, `None` never.
  pub unload_after_minutes: Option<u32>,
  pub history_retention: Retention,
  /// Launch at login.
  pub autostart: bool,
  /// Where the HUD (the recording panel) shows on the screen.
  pub hud_position: HudPosition,
  /// Start without the settings window: only the icon in the menu bar (the tray).
  pub start_in_tray: bool,
}

/// How the hotkey starts and stops a dictation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
  /// Hold the hotkey to talk while it is down; a tap starts a recording the next press stops.
  #[default]
  HoldOrToggle,
  /// Records while the hotkey is down.
  PushToTalk,
  /// A press starts, the next press stops.
  Toggle,
}

/// Where the HUD shows: centered near the bottom of the screen (above the Dock, the taskbar) or
/// near the top (under the menu bar).
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
      schema_version: SCHEMA_VERSION,
      hotkey: "option+space".into(),
      activation: Activation::HoldOrToggle,
      microphone: None,
      language: "auto".into(),
      model: None,
      auto_paste: true,
      keep_in_clipboard: false,
      unload_after_minutes: Some(5),
      history_retention: Retention::Unlimited,
      autostart: false,
      hud_position: HudPosition::Bottom,
      start_in_tray: false,
    }
  }
}

impl Settings {
  /// The settings at `path`, and a warning if they could not all be used.
  pub fn load(path: &Path) -> (Self, Option<String>) {
    let bytes = match fs::read(path) {
      Ok(bytes) => bytes,
      Err(e) if e.kind() == ErrorKind::NotFound => return (Self::default(), None),
      Err(e) => {
        return (
          Self::default(),
          Some(format!("could not read {}: {e}; using defaults", path.display())),
        );
      }
    };

    match serde_json::from_slice::<Self>(&bytes) {
      Ok(settings) if settings.schema_version > SCHEMA_VERSION => {
        let warning = format!(
          "{} is from a newer Wavo (schema {}): settings this version does not know are ignored",
          path.display(),
          settings.schema_version
        );
        (Self { schema_version: SCHEMA_VERSION, ..settings }, Some(warning))
      }
      Ok(settings) => (Self { schema_version: SCHEMA_VERSION, ..settings }, None),
      Err(e) => {
        let bad = path.with_extension("json.bad");
        let kept = match fs::write(&bad, &bytes) {
          Ok(()) => format!("kept as {}", bad.display()),
          Err(e) => format!("could not keep a copy: {e}"),
        };
        (
          Self::default(),
          Some(format!("{} is invalid ({e}); using defaults, {kept}", path.display())),
        )
      }
    }
  }

  /// Writes the settings to `path` atomically, creating its directory.
  pub fn save(&self, path: &Path) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let mut json = serde_json::to_vec_pretty(self)?;
    json.push(b'\n');

    // Its own temporary file per save: two saves at once must not write into one.
    static SAVES: AtomicU32 = AtomicU32::new(0);
    let mut tmp = path.file_name().unwrap_or_default().to_owned();
    tmp.push(format!(".tmp-{}-{}", std::process::id(), SAVES.fetch_add(1, Ordering::Relaxed)));
    let tmp = path.with_file_name(tmp);
    let write = || -> std::io::Result<()> {
      let mut file = File::create_new(&tmp)?;
      file.write_all(&json)?;
      file.sync_all()?;
      fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
      let _ = fs::remove_file(&tmp);
    })?;

    // The rename itself is durable once the directory is synced; best effort.
    if let Ok(dir) = File::open(dir) {
      let _ = dir.sync_all();
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::TempDir;

  #[test]
  fn defaults() {
    let s = Settings::default();
    assert_eq!(s.hotkey, "option+space");
    assert_eq!(s.activation, Activation::HoldOrToggle);
    assert_eq!((&s.microphone, &s.model), (&None, &None));
    assert_eq!((s.language.as_str(), s.unload_after_minutes), ("auto", Some(5)));
    assert_eq!(s.history_retention, Retention::Unlimited);
    assert!(s.auto_paste && !s.keep_in_clipboard && !s.autostart && !s.start_in_tray);
  }

  #[test]
  fn missing_file_is_the_defaults() {
    let dir = TempDir::new("settings-missing");
    assert_eq!(Settings::load(&dir.path().join(FILE)), (Settings::default(), None));
  }

  #[test]
  fn saves_atomically_and_loads_back() {
    let dir = TempDir::new("settings-save");
    let path = dir.path().join("nested").join(FILE);
    let settings = Settings {
      hotkey: "ctrl+option+cmd+F12".into(),
      activation: Activation::Toggle,
      microphone: Some(Choice { id: Some("uid-1".into()), name: "RØDE PodMic USB".into() }),
      language: "ru".into(),
      model: Some("gigaam-v3-e2e-rnnt-Q8_0".into()),
      unload_after_minutes: None,
      history_retention: Retention::Count(500),
      hud_position: HudPosition::Top,
      ..Settings::default()
    };
    settings.save(&path).unwrap();
    assert_eq!(Settings::load(&path), (settings.clone(), None));

    let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "no temporary file left: {names:?}");

    let json = fs::read_to_string(&path).unwrap();
    for field in [
      r#""schema_version": 1"#,
      r#""activation": "toggle""#,
      r#""count": 500"#,
      r#""hud_position": "top""#,
    ] {
      assert!(json.contains(field), "{field} in {json}");
    }
  }

  #[test]
  fn missing_fields_take_defaults_and_unknown_ones_are_ignored() {
    let dir = TempDir::new("settings-partial");
    let path = dir.path().join(FILE);
    fs::write(&path, r#"{"hotkey": "ctrl+option+cmd+F12", "later_feature": 1}"#).unwrap();
    let (settings, warning) = Settings::load(&path);
    assert_eq!(warning, None);
    assert_eq!(settings, Settings { hotkey: "ctrl+option+cmd+F12".into(), ..Settings::default() });
  }

  #[test]
  fn invalid_file_is_kept_aside_and_defaults_are_used() {
    let dir = TempDir::new("settings-bad");
    let path = dir.path().join(FILE);
    let text = r#"{"activation": "sometimes"}"#;
    fs::write(&path, text).unwrap();

    let (settings, warning) = Settings::load(&path);
    assert_eq!(settings, Settings::default());

    let warning = warning.unwrap();
    assert!(warning.contains("invalid") && warning.contains("settings.json.bad"), "{warning}");
    assert_eq!(fs::read_to_string(dir.path().join("settings.json.bad")).unwrap(), text);
  }

  #[test]
  fn a_file_that_is_not_utf8_is_kept_aside_too() {
    let dir = TempDir::new("settings-bytes");
    let path = dir.path().join(FILE);
    let bytes = b"{\"language\": \"\xff\"}";
    fs::write(&path, bytes).unwrap();

    let (settings, warning) = Settings::load(&path);
    assert_eq!(settings, Settings::default());
    assert!(warning.unwrap().contains("invalid"));
    assert_eq!(fs::read(dir.path().join("settings.json.bad")).unwrap(), bytes);
  }

  #[test]
  fn saves_at_once_leave_one_whole_file() {
    let dir = TempDir::new("settings-race");
    let path = dir.path().join(FILE);
    std::thread::scope(|s| {
      for i in 0..8 {
        let path = &path;
        s.spawn(move || {
          let settings = Settings { language: "x".repeat(i * 1_000), ..Settings::default() };
          for _ in 0..20 {
            settings.save(path).unwrap();
          }
        });
      }
    });

    assert_eq!(Settings::load(&path).1, None);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left");
  }

  #[test]
  fn newer_schema_loads_what_it_knows_with_a_warning() {
    let dir = TempDir::new("settings-newer");
    let path = dir.path().join(FILE);
    fs::write(&path, r#"{"schema_version": 7, "language": "en"}"#).unwrap();
    let (settings, warning) = Settings::load(&path);
    assert_eq!(settings.language, "en");
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert!(warning.unwrap().contains("newer"));
  }
}
