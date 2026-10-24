// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Languages by the codes wavo uses for them.

use isolang::Language;

/// The English name of a language code wavo uses: ISO 639-1 (`en`), else 639-3 (`haw`), and
/// Whisper's own `jw` for Javanese; an unknown code is its own name.
pub fn name(code: &str) -> String {
  let language = match code {
    "jw" => Some(Language::Jav),
    _ => Language::from_639_1(code).or_else(|| Language::from_639_3(code)),
  };

  language.map_or_else(|| code.to_owned(), |l| l.to_name().to_owned())
}

#[cfg(test)]
mod tests {
  use wavo::hub::MODELS;

  use super::*;

  #[test]
  fn every_language_of_wavos_models_has_a_name() {
    let names =
      [("en", "English"), ("haw", "Hawaiian"), ("jw", "Javanese"), ("yue", "Yue Chinese")];
    for (code, expected) in names {
      assert_eq!(name(code), expected);
    }

    for code in MODELS.iter().flat_map(|m| m.langs) {
      assert_ne!(name(code), *code, "{code} has no name");
    }

    assert_eq!(name("xx"), "xx");
  }
}
