// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `settings.json` in the data home: a missing file or field is the default; a file that does not
//! parse is copied to `settings.json.bad` and the defaults are used.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use isolang::Language;
use nimbo::keys::f_key;
use nimbo::{Hotkey, Key, Modifiers, Shortcut};
use serde::{Deserialize, Serialize};
use wavo::hub;

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

/// What a model key is ([ModelKey::binding]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
  /// A letter or digit pressed while a hotkey is held, lowercase.
  Held(char),
  /// A hotkey of the model's own.
  Own(Hotkey),
}

impl ModelKey {
  /// A letter or digit is held with a hotkey; a chord with ⌃, ⌥, ⌘ or fn, an F-key or a modifier
  /// alone is a hotkey of the model's own; `None` for any other (empty, not in nimbo's hotkey
  /// grammar, a key alone or with ⇧ that every app would lose).
  pub fn binding(&self) -> Option<Binding> {
    let key = self.key.trim();
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
      (Some(c), None) if c.is_ascii_alphanumeric() => Some(Binding::Held(c.to_ascii_lowercase())),
      _ => Hotkey::parse(key).ok().filter(|&h| own(h)).map(Binding::Own),
    }
  }
}

/// A hotkey that takes its key from every app without taking typing: a modifier alone, a chord
/// with ⌃, ⌥, ⌘ or fn, an F-key.
fn own(hotkey: Hotkey) -> bool {
  let Hotkey::Shortcut(s) = hotkey else { return true };
  let chord = Modifiers::CONTROL | Modifiers::ALT | Modifiers::META | Modifiers::FN;
  let f = |n| f_key(n).is_some_and(|(_, k)| s.key == Key::Named(k));
  s.mods.intersects(chord) || (1..=20).any(f)
}

impl Binding {
  /// As nimbo listens to it: a held key is the key alone.
  pub fn hotkey(self) -> Hotkey {
    match self {
      Binding::Held(c) => {
        Hotkey::Shortcut(Shortcut { key: Key::Char(c), mods: Modifiers::empty() })
      }
      Binding::Own(h) => h,
    }
  }

  /// As the platform writes it: `Z`, `F1`, `fn 1`, `⌃⌥R`.
  pub fn label(self) -> String {
    self.hotkey().label()
  }
}

/// The key a model's key field recorded ([nimbo::Ui::shortcut_key]) as [ModelKey::key] spells it,
/// or why it can't be one: it is the main `hotkey` or that one's own key, or neither a letter or
/// digit nor a hotkey of its own.
pub fn model_key(recorded: Hotkey, hotkey: &str) -> Result<String, String> {
  let main = Hotkey::parse(hotkey).ok();
  let label = recorded.label();
  if main == Some(recorded) {
    return Err(format!("{label} is the shortcut"));
  }

  // ⌥Space's Space, ⌃Z's Z: the key alone.
  if let (Some(Hotkey::Shortcut(m)), Hotkey::Shortcut(k)) = (main, recorded)
    && k.mods.is_empty()
    && m.code().is_some_and(|c| k.code() == Some(c))
  {
    return Err(format!("{label} is in the shortcut"));
  }

  let key = ModelKey { key: recorded.to_string(), model: String::new() };
  match key.binding() {
    Some(_) => Ok(key.key),
    None => Err(format!("{label} can’t be a key")),
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

  /// The model keys in use, in order: those of a model with a [binding](ModelKey::binding), each
  /// binding and each model once, the first one.
  pub fn model_keys(&self) -> Vec<&ModelKey> {
    let mut keys: Vec<(&ModelKey, Binding)> = Vec::new();
    for k in &self.keys {
      let Some(binding) = k.binding() else { continue };
      let model = k.model.trim();
      let taken = keys.iter().any(|(u, b)| *b == binding || u.model.trim() == model);
      if !model.is_empty() && !taken {
        keys.push((k, binding));
      }
    }

    keys.into_iter().map(|(k, _)| k).collect()
  }

  /// The model keys that are hotkeys of their own, in the order the hotkey listener gets them
  /// after the main one: hotkey 1 is the first.
  pub fn own_hotkeys(&self) -> Vec<&ModelKey> {
    let own = |k: &&ModelKey| matches!(k.binding(), Some(Binding::Own(_)));
    self.model_keys().into_iter().filter(own).collect()
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
    let held = |k: &&ModelKey| matches!(k.binding(), Some(Binding::Held(_)));
    self.model_keys().into_iter().filter(held).collect()
  }

  /// The model of key `index` of [held_keys](Self::held_keys).
  pub fn held_key_model(&self, index: usize) -> Option<&str> {
    let key = self.held_keys().into_iter().nth(index)?;
    Some(key.model.as_str())
  }

  /// The key in use of wavo's model `name` ([model_keys](Self::model_keys)).
  pub fn model_key(&self, name: &str) -> Option<Binding> {
    let of = |k: &&ModelKey| hub::lookup(k.model.trim()).is_ok_and(|m| m.name == name);
    self.model_keys().into_iter().find(of)?.binding()
  }

  /// The keys in use as the platform writes them ([Binding::label]), by wavo's name of their model.
  pub fn key_labels(&self) -> Vec<(&'static str, String)> {
    let label = |k: &ModelKey| Some((hub::lookup(k.model.trim()).ok()?.name, k.binding()?.label()));
    self.model_keys().into_iter().filter_map(label).collect()
  }

  /// Gives wavo's model `name` the `key` ([ModelKey::key]), taken from a model that had it, or
  /// none.
  pub fn set_model_key(&mut self, name: &str, key: Option<String>) {
    let binding =
      key.as_ref().and_then(|key| ModelKey { key: key.clone(), model: String::new() }.binding());
    let of = |k: &ModelKey| hub::lookup(k.model.trim()).is_ok_and(|m| m.name == name);
    self.keys.retain(|k| !of(k) && (binding.is_none() || k.binding() != binding));
    if let Some(key) = key {
      self.keys.push(ModelKey { key, model: name.to_owned() });
    }
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

  /// The keys in use, in the order they come: a key taken (in any case, in any spelling), a model
  /// with one, an empty key or model are passed over.
  #[test]
  fn model_keys_take_each_key_and_model_once() {
    let s = with_keys(&[
      ("Z", "gigaam-v3"),
      ("f1", "parakeet-v2"),
      ("z", "whisper-turbo"),
      ("x", "gigaam-v3"),
      (" ", "moonshine-tiny"),
      ("ctrl+option+r", "whisper-turbo"),
      ("Control+Alt+R", "parakeet-v3"),
      ("7", ""),
    ]);
    let keys: Vec<&str> = s.model_keys().into_iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["Z", "f1", "ctrl+option+r"]);
  }

  /// A letter or digit is held with a hotkey, another key of nimbo's grammar is a hotkey of its
  /// own, numbered after the main one; one that is neither shifts no number.
  #[test]
  fn model_keys_are_held_or_hotkeys_of_their_own() {
    let s = with_keys(&[
      ("Z", "gigaam-v3"),
      ("f1", "parakeet-v2"),
      ("no such key", "parakeet-v3"),
      ("7", "whisper-turbo"),
      ("ctrl+option+r", "moonshine-tiny"),
    ]);
    let held: Vec<Option<Binding>> = s.held_keys().into_iter().map(ModelKey::binding).collect();
    assert_eq!(held, [Some(Binding::Held('z')), Some(Binding::Held('7'))]);
    assert_eq!((s.held_key_model(1), s.held_key_model(2)), (Some("whisper-turbo"), None));
    let hotkeys: Vec<Option<&str>> = (0..4).map(|which| s.hotkey_model(which)).collect();
    assert_eq!(hotkeys, [None, Some("parakeet-v2"), Some("moonshine-tiny"), None]);
  }

  /// A hotkey of its own is one that types nothing: a key alone or with ⇧ is none.
  #[test]
  fn a_hotkey_of_its_own_takes_no_typing() {
    let binding = |key: &str| ModelKey { key: key.into(), model: "m".into() }.binding();
    for key in ["f1", "shift+f5", "fn+1", "option+z", "ctrl+cmd+r", "right_option"] {
      assert!(matches!(binding(key), Some(Binding::Own(_))), "{key}");
    }

    for key in ["space", "shift+z", "return", "left", ""] {
      assert_eq!(binding(key), None, "{key}");
    }
  }

  /// A key field's key: a letter or digit, or a hotkey of its own; not the main shortcut nor its
  /// key.
  #[test]
  fn a_recorded_key_is_held_or_its_own_but_not_the_shortcuts() {
    let hk = |text: &str| Hotkey::parse(text).unwrap();
    let key = |text: &str, main: &str| model_key(hk(text), main);
    assert_eq!(key("z", "fn"), Ok("z".into()));
    assert_eq!(key("7", "option+space"), Ok("7".into()));
    assert_eq!(key("f1", "fn"), Ok("f1".into()));
    assert_eq!(key("ctrl+option+r", "fn"), Ok("ctrl+option+r".into()));
    let label = |text: &str| hk(text).label();
    assert_eq!(key("f5", "f5"), Err(format!("{} is the shortcut", label("f5"))));
    assert_eq!(key("space", "option+space"), Err(format!("{} is in the shortcut", label("space"))));
    assert_eq!(key("z", "ctrl+z"), Err(format!("{} is in the shortcut", label("z"))));
    assert_eq!(key("shift+z", "fn"), Err(format!("{} can’t be a key", label("shift+z"))));
  }

  /// A key set moves from the model that had it; one cleared goes; the labels follow.
  #[test]
  fn a_key_moves_to_the_model_it_is_set_for() {
    let mut s = with_keys(&[("z", "gigaam-v3"), ("f1", "parakeet-v2")]);
    s.set_model_key("whisper-turbo", Some("Z".into()));
    assert_eq!(s.model_key("gigaam-v3"), None);
    assert_eq!(s.model_key("whisper-turbo"), Some(Binding::Held('z')));
    s.set_model_key("parakeet-v2", Some("x".into()));
    let labels = s.key_labels();
    assert_eq!(labels, [("whisper-turbo", "Z".into()), ("parakeet-v2", "X".into())]);
    s.set_model_key("whisper-turbo", None);
    assert_eq!(s.keys, [ModelKey { key: "x".into(), model: "parakeet-v2".into() }]);
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
