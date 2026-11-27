// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `settings.json` in the data home: a missing file or field is the default; a file that does not
//! parse is copied to `settings.json.bad` and the defaults are used.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use isolang::Language;
use nimbo::{Locale, Localized, t};
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
  /// A language code, `auto` for the model to detect it.
  pub language: String,
  /// A model name or a GGUF file path; `None` is the first model found.
  pub model: Option<String>,
  /// The languages [Settings::switch_hotkey] goes through in turn, each with its model: the
  /// current one is [Settings::language] with [Settings::model]. None, or two and more.
  pub languages: Vec<Line>,
  /// The hotkey that moves to the next of [Settings::languages]; empty for none.
  pub switch_hotkey: String,
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
  /// The HUD shows the text while the person speaks ([crate::live]).
  pub live_preview: bool,
  /// After each dictation the HUD says how long the text took: a debugging aid.
  pub show_transcription_time: bool,
  /// Wayland's GlobalShortcuts portal took the hotkey once: later launches bind it without a click
  /// (the desktop remembers the person's answer).
  pub hotkey_portal: bool,
  /// Check for a new version at launch and once a day, and offer it in the menu (a release only).
  pub auto_update: bool,
  /// The version Skip This Version passed over: automatic checks don't offer it.
  pub skipped_update: Option<String>,
  /// The language of Wavo's own texts, a tag of [crate::LANGUAGES] (`ru`); `None` is the
  /// system's.
  pub app_language: Option<String>,
}

/// One of [Settings::languages].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Line {
  pub language: String,
  pub model: Option<String>,
}

impl Line {
  /// The language's name, Auto-detect for `auto`.
  pub fn name(&self) -> String {
    match self.language.as_str() {
      "auto" => t!("Auto-detect").to_string(),
      lang => language_name(lang),
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

/// The HUD's pill under a dictation: the icon's colours flowing, or the plain pill of the system's
/// appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudStyle {
  #[default]
  Gradient,
  Plain,
}

/// The text while speaking is on by default where the model keeps up with it: on a Mac, or with a
/// discrete GPU; an integrated or software one would be busy all the time while speaking. Probed
/// once: wavo reads the adapters' info only, in milliseconds.
fn live_default() -> bool {
  static FAST: OnceLock<bool> = OnceLock::new();
  *FAST.get_or_init(|| {
    cfg!(target_os = "macos")
      || wavo::devices()
        .is_ok_and(|d| d.first().is_some_and(|d| d.kind == wavo::DeviceKind::DiscreteGpu))
  })
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
      languages: Vec::new(),
      switch_hotkey: String::new(),
      auto_paste: true,
      keep_in_clipboard: false,
      clean_text: true,
      unload_after_minutes: Some(15),
      neural_engine: true,
      history_retention: Retention::Unlimited,
      autostart: false,
      hud_position: HudPosition::Bottom,
      hud_style: HudStyle::Gradient,
      live_preview: live_default(),
      show_transcription_time: false,
      hotkey_portal: false,
      auto_update: true,
      skipped_update: None,
      app_language: None,
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
        let moved = fs::rename(path, &bad);
        let (path, kept) = (path.display(), bad.display());
        let warning = match moved {
          Ok(()) => t!("{path}: {e}; using the defaults, the file kept as {kept}", path, e, kept),
          Err(_) => t!("{path}: {e}; using the defaults", path, e),
        };
        return (Self::default(), Some(warning));
      }
    };

    let value = serde_json::from_slice::<serde_json::Value>(&bytes);
    match value.and_then(|v| Ok((Self::deserialize(&v)?, v))) {
      Ok((mut settings, value)) => {
        settings.migrate(&value);
        (settings, None)
      }
      Err(e) => {
        let bad = path.with_extension("json.bad");
        let _ = fs::write(&bad, &bytes);
        let (path, kept) = (path.display(), bad.display());
        let warning =
          t!("{path} is invalid ({e}); using the defaults, the file kept as {kept}", path, e, kept);
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

  /// What v0.3.0 stored as Keyboard layout (`"language": "keyboard"` and `keyboard_models`, the
  /// model of each layout's language, none for `model`'s): its languages to switch between, the
  /// one of `model` current.
  fn migrate(&mut self, value: &serde_json::Value) {
    if self.language != "keyboard" {
      return;
    }

    let rows = value.get("keyboard_models").and_then(|v| v.as_object());
    let lines = rows.into_iter().flatten().map(|(language, model)| Line {
      language: language.clone(),
      model: model.as_str().map(str::to_owned).or_else(|| self.model.clone()),
    });
    self.languages = lines.collect();
    let current = self.languages.iter().position(|l| l.model == self.model).unwrap_or(0);
    match self.languages.get(current).cloned() {
      Some(line) => (self.language, self.model) = (line.language, line.model),
      None => self.language = "auto".into(),
    }

    if self.languages.len() < 2 {
      self.languages.clear();
    }
  }

  /// The language chosen for the engine; `None` (empty or `auto`) leaves it to the model.
  pub fn engine_language(&self) -> Option<String> {
    let code = self.language.trim();
    let chosen = !code.is_empty() && !code.eq_ignore_ascii_case("auto");
    chosen.then(|| code.to_owned())
  }

  /// The model choice and the language of a dictation now.
  pub fn route(&self) -> (Option<String>, Option<String>) {
    (self.model.clone(), self.engine_language())
  }

  /// The hotkeys by nimbo's place: the dictation's, then the switch's while there are languages
  /// to switch between; empty for none.
  pub fn hotkeys(&self) -> [&str; 2] {
    let switch = if self.languages.is_empty() { "" } else { self.switch_hotkey.as_str() };
    [&self.hotkey, switch]
  }

  /// Makes the next of [Settings::languages] current, the first after the last or after a
  /// language not among them; `false` with none.
  pub fn next_language(&mut self) -> bool {
    let Some(first) = self.languages.first() else { return false };
    let at = self.languages.iter().position(|l| l.language == self.language);
    let next = at.and_then(|i| self.languages.get(i + 1)).unwrap_or(first).clone();
    (self.language, self.model) = (next.language, next.model);
    true
  }

  /// `language` current, with its line's model when it has one.
  pub fn choose_language(&mut self, language: &str) {
    self.language = language.to_owned();
    if let Some(line) = self.languages.iter().find(|l| l.language == language) {
      self.model = line.model.clone();
    }
  }

  /// `model` current, and the model of the current language's line.
  pub fn choose_model(&mut self, model: &str) {
    self.model = Some(model.to_owned());
    let current = self.languages.iter_mut().find(|l| l.language == self.language);
    if let Some(line) = current {
      line.model = self.model.clone();
    }
  }

  /// The language of the app's texts: [Settings::app_language], the system's when it is none of
  /// [crate::LANGUAGES].
  pub fn locale(&self) -> Locale {
    let tag = self.app_language.as_deref();
    let known = crate::LANGUAGES.iter().find(|l| Some(l.0) == tag);
    known.map_or(Locale::System, |l| Locale::Tag(l.0))
  }

  /// [Settings::locale] of the file at `path`, read without a word: [Settings::load]'s warning
  /// and its copy aside are the session's.
  pub fn locale_at(path: &Path) -> Locale {
    let settings = fs::read(path).ok().and_then(|b| serde_json::from_slice::<Self>(&b).ok());
    settings.unwrap_or_default().locale()
  }

  pub fn unload_after(&self) -> Option<Duration> {
    self.unload_after_minutes.map(|m| Duration::from_secs(u64::from(m) * 60))
  }
}

// MARK: Languages

/// A language's name in the app's language by the code wavo's models list it under (`ru`:
/// `Russian`): ISO 639-1, else 639-3, and Whisper's `jw` for Javanese; a code the table misses by
/// its English name, an unknown one as it is.
pub fn language_name(code: &str) -> String {
  language(code).map_or_else(|| english_name(code), |l| l.as_str().to_owned())
}

/// A language's English name by its code, as [language_name] takes it: for a search in either.
pub fn english_name(code: &str) -> String {
  if let Some(l) = language(code) {
    return l.key().to_owned();
  }

  let lower = code.to_ascii_lowercase();
  let language = Language::from_639_1(&lower).or_else(|| Language::from_639_3(&lower));
  language.map_or_else(|| code.to_owned(), |l| l.to_name().to_owned())
}

/// The name of the language `code` (any case, `jw` for `jv`) to translate.
fn language(code: &str) -> Option<Localized> {
  let lower = code.to_ascii_lowercase();
  let lower = if lower == "jw" { "jv" } else { lower.as_str() };
  NAMES.iter().find(|n| n.0 == lower).map(|n| n.1)
}

/// The names of the languages of wavo's models, in English as isolang names them.
const NAMES: [(&str, Localized); 101] = [
  // Translators: a language, as a list of languages names it.
  ("af", t!("Afrikaans")),
  ("am", t!("Amharic")),
  ("ar", t!("Arabic")),
  ("as", t!("Assamese")),
  ("az", t!("Azerbaijani")),
  ("ba", t!("Bashkir")),
  ("be", t!("Belarusian")),
  ("bg", t!("Bulgarian")),
  ("bn", t!("Bengali")),
  ("bo", t!("Tibetan")),
  ("br", t!("Breton")),
  ("bs", t!("Bosnian")),
  ("ca", t!("Catalan")),
  ("cs", t!("Czech")),
  ("cy", t!("Welsh")),
  ("da", t!("Danish")),
  ("de", t!("German")),
  ("el", t!("Modern Greek")),
  ("en", t!("English")),
  ("es", t!("Spanish")),
  ("et", t!("Estonian")),
  ("eu", t!("Basque")),
  ("fa", t!("Persian")),
  ("fi", t!("Finnish")),
  ("fo", t!("Faroese")),
  ("fr", t!("French")),
  ("gl", t!("Galician")),
  ("gu", t!("Gujarati")),
  ("ha", t!("Hausa")),
  ("haw", t!("Hawaiian")),
  ("he", t!("Hebrew")),
  ("hi", t!("Hindi")),
  ("hr", t!("Croatian")),
  ("ht", t!("Haitian")),
  ("hu", t!("Hungarian")),
  ("hy", t!("Armenian")),
  ("id", t!("Indonesian")),
  ("is", t!("Icelandic")),
  ("it", t!("Italian")),
  ("ja", t!("Japanese")),
  ("jv", t!("Javanese")),
  ("ka", t!("Georgian")),
  ("kk", t!("Kazakh")),
  ("km", t!("Khmer")),
  ("kn", t!("Kannada")),
  ("ko", t!("Korean")),
  ("ky", t!("Kirghiz")),
  ("la", t!("Latin")),
  ("lb", t!("Luxembourgish")),
  ("ln", t!("Lingala")),
  ("lo", t!("Lao")),
  ("lt", t!("Lithuanian")),
  ("lv", t!("Latvian")),
  ("mg", t!("Malagasy")),
  ("mi", t!("Maori")),
  ("mk", t!("Macedonian")),
  ("ml", t!("Malayalam")),
  ("mn", t!("Mongolian")),
  ("mr", t!("Marathi")),
  ("ms", t!("Malay")),
  ("mt", t!("Maltese")),
  ("my", t!("Burmese")),
  ("ne", t!("Nepali")),
  ("nl", t!("Dutch")),
  ("nn", t!("Norwegian Nynorsk")),
  ("no", t!("Norwegian")),
  ("oc", t!("Occitan")),
  ("pa", t!("Panjabi")),
  ("pl", t!("Polish")),
  ("ps", t!("Pushto")),
  ("pt", t!("Portuguese")),
  ("ro", t!("Romanian")),
  ("ru", t!("Russian")),
  ("sa", t!("Sanskrit")),
  ("sd", t!("Sindhi")),
  ("si", t!("Sinhala")),
  ("sk", t!("Slovak")),
  ("sl", t!("Slovenian")),
  ("sn", t!("Shona")),
  ("so", t!("Somali")),
  ("sq", t!("Albanian")),
  ("sr", t!("Serbian")),
  ("su", t!("Sundanese")),
  ("sv", t!("Swedish")),
  ("sw", t!("Swahili")),
  ("ta", t!("Tamil")),
  ("te", t!("Telugu")),
  ("tg", t!("Tajik")),
  ("th", t!("Thai")),
  ("tk", t!("Turkmen")),
  ("tl", t!("Tagalog")),
  ("tr", t!("Turkish")),
  ("tt", t!("Tatar")),
  ("uk", t!("Ukrainian")),
  ("ur", t!("Urdu")),
  ("uz", t!("Uzbek")),
  ("vi", t!("Vietnamese")),
  ("yi", t!("Yiddish")),
  ("yo", t!("Yoruba")),
  ("yue", t!("Yue Chinese")),
  ("zh", t!("Chinese")),
];

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

  /// The switch goes round the languages, each with its model; choosing a model or a language
  /// keeps the line in step.
  #[test]
  fn the_switch_goes_round_the_languages() {
    let line =
      |language: &str, model: &str| Line { language: language.into(), model: Some(model.into()) };
    let mut s = Settings {
      language: "ru".into(),
      model: Some("gigaam-v3".into()),
      languages: vec![
        line("ru", "gigaam-v3"),
        line("en", "parakeet-v2"),
        line("de", "parakeet-v3"),
      ],
      switch_hotkey: "f6".into(),
      ..Settings::default()
    };
    let now = |s: &Settings| (s.language.clone(), s.model.clone().unwrap_or_default());
    let pair = |l: &str, m: &str| (l.to_owned(), m.to_owned());
    assert_eq!(s.hotkeys(), [DEFAULT_HOTKEY, "f6"]);
    assert!(s.next_language());
    assert_eq!(now(&s), pair("en", "parakeet-v2"));
    assert_eq!(s.route(), (Some("parakeet-v2".into()), Some("en".into())));
    s.next_language();
    s.next_language();
    assert_eq!(now(&s), pair("ru", "gigaam-v3"), "after the last, the first");

    s.choose_language("de");
    assert_eq!(now(&s), pair("de", "parakeet-v3"));
    s.choose_model("parakeet-ultra");
    assert_eq!(s.languages[2], line("de", "parakeet-ultra"));
    s.choose_language("fr");
    assert_eq!(now(&s), pair("fr", "parakeet-ultra"), "a language of none keeps the model");
    s.choose_model("parakeet-v3");
    assert_eq!(s.languages[2], line("de", "parakeet-ultra"), "no line of it");
    s.next_language();
    assert_eq!(now(&s), pair("ru", "gigaam-v3"), "the first after one of none");

    let mut none = Settings { switch_hotkey: "f6".into(), ..Settings::default() };
    assert!(!none.next_language());
    assert_eq!(none.hotkeys(), [DEFAULT_HOTKEY, ""], "no switch without languages");
  }

  /// v0.3.0's Keyboard layout: its rows to switch between, the one of the Model current.
  #[test]
  fn keyboard_layout_becomes_the_languages() {
    let dir = TempDir::new("settings-keyboard");
    let path = dir.path().join(FILE);
    let load = |json: &str| {
      fs::write(&path, json).unwrap();
      let s = Settings::load(&path).0;
      let lines: Vec<_> =
        s.languages.iter().map(|l| (l.language.as_str(), l.model.as_deref())).collect();
      (s.language.clone(), s.model.clone(), format!("{lines:?}"))
    };
    let some = |s: &str| Some(s.to_owned());
    let rows = r#"{"language": "keyboard", "model": "parakeet-v2",
      "keyboard_models": {"en": null, "ru": "gigaam-v3"}}"#;
    let want = r#"[("en", Some("parakeet-v2")), ("ru", Some("gigaam-v3"))]"#;
    assert_eq!(load(rows), ("en".into(), some("parakeet-v2"), want.into()));
    let other = r#"{"language": "keyboard", "model": "parakeet-v3",
      "keyboard_models": {"en": "parakeet-v2", "ru": "gigaam-v3"}}"#;
    let want = r#"[("en", Some("parakeet-v2")), ("ru", Some("gigaam-v3"))]"#;
    assert_eq!(load(other), ("en".into(), some("parakeet-v2"), want.into()), "none of the Model");
    let one =
      r#"{"language": "keyboard", "model": "parakeet-v3", "keyboard_models": {"ru": "gigaam-v3"}}"#;
    assert_eq!(load(one), ("ru".into(), some("gigaam-v3"), "[]".into()), "one is no switch");
    let empty = r#"{"language": "keyboard", "model": "parakeet-v3"}"#;
    assert_eq!(load(empty), ("auto".into(), some("parakeet-v3"), "[]".into()));
    let plain = r#"{"language": "ru", "model": "gigaam-v3", "keyboard_models": {"en": null}}"#;
    assert_eq!(load(plain), ("ru".into(), some("gigaam-v3"), "[]".into()), "a retired field");

    let s = Settings::load(&path).0;
    s.save(&path).unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains("keyboard"));
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

  /// The app's language: one of [crate::LANGUAGES] by its tag, else the system's.
  #[test]
  fn the_locale_is_the_app_language_set() {
    let locale = |tag: Option<&str>| {
      Settings { app_language: tag.map(str::to_owned), ..Settings::default() }.locale()
    };
    assert_eq!(locale(None), Locale::System);
    assert_eq!(locale(Some("ru")), Locale::Tag("ru"));
    assert_eq!(locale(Some("en")), Locale::Tag("en"));
    assert_eq!(locale(Some("xx")), Locale::System, "a language the app has no catalog for");
    let dir = crate::testing::TempDir::new("settings-locale");
    let path = dir.path().join(FILE);
    assert_eq!(Settings::locale_at(&path), Locale::System, "no file");
    fs::write(&path, r#"{"app_language": "ru"}"#).unwrap();
    assert_eq!(Settings::locale_at(&path), Locale::Tag("ru"));
  }

  #[test]
  fn every_language_of_wavos_models_has_a_name() {
    assert_eq!((language_name("EN"), language_name("jw")), ("English".into(), "Javanese".into()));
    for code in wavo::hub::MODELS.iter().flat_map(|m| m.langs) {
      assert!(language(code).is_some(), "{code} has no name to translate");
    }

    // The English names are isolang's, which named them before.
    for (code, name) in NAMES {
      let language = Language::from_639_1(code).or_else(|| Language::from_639_3(code));
      assert_eq!(language.map(|l| l.to_name()), Some(name.key()), "{code}");
    }
  }
}
