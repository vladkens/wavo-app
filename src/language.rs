// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Languages by the codes wavo's models list them under ([`ModelInfo::langs`]): ISO 639-1 where
//! one exists (`en`), else ISO 639-3 (`haw`); Whisper keeps its own `jw` for Javanese.
//!
//! [`ModelInfo::langs`]: wavo::hub::ModelInfo::langs

use isolang::Language;

/// The language of `code`, any case.
fn language(code: &str) -> Option<Language> {
  let code = code.to_ascii_lowercase();
  let code = if code == "jw" { "jv" } else { code.as_str() };
  Language::from_639_1(code).or_else(|| Language::from_639_3(code))
}

/// A language's English name by its code (`ru`: `Russian`); an unknown code as it is.
pub fn name(code: &str) -> String {
  language(code).map_or_else(|| code.to_owned(), |l| l.to_name().to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn names_by_either_code() {
    assert_eq!(name("ru"), "Russian");
    assert_eq!(name("EN"), "English");
    assert_eq!(name("haw"), "Hawaiian");
    assert_eq!(name("jw"), "Javanese");
    assert_eq!(name("xx"), "xx");
  }
}
