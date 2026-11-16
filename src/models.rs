// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What Settings › Models knows about wavo's models: which ones to show for a language, where
//! they stand in a list, which one to recommend and how its numbers read. Nothing here draws:
//! the page only shows what these answer, and the tests read them without a window.

use std::sync::LazyLock;

use nimbo::{Localized, t, tn};
use wavo::hub::{self, ModelInfo};

use crate::history::fold;
use crate::settings::{english_name, language_name};

/// The model to start with, wavo's own default: first in Available, its Download the blue one.
pub const RECOMMENDED: &str = "parakeet-ultra";

/// Every language of wavo's models by its name in the app's language, after "All languages":
/// `(label, code)`.
pub fn languages() -> Vec<(String, &'static str)> {
  static CODES: LazyLock<Vec<&str>> = LazyLock::new(|| {
    let mut codes: Vec<&str> = hub::MODELS.iter().flat_map(|m| m.langs.iter().copied()).collect();
    codes.sort_unstable();
    codes.dedup();
    codes
  });
  let mut all: Vec<(String, &str)> = CODES.iter().map(|&c| (language_name(c), c)).collect();
  all.sort();
  all.insert(0, (t!("All languages").to_string(), ""));
  all
}

/// Every word of `search` is in the model's names, maker or languages, in any case.
pub fn finds(search: &str, m: &ModelInfo) -> bool {
  let search = fold(search);
  if search.trim().is_empty() {
    return true;
  }

  // In the app's language and in English, as the models' own descriptions are.
  let names = |c: &&str| [language_name(c), english_name(c)];
  let langs: Vec<String> = m.langs.iter().flat_map(names).collect();
  let text =
    [m.full_name, m.name, m.family, m.maker, m.languages].join(" ") + " " + &langs.join(" ");
  let text = fold(&text);
  search.split_whitespace().all(|w| text.contains(w))
}

/// Where a model goes in a list of Settings › Models, the least first: `best`, then the measured
/// ones by [measure], then the rest, with a language chosen those made for it alone first.
pub fn rank(lang: &str, best: Option<&str>, m: &ModelInfo) -> (bool, bool, i32, bool) {
  let by = measure(lang, m);
  let at = by.map_or(0, |x| (x * 100.0).round() as i32);
  (Some(m.name) != best, by.is_none(), at, !lang.is_empty() && m.langs != [lang])
}

/// What orders a measured model, the least first: its errors in `lang`, or with all languages
/// its speed, the fastest first. `None` when not measured.
pub fn measure(lang: &str, m: &ModelInfo) -> Option<f32> {
  if lang.is_empty() { m.speed().map(|x| -x) } else { m.wer(lang) }
}

/// A model at least this many times faster than whisper-turbo is fast.
const FAST: f32 = 3.0;

/// The most errors in 100 words (characters where wavo counts those) a model may make in a
/// language and still be usable in it: with more, a person fixes about every fourth word and
/// would rather type. Settings › Models never recommends a model above it, and folds those away.
pub const USABLE: f32 = 25.0;

/// Whether `m` was measured in `lang` and makes too many errors there to be usable ([USABLE]).
pub fn unusable(lang: &str, m: &ModelInfo) -> bool {
  m.wer(lang).is_some_and(|x| x > USABLE)
}

/// Whether `m`, which writes lowercase without punctuation, counts in `lang`: it is usable there
/// and no model with punctuation is, or the best of those makes twice its errors, as in Kazakh,
/// Kyrgyz and Uzbek. A text to fix beats none; where a good one punctuates, it stays hidden.
pub fn plain_fits(lang: &str, m: &ModelInfo) -> bool {
  let Some(wer) = m.wer(lang).filter(|&x| x <= USABLE) else { return false };
  let punctuated = hub::MODELS.iter().filter(|m| m.punctuated).filter_map(|m| m.wer(lang));
  punctuated.min_by(f32::total_cmp).is_none_or(|best| best > USABLE || best >= wer * 2.0)
}

/// Whether models were measured in `lang` but none of them is usable there ([USABLE]).
pub fn none_usable(lang: &str) -> bool {
  let measured = hub::MODELS.iter().any(|m| m.wer(lang).is_some());
  measured && recommended(lang).is_none()
}

/// The model to recommend for `lang` by wavo's measures: the fast one with the fewest errors, or
/// the fastest within 10% of it; a slow one only with 1.5 times fewer errors. `None` when none
/// was measured on it, or none of those is usable in it ([USABLE]).
pub fn recommended(lang: &str) -> Option<&'static ModelInfo> {
  let measured: Vec<(&'static ModelInfo, f32, f32)> = (hub::MODELS.iter())
    .filter(|m| m.punctuated || plain_fits(lang, m))
    .filter_map(|m| Some((m, m.wer(lang).filter(|&x| x <= USABLE)?, m.speed().unwrap_or(0.0))))
    .collect();
  let least = |fast: bool| {
    let them = measured.iter().filter(move |x| (x.2 >= FAST) == fast);
    them.min_by(|a, b| a.1.total_cmp(&b.1))
  };
  let Some(&(_, wer, _)) = least(true) else {
    return least(false).map(|x| x.0);
  };

  if let Some(slow) = least(false).filter(|x| x.1 * 1.5 <= wer) {
    return Some(slow.0);
  }

  let near = measured.iter().filter(|x| x.2 >= FAST && x.1 <= wer * 1.1);
  near.max_by(|a, b| a.2.total_cmp(&b.2)).map(|x| x.0)
}

/// `WER 4.5%` of `m` in `lang`, a decimal under 10 (`WER 32%`); `None` when not measured.
pub fn wer_text(lang: &str, m: &ModelInfo) -> Option<String> {
  Some(t!("WER {wer}%", wer = figure(m.wer(lang)?)))
}

/// Languages written without spaces, whose WER wavo counts in characters: `BY_CHARACTER` in
/// wavo's `scripts/wer.py`, which makes `src/hub/scores.rs` (the crate doesn't say it).
const BY_CHARACTER: [&str; 7] = ["zh", "yue", "ja", "th", "lo", "my", "km"];

/// What a WER in `lang` counts, on its hover.
pub fn errors_per(lang: &str) -> Localized {
  match BY_CHARACTER.contains(&lang) {
    true => t!("Errors per 100 characters"),
    false => t!("Errors per 100 words"),
  }
}

/// A WER or a speed as Settings › Models shows it: a decimal under 10 (`4.5`), whole from it
/// (`32`; 9.96 is `10`).
pub fn figure(x: f32) -> String {
  let tenths = (x * 10.0).round() / 10.0;
  if tenths < 10.0 { format!("{tenths:.1}") } else { format!("{:.0}", x.round()) }
}

/// The model Settings › Models recommends for `lang` and why: [recommended], else whisper-turbo
/// where it takes the language and wasn't measured on it, the best general model; [RECOMMENDED]
/// for all languages.
pub fn best_for(lang: &str) -> Option<(&'static str, String)> {
  let name = language_name(lang);
  match (lang, recommended(lang)) {
    ("", _) => Some((RECOMMENDED, t!("The default model").to_string())),
    (_, Some(m)) if !m.punctuated => {
      let why = t!("The fewest errors in {lang}, but lowercase without punctuation", lang = name);
      Some((m.name, why))
    }
    (_, Some(m)) => Some((m.name, t!("Best balance of accuracy and speed in {lang}", lang = name))),
    _ => (hub::lookup("whisper-turbo").ok())
      .filter(|m| m.langs.contains(&lang) && m.wer(lang).is_none())
      .map(|m| {
        (m.name, t!("No accuracy data for {lang} yet: the best general model", lang = name))
      }),
  }
}

/// `1 model`, `3 models`.
pub fn model_count(n: usize) -> String {
  tn!("{n} model", "{n} models", n)
}

/// A speed as times whisper-turbo's: `22×`, a decimal under 10 (`8.5×`, `0.9×`).
pub fn times(x: f32) -> String {
  format!("{}×", figure(x))
}

/// What [times] says on hover: `22× faster than whisper-turbo`, `As fast as whisper-turbo` when
/// it shows `1.0×`; `None` when not measured.
pub fn speed_text(m: &ModelInfo) -> Option<String> {
  let x = m.speed()?;
  let times = times(x);
  Some(match m.name {
    "whisper-turbo" => t!("The speed baseline ({times})", times),
    _ if times == "1.0×" => t!("As fast as whisper-turbo").to_string(),
    _ if x < 1.0 => t!("{times} the speed of whisper-turbo", times),
    _ => t!("{times} faster than whisper-turbo", times),
  })
}

/// What wavo says of the languages of its models, to translate: [model_languages].
pub const MODEL_LANGUAGES: [Localized; 9] = [
  // Translators: the languages a speech model knows.
  t!("100 languages"),
  t!("25 European languages"),
  t!("99 languages"),
  t!("English"),
  t!("Hebrew"),
  t!("Russian"),
  t!("Russian, English, Kazakh, Kyrgyz and Uzbek"),
  t!("Taiwanese Mandarin and English"),
  t!("Ukrainian and English"),
];

/// The languages `m` knows as wavo says them, in the app's language: one wavo adds later in
/// English until it is in [MODEL_LANGUAGES].
pub fn model_languages(m: &ModelInfo) -> &'static str {
  let known = MODEL_LANGUAGES.iter().find(|l| l.key() == m.languages);
  known.map_or(m.languages, |l| l.as_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The recommended model first, the measured ones by their errors, then the rest, those made
  /// for the language alone first.
  #[test]
  fn a_language_lists_its_pick_then_its_own_models_first() {
    let order = |lang: &str| {
      let best = recommended(lang).map(|m| m.name);
      let mut models: Vec<&ModelInfo> =
        hub::MODELS.iter().filter(|m| m.langs.contains(&lang)).collect();
      models.sort_by_key(|m| rank(lang, best, m));
      models.into_iter().map(|m| m.name).collect::<Vec<_>>()
    };
    assert_eq!(order("he")[..3], ["parakeet-he", "whisper-turbo-he", "whisper-turbo"]);
    let en = ["parakeet-v2", RECOMMENDED, "whisper-large-v3", "breeze-asr-25", "parakeet-uk"];
    assert_eq!(order("en")[..6], [&en[..], &["whisper-turbo"]].concat());
    let ru = ["gigaam-v3-e2e-ctc", "whisper-large-v3", "gigaam-v3", "whisper-turbo", RECOMMENDED];
    assert_eq!(order("ru")[..5], ru);

    // On a tie, the one made for the language alone first, wherever it is in wavo's table.
    let all = *hub::lookup("whisper-turbo").unwrap();
    let mut he = all;
    he.langs = &["he"];
    let mut tie = [&all, &he];
    tie.sort_by_key(|m| rank("he", None, m));
    assert_eq!(tie.map(|m| m.langs.len() == 1), [true, false]);
  }

  /// The fast model with the fewest errors, the fastest of those near it; a slow one only when
  /// far better, as whisper-turbo in Greek; never one with too many errors, none in Bengali.
  #[test]
  fn the_recommended_model_is_fast_and_accurate() {
    let cases = [
      ("ru", "gigaam-v3-e2e-ctc"),
      ("en", "parakeet-v2"),
      ("sv", "parakeet-ultra"),
      ("el", "whisper-turbo"),
      ("he", "parakeet-he"),
    ];
    for (lang, name) in cases {
      assert_eq!(recommended(lang).map(|m| m.name), Some(name), "{lang}");
    }

    assert_eq!(recommended(""), None);

    // With none measured, whisper-turbo where it takes the language.
    let pick = |lang: &str| best_for(lang).map(|p| p.0);
    assert_eq!(
      (pick(""), pick("ru"), pick("sq"), pick("xx")),
      (Some(RECOMMENDED), Some("gigaam-v3-e2e-ctc"), Some("whisper-turbo"), None)
    );
    let sq = best_for("sq").unwrap().1;
    assert_eq!(sq, "No accuracy data for Albanian yet: the best general model");
    assert_eq!(recommended("sq"), None, "none measured");
    // None in Georgian or Bengali, where every model measured has too many errors.
    for lang in ["ka", "bn"] {
      assert_eq!(pick(lang), None, "{lang}: none usable");
      assert!(none_usable(lang), "{lang}");
      assert!(hub::lookup("whisper-turbo").unwrap().wer(lang).is_some_and(|x| x > USABLE));
    }

    assert!(!none_usable("sq") && !none_usable("ru") && !none_usable(""));
    // Never one above the cutoff, in any language.
    for (_, lang) in &languages() {
      assert!(recommended(lang).is_none_or(|m| m.wer(lang).unwrap() <= USABLE), "{lang}");
    }
  }

  /// WER to a decimal under 10 (rounded first); bolts and the speed's words, whisper-turbo the
  /// baseline; an unpunctuated model recommended only where nothing better is.
  #[test]
  fn models_say_their_errors_and_speed() {
    let m = |name: &str| hub::lookup(name).unwrap();
    assert_eq!(wer_text("ru", m("gigaam-v3")).as_deref(), Some("WER 4.5%"));
    assert_eq!(wer_text("el", m("parakeet-ultra")).as_deref(), Some("WER 32%"));
    assert_eq!(wer_text("", m("parakeet-ultra")), None, "all languages");
    assert_eq!([9.94, 9.96, 31.6].map(figure), ["9.9", "10", "32"]);
    for (_, lang) in &languages() {
      let plain = recommended(lang).filter(|m| !m.punctuated).map(|m| m.name);
      let only = ["kk", "ky", "uz"].contains(lang);
      assert_eq!(plain, only.then_some("gigaam-multilingual"), "{lang}");
    }

    assert_eq!([22.1, 9.96, 8.5, 0.9].map(times), ["22×", "10×", "8.5×", "0.9×"]);
    let speed = |name| speed_text(m(name)).unwrap();
    assert_eq!(speed("gigaam-v3"), "13× faster than whisper-turbo");
    assert_eq!(speed("parakeet-ultra"), "6.7× faster than whisper-turbo");
    assert_eq!(speed("whisper-turbo"), "The speed baseline (1.0×)");
    assert_eq!(speed("whisper-turbo-he"), "As fast as whisper-turbo");
    assert_eq!(speed("whisper-medium"), "0.9× the speed of whisper-turbo");
    let mut unmeasured = *m("whisper-tiny");
    unmeasured.name = "unmeasured";
    assert_eq!(speed_text(&unmeasured), None);
  }
}
