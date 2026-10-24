// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Settings: `settings.json` in the data home ([`home`](crate::home)).
//!
//! - A missing file, or a missing field, is the default.
//! - Each field is read on its own: one this version does not understand (a value of a newer
//!   Wavo's, a typo) is the default, with a warning, and its value is written back as it was until
//!   the setting is changed here ([`Settings::unread`]). A file that is not a JSON object is all
//!   defaults, the file copied to `settings.json.bad` first: the next save overwrites it.
//! - Keys this version does not know are kept through a save ([`Settings::unknown`]).
//! - Writes are atomic: a temporary file next to it, `fsync`, then `rename`.
//! - `schema_version` says which layout a file has. Migrations go into [`Settings::load`]; a file
//!   from a newer version loads what this version knows, with a warning, and keeps its number
//!   through a save: the newer version's keys stay in its layout.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
  /// Keys this version does not know (a newer Wavo's), written back as they were.
  #[serde(flatten)]
  pub unknown: Map<String, Value>,
  /// Values of known keys this version could not read (a newer Wavo's), their fields the defaults
  /// meanwhile: written back as they were until the setting is changed
  /// ([`Settings::changed_from`]).
  #[serde(skip)]
  pub unread: Map<String, Value>,
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
      unknown: Map::new(),
      unread: Map::new(),
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

    let file = match serde_json::from_slice::<Map<String, Value>>(&bytes) {
      Ok(file) => file,
      Err(e) => {
        let kept = keep_aside(path, &bytes);
        let warning = format!("{} is invalid ({e}); using defaults, {kept}", path.display());
        return (Self::default(), Some(warning));
      }
    };

    let settings = Self::parse(file);
    let mut warnings = Vec::new();
    if settings.schema_version > SCHEMA_VERSION {
      warnings.push(format!(
        "{} is from a newer Wavo (schema {}): settings this version does not know are kept, not \
         used",
        path.display(),
        settings.schema_version
      ));
    }

    if !settings.unread.is_empty() {
      let keys = settings.unread.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
      warnings.push(format!(
        "{}: {keys} not understood, using defaults; kept in the file until changed",
        path.display()
      ));
    }

    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    let schema_version = settings.schema_version.max(SCHEMA_VERSION);
    (Self { schema_version, ..settings }, warning)
  }

  /// The settings of a file's keys, each one on its own: a value that does not parse is left out
  /// (its field is the default) and kept in [`Settings::unread`].
  fn parse(file: Map<String, Value>) -> Self {
    let parses = |key: &String, value: &Value| {
      let one = Map::from_iter([(key.clone(), value.clone())]);
      serde_json::from_value::<Self>(Value::Object(one)).is_ok()
    };
    let (good, unread): (Map<_, _>, Map<_, _>) = file.into_iter().partition(|(k, v)| parses(k, v));
    let settings: Self = serde_json::from_value(Value::Object(good)).unwrap_or_default();
    Self { unread, ..settings }
  }

  /// `self` as the edit of `old`: the settings it changes no longer keep the values this version
  /// could not read.
  pub fn changed_from(mut self, old: &Settings) -> Self {
    if !self.unread.is_empty()
      && let (Ok(Value::Object(old)), Ok(Value::Object(new))) =
        (serde_json::to_value(old), serde_json::to_value(&self))
    {
      self.unread.retain(|key, _| old.get(key) == new.get(key));
    }

    self
  }

  /// Writes the settings to `path` atomically, creating its directory.
  pub fn save(&self, path: &Path) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    // Straight from the struct while it can: a `Value` writes the keys sorted, not in field order.
    let mut json = if self.unread.is_empty() {
      serde_json::to_vec_pretty(self)?
    } else {
      let mut file = serde_json::to_value(self)?;
      if let Value::Object(file) = &mut file {
        file.extend(self.unread.clone());
      }

      serde_json::to_vec_pretty(&file)?
    };
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

  /// The paste puts the clipboard back afterwards, which needs reading it.
  pub fn restores_clipboard(&self) -> bool {
    self.auto_paste && !self.keep_in_clipboard
  }

  /// `language` for the engine: see [`engine_language`].
  pub fn engine_language(&self) -> Option<String> {
    engine_language(&self.language)
  }

  pub fn unload_after(&self) -> Option<Duration> {
    self.unload_after_minutes.map(|m| Duration::from_secs(u64::from(m) * 60))
  }
}

/// Copies the file that could not be used to `settings.json.bad`; what the warning says of it.
fn keep_aside(path: &Path, bytes: &[u8]) -> String {
  let bad = path.with_extension("json.bad");
  match fs::write(&bad, bytes) {
    Ok(()) => format!("kept as {}", bad.display()),
    Err(e) => format!("could not keep a copy: {e}"),
  }
}

/// A language code for the engine; empty or `auto` is `None`, which leaves it to the model.
pub fn engine_language(code: &str) -> Option<String> {
  let code = code.trim();
  (!code.is_empty() && !code.eq_ignore_ascii_case("auto")).then(|| code.to_owned())
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
    assert_eq!((s.engine_language(), s.unload_after()), (None, Some(Duration::from_secs(300))));
    assert_eq!(s.history_retention, Retention::Unlimited);
    assert!(s.auto_paste && !s.keep_in_clipboard && !s.autostart && !s.start_in_tray);
  }

  #[test]
  fn the_engine_gets_a_language_but_auto() {
    let language =
      |l: &str| Settings { language: l.into(), ..Settings::default() }.engine_language();
    assert_eq!(language(" ru "), Some("ru".into()));
    assert_eq!((language("AUTO"), language("")), (None, None));
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
  fn missing_fields_take_defaults_and_unknown_ones_are_kept_through_a_save() {
    let dir = TempDir::new("settings-partial");
    let path = dir.path().join(FILE);
    fs::write(&path, r#"{"hotkey": "ctrl+option+cmd+F12", "later_feature": {"on": true}}"#)
      .unwrap();
    let (settings, warning) = Settings::load(&path);
    assert_eq!(warning, None);
    assert_eq!(settings.hotkey, "ctrl+option+cmd+F12");
    assert_eq!(settings.unknown.get("later_feature"), Some(&serde_json::json!({"on": true})));
    assert_eq!(
      Settings { unknown: Map::new(), ..settings.clone() },
      Settings { hotkey: "ctrl+option+cmd+F12".into(), ..Settings::default() }
    );

    Settings { language: "ru".into(), ..settings }.save(&path).unwrap();
    let (saved, _) = Settings::load(&path);
    assert_eq!(saved.language, "ru");
    assert_eq!(saved.unknown.get("later_feature"), Some(&serde_json::json!({"on": true})));
  }

  /// A value this version does not understand (a newer Wavo's) costs only its own field, and is
  /// written back as it was until that setting is changed.
  #[test]
  fn a_value_not_understood_is_the_default_and_kept_until_changed() {
    let dir = TempDir::new("settings-bad-value");
    let path = dir.path().join(FILE);
    let text =
      r#"{"activation": "sometimes", "language": "ru", "hud_position": 3, "autostart": true}"#;
    fs::write(&path, text).unwrap();

    let (settings, warning) = Settings::load(&path);
    let Value::Object(unread) = serde_json::json!({"activation": "sometimes", "hud_position": 3})
    else {
      unreachable!()
    };
    let expected = Settings { language: "ru".into(), autostart: true, ..Settings::default() };
    assert_eq!(settings, Settings { unread, ..expected });

    let warning = warning.unwrap();
    assert!(warning.contains("activation, hud_position not understood"), "{warning}");
    assert!(!dir.path().join("settings.json.bad").exists(), "nothing lost, nothing set aside");

    // Another setting changed: both values stay in the file.
    let edited = Settings { language: "en".into(), ..settings.clone() }.changed_from(&settings);
    edited.save(&path).unwrap();
    let file: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!((&file["activation"], &file["hud_position"]), (&"sometimes".into(), &3.into()));
    let (loaded, _) = Settings::load(&path);
    assert_eq!((loaded.language.as_str(), loaded.unread.len()), ("en", 2));

    // The setting changed here: its value replaces the one not understood.
    let toggle =
      Settings { activation: Activation::Toggle, ..loaded.clone() }.changed_from(&loaded);
    toggle.save(&path).unwrap();
    let (loaded, warning) = Settings::load(&path);
    assert_eq!(loaded.activation, Activation::Toggle);
    assert!(warning.unwrap().contains(": hud_position not understood"));
  }

  #[test]
  fn a_file_that_is_no_json_object_is_kept_aside_and_defaults_are_used() {
    let dir = TempDir::new("settings-bad");
    let path = dir.path().join(FILE);
    for text in [r#"{"activation": "#, r#"["toggle"]"#] {
      fs::write(&path, text).unwrap();
      let (settings, warning) = Settings::load(&path);
      assert_eq!(settings, Settings::default());

      let warning = warning.unwrap();
      assert!(warning.contains("invalid") && warning.contains("settings.json.bad"), "{warning}");
      assert_eq!(fs::read_to_string(dir.path().join("settings.json.bad")).unwrap(), text);
    }
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
    fs::write(&path, r#"{"schema_version": 7, "language": "en", "activation": "double_tap"}"#)
      .unwrap();
    let (settings, warning) = Settings::load(&path);
    assert_eq!(settings.language, "en");
    assert_eq!(settings.schema_version, 7);
    let warning = warning.unwrap();
    assert!(warning.contains("newer") && warning.contains("activation"), "{warning}");

    // Saved, it stays the newer layout: its number and its value.
    Settings { language: "ru".into(), ..settings }.save(&path).unwrap();
    let file: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!((&file["schema_version"], &file["activation"]), (&7.into(), &"double_tap".into()));
  }
}
