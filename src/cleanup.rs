// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What changes in a dictated text on its way to the person: hesitation sounds go, and a text that
//! goes on a sentence the paste before it left open starts in lower case (`ends_sentence`,
//! `going_on`). A filler left in is the cheap mistake and a real word removed the expensive one,
//! so whatever is in doubt stays as it is.

/// The hesitation sounds of a language, in lower case. Not one list for all: `um` is a German and
/// Portuguese word, `мм` a unit. No bare `э`: it is a letter, an initial and a building's name; no
/// `эм` for the same reason: `буква эм`, `Эм Ватсон`.
const FILLERS: [(&str, &[&str]); 2] = [
  ("ru", &["ээ", "э-э", "э-э-э", "ммм", "хм"]),
  ("en", &["uh", "uhh", "um", "umm", "uhm", "er", "erm", "hmm", "hm", "mmm"]),
];

/// What ends a sentence.
const ENDS: [char; 4] = ['.', '?', '!', '…'];
/// What a sentence goes on after, and a filler passes to the word before it.
const PAUSES: [char; 2] = [':', ';'];
/// What goes with a filler after it. The same mark before it stays: it may be a real one.
const LIGHT: [char; 4] = [',', '—', '–', '-'];

fn is_mark(c: char) -> bool {
  ENDS.contains(&c) || PAUSES.contains(&c) || LIGHT.contains(&c)
}

/// `text` without what doesn't belong in it. `language` is a model's code, `None` when it isn't
/// known: only a language with a table changes.
pub fn clean(text: &str, language: Option<&str>) -> String {
  let language = language.map(str::to_lowercase);
  match FILLERS.iter().find(|(code, _)| Some(*code) == language.as_deref()) {
    Some((_, fillers)) => without_fillers(text, fillers),
    None => text.to_owned(),
  }
}

/// `text` without its `fillers`. Spaces make the words here, marks only follow them: `uh-huh`,
/// `um.edu` and a quoted `«э-э»` aren't fillers, and neither is an `ER` or a `uM`: a filler is in
/// lower case or opens with a capital. Of a filler's marks an end, colon or semicolon goes to the
/// word before it, unless it opens a sentence, and a comma or dash goes with it, so a pair leaves
/// the one before it; at the text's end only a comma or dash left dangling goes. The text keeps
/// its own whitespace, and where words go one separator stays: the text's own at its start or end,
/// the piece with the most line breaks if one has any, a space otherwise.
fn without_fillers(text: &str, fillers: &[&str]) -> String {
  let filler = |chunk: &str| {
    let word = chunk.trim_end_matches(is_mark);
    !capital_inside(word) && fillers.contains(&word.to_lowercase().as_str())
  };

  if !text.split_whitespace().any(filler) {
    return text.to_owned();
  }

  // The words kept with the whitespace before each, the whitespace of the words gone since.
  let (mut kept, mut gap): (Vec<(&str, String)>, Vec<&str>) = (Vec::new(), Vec::new());
  let mut chunks = words(text).peekable();
  // The next word opens a sentence, it takes a capital from a filler, the text ends in a filler.
  let (mut opening, mut capital, mut last_filler) = (true, false, false);
  while let Some((space, chunk)) = chunks.next() {
    last_filler = filler(chunk);
    if !last_filler {
      let word = chunk.contains(char::is_alphanumeric);
      let space = match gap.is_empty() {
        true => space,
        false => {
          gap.push(space);
          separator(&gap, kept.is_empty(), false)
        }
      };

      gap.clear();
      kept.push((space, if capital && word { capitalized(chunk) } else { chunk.to_owned() }));
      capital &= !word;
      opening = if word { ends_sentence(chunk) } else { opening };
      continue;
    }

    gap.push(space);
    let mut marks = chunk[chunk.trim_end_matches(is_mark).len()..].to_owned();
    while let Some((space, next)) = chunks.next_if(|(_, c)| c.chars().all(is_mark)) {
      gap.push(space);
      marks.push_str(next);
    }

    if opening {
      capital |= chunk.starts_with(char::is_uppercase);
      continue;
    }

    if marks.contains(ENDS) || marks.contains(PAUSES) {
      trim_marks(&mut kept, &mut gap, is_mark);
      kept.last_mut().unwrap().1.push_str(&marks);
      opening = marks.contains(ENDS);
    }
  }

  if last_filler && !opening {
    trim_marks(&mut kept, &mut gap, |c| LIGHT.contains(&c));
  }

  if !kept.iter().any(|(_, k)| k.contains(char::is_alphanumeric)) {
    return text.to_owned();
  }

  let mut end = &text[text.trim_end().len()..];
  if !gap.is_empty() {
    gap.push(end);
    end = separator(&gap, false, true);
  }

  kept.iter().flat_map(|(space, k)| [*space, k.as_str()]).chain([end]).collect()
}

/// The words of `text`, each with the whitespace before it.
fn words(text: &str) -> impl Iterator<Item = (&str, &str)> {
  let mut rest = text;
  std::iter::from_fn(move || {
    let word = rest.trim_start();
    let end = word.find(char::is_whitespace).unwrap_or(word.len());
    let item = (&rest[..rest.len() - word.len()], &word[..end]);
    rest = &word[end..];
    (end > 0).then_some(item)
  })
}

/// The one separator the whitespace of a `gap` leaves: the text's own at its start or end, else
/// the piece with the most line breaks, the first of equals, else a space.
fn separator<'a>(gap: &[&'a str], start: bool, end: bool) -> &'a str {
  match (start, end) {
    (true, _) => gap[0],
    (_, true) => gap[gap.len() - 1],
    _ => {
      let breaks = |s: &&str| s.matches('\n').count();
      let most = gap.iter().filter(|s| breaks(s) > 0).min_by_key(|s| std::cmp::Reverse(breaks(s)));
      most.copied().unwrap_or(" ")
    }
  }
}

/// The word before takes no `mark` a filler left behind, and goes if it was only marks, its
/// whitespace joining the `gap`.
fn trim_marks<'a>(
  kept: &mut Vec<(&'a str, String)>,
  gap: &mut Vec<&'a str>,
  mark: fn(char) -> bool,
) {
  while let Some((space, last)) = kept.last_mut() {
    last.truncate(last.trim_end_matches(mark).len());
    if !last.is_empty() {
      return;
    }

    gap.insert(0, space);
    kept.pop();
  }
}

/// A word or a text ends its sentence, also inside a closing quote.
pub fn ends_sentence(word: &str) -> bool {
  word.trim_end_matches(|c: char| !c.is_alphanumeric() && !ENDS.contains(&c)).ends_with(ENDS)
}

/// `text` going on the sentence before it: its first word loses its capital when that is its only
/// one and the word isn't in Latin script, where a capital is more often a name (`Python`, `API`),
/// or a letter and a dot, an initial (`А. Пушкин`).
pub fn going_on(text: &str) -> String {
  let Some(word) = text.split_whitespace().next() else { return text.to_owned() };
  let Some((at, c)) = word.char_indices().find(|(_, c)| c.is_alphabetic()) else {
    return text.to_owned();
  };

  let capitals = word.chars().filter(|c| c.is_uppercase()).count();
  let letters = word.chars().filter(|c| c.is_alphabetic()).count();
  let initial = letters == 1 && word[at + c.len_utf8()..].starts_with('.');
  if !c.is_uppercase() || capitals > 1 || initial || word.chars().any(latin) {
    return text.to_owned();
  }

  let at = text.len() - text.trim_start().len() + at;
  format!("{}{}{}", &text[..at], c.to_lowercase(), &text[at + c.len_utf8()..])
}

/// A Latin letter, with the accented ones of Latin-1 and Latin Extended.
fn latin(c: char) -> bool {
  c.is_ascii_alphabetic() || matches!(c, '\u{c0}'..='\u{24f}' | '\u{1e00}'..='\u{1eff}')
}

/// A capital after the first letter of `word`: `ER`, `iPhone`.
fn capital_inside(word: &str) -> bool {
  word.chars().skip(1).any(char::is_uppercase)
}

/// `word` with a capital, after what opens it, a quote or a bracket, unless it has one of its own
/// inside it: `iPhone` is written so.
fn capitalized(word: &str) -> String {
  let Some((at, c)) = word.char_indices().find(|(_, c)| c.is_alphanumeric()) else {
    return word.to_owned();
  };

  match capital_inside(&word[at..]) {
    true => word.to_owned(),
    false => format!("{}{}{}", &word[..at], c.to_uppercase(), &word[at + c.len_utf8()..]),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ru(text: &str) -> String {
    clean(text, Some("ru"))
  }

  fn en(text: &str) -> String {
    clean(text, Some("en"))
  }

  #[test]
  fn every_filler_of_a_table_goes() {
    for (code, fillers) in FILLERS {
      for filler in fillers {
        let words = format!("one {filler} two");
        assert_eq!(clean(&words, Some(code)), "one two", "{filler}");
      }
    }
  }

  /// Hesitation sounds only: filler words and repeats may be meant.
  #[test]
  fn what_is_not_in_the_table_stays() {
    for text in ["Ну вот, мы пошли", "Нет нет нет", "эээ, мм, это", "Э-э-э-э, да"]
    {
      assert_eq!(ru(text), text);
    }

    for text in ["no no no", "like, you know, it's fine", "the umbrella is here"] {
      assert_eq!(en(text), text);
    }
  }

  /// A letter alone is no filler: an initial, a letter talked about, a building, a name.
  #[test]
  fn the_letter_stays() {
    for text in ["Э. Хемингуэй", "буква э", "корпус Э стоит", "буква эм", "Эм Ватсон"]
    {
      assert_eq!(ru(text), text);
    }
  }

  /// Another language, or none known, keeps every word: `um` is a word in German.
  #[test]
  fn a_language_without_a_table_stays_as_it_is() {
    let words = "Um, er kommt um acht.";
    assert_eq!(clean(words, Some("de")), words);
    assert_eq!(clean(words, Some("pt")), words);
    assert_eq!(clean(words, None), words);
    assert_eq!(clean("Э-э, да", Some("en")), "Э-э, да", "by the language, not the script");
  }

  #[test]
  fn a_language_code_has_no_case() {
    assert_eq!(clean("Да э-э нет", Some("RU")), "Да нет");
  }

  #[test]
  fn only_whole_words_go() {
    for text in ["этот эмир хмурится", "э-мейл и э-э-мейл", "Уже Э-маркет"]
    {
      assert_eq!(ru(text), text);
    }

    for text in ["uh-huh, um-hum", "um.edu and er.txt", "the summer umpire", "er's um_x 2um"] {
      assert_eq!(en(text), text);
    }
  }

  /// A filler the model set apart is a word someone talks about.
  #[test]
  fn a_quoted_filler_stays() {
    assert_eq!(ru("звук «э-э» и (э-э) потом"), "звук «э-э» и (э-э) потом");
    assert_eq!(en(r#"the word "um" here"#), r#"the word "um" here"#);
  }

  #[test]
  fn a_filler_may_open_with_a_capital() {
    assert_eq!(ru("Да Э-э нет Хм потом"), "Да нет потом");
    assert_eq!(en("Well Um yes Hmm no"), "Well yes no");
  }

  /// A capital inside a word makes it a name: `ER` is a hospital's.
  #[test]
  fn a_word_with_capitals_inside_stays() {
    for text in ["I went to the ER yesterday", "Well UM yes", "Well, uM, yes", "ER, UM."] {
      assert_eq!(en(text), text);
    }

    assert_eq!(ru("Да Э-Э нет ХМ потом"), "Да Э-Э нет ХМ потом");
  }

  #[test]
  fn a_first_filler_gives_its_capital_to_the_next_word() {
    assert_eq!(ru("Э-э, в 1990 году"), "В 1990 году");
    assert_eq!(ru("Э-э в 1990 году"), "В 1990 году");
    assert_eq!(en("Um, I think so."), "I think so.");
    assert_eq!(ru("Э-э, э-э, хм, в 1990 году"), "В 1990 году");
    assert_eq!(ru("э-э, Э-э, в 1990 году"), "В 1990 году");
    assert_eq!(ru("Э-э, «в» году"), "«В» году");
    assert_eq!(ru("Э-э, 1990 год"), "1990 год");
    assert_eq!(ru("Э-э — в году"), "В году");
    assert_eq!(ru("Э-э... в году"), "В году");
  }

  /// `iPhone` is written so: the capital is spent on it all the same.
  #[test]
  fn a_word_with_a_capital_inside_takes_none() {
    assert_eq!(en("Um, iPhone sales"), "iPhone sales");
    assert_eq!(en("Um, «iPhone» sales"), "«iPhone» sales");
    assert_eq!(en("Um, NASA called"), "NASA called");
  }

  /// A text the model opened in lower case stays so: a filler gives no capital it didn't have.
  #[test]
  fn a_lower_case_first_filler_gives_none() {
    assert_eq!(ru("э-э, в 1990 году"), "в 1990 году");
    assert_eq!(ru("э-э, Москва"), "Москва");
  }

  #[test]
  fn a_sentence_opened_by_a_filler_keeps_its_capital() {
    assert_eq!(ru("Да. Э-э, в 1990 году"), "Да. В 1990 году");
    assert_eq!(ru("Да? Э-э. Потом"), "Да? Потом");
    assert_eq!(ru("Он сказал: «Да.» Э-э, потом"), "Он сказал: «Да.» Потом");
    assert_eq!(ru("Да, э-э. Э-э, потом"), "Да. Потом");
    assert_eq!(ru("— Э-э, в году"), "— В году");
  }

  #[test]
  fn a_capital_inside_a_sentence_goes_with_its_filler() {
    assert_eq!(ru("мы Э-э пошли"), "мы пошли");
  }

  #[test]
  fn the_marks_a_filler_leaves_are_tidied() {
    assert_eq!(ru("Мы э-э, пошли"), "Мы пошли");
    assert_eq!(ru("Он сказал, э-э что"), "Он сказал, что");
    assert_eq!(ru("Купили э-э: сыр"), "Купили: сыр");
    assert_eq!(ru("Купили, э-э: сыр"), "Купили: сыр");
    assert_eq!(ru("Купили: э-э, сыр"), "Купили: сыр");
    assert_eq!(ru("Купили: э-э: сыр"), "Купили: сыр");
    assert_eq!(en("I went, um... to the store"), "I went... to the store");
  }

  /// The mark before a filler may be real: a comma too many costs less than a lost one.
  #[test]
  fn a_pair_around_a_filler_leaves_one() {
    assert_eq!(ru("Да, э-э, конечно"), "Да, конечно");
    assert_eq!(ru("сыр, э-э, хлеб"), "сыр, хлеб");
    assert_eq!(ru("Он сказал, э-э, что"), "Он сказал, что");
    assert_eq!(ru("Мы, э-э, пошли"), "Мы, пошли");
    assert_eq!(ru("Мы, э-э, э-э, пошли"), "Мы, пошли");
    assert_eq!(ru("Мы, э-э э-э, пошли"), "Мы, пошли");
    assert_eq!(ru("Мы , э-э , пошли"), "Мы , пошли");
    assert_eq!(ru("Москва — э-э — столица"), "Москва — столица");
    assert_eq!(ru("Москва - э-э - столица"), "Москва - столица");
  }

  #[test]
  fn a_last_filler_leaves_the_sentence_its_end() {
    assert_eq!(ru("Мы пошли, э-э."), "Мы пошли.");
    assert_eq!(ru("Мы пошли, э-э?"), "Мы пошли?");
    assert_eq!(ru("Мы пошли, э-э"), "Мы пошли");
    assert_eq!(ru("Мы пошли э-э"), "Мы пошли");
    assert_eq!(ru("Мы пошли — э-э"), "Мы пошли");
    assert_eq!(ru("Мы пошли. Э-э"), "Мы пошли.");
    assert_eq!(ru("Мы пошли, э-э. Потом"), "Мы пошли. Потом");
    assert_eq!(ru("Купили: э-э. Потом"), "Купили. Потом");
    assert_eq!(ru("Купили: э-э"), "Купили:");
    assert_eq!(ru("Купили э-э:"), "Купили:");
    assert_eq!(ru("Купили; э-э"), "Купили;");
  }

  #[test]
  fn no_double_space_is_left() {
    assert_eq!(ru("Мы  э-э  пошли"), "Мы пошли");
  }

  #[test]
  fn a_text_of_nothing_else_stays_as_it_is() {
    for text in ["Э-э.", "ээ", "Э-э, ммм, хм.", "— Э-э...", ""] {
      assert_eq!(ru(text), text);
    }

    assert_eq!(en("Um, uh, hmm?"), "Um, uh, hmm?");
  }

  #[test]
  fn a_text_without_a_filler_keeps_its_spaces() {
    assert_eq!(ru("Мы  пошли\nдомой"), "Мы  пошли\nдомой");
  }

  #[test]
  fn line_breaks_stay() {
    assert_eq!(ru("Первый абзац.\n\nЭ-э, второй абзац."), "Первый абзац.\n\nВторой абзац.");
    assert_eq!(ru("Первая строка\nвторая э-э строка\n"), "Первая строка\nвторая строка\n");
    assert_eq!(ru("Начало\nэ-э\n\nКонец"), "Начало\n\nКонец");
    assert_eq!(ru("Начало\n\nэ-э\nКонец"), "Начало\n\nКонец");
    assert_eq!(ru("Мы пошли э-э\n\nдомой"), "Мы пошли\n\nдомой");
    assert_eq!(ru("Мы пошли. Э-э\n"), "Мы пошли.\n");
    assert_eq!(ru("Мы пошли, э-э.\r\nПотом"), "Мы пошли.\r\nПотом");
  }

  #[test]
  fn a_filler_opening_a_line_goes_with_its_space() {
    assert_eq!(ru("Да.\nЭ-э, потом\nдомой"), "Да.\nПотом\nдомой");
    assert_eq!(ru("  Э-э, мы пошли"), "  Мы пошли");
    assert_eq!(en("Fine.\n\tUm, I think so."), "Fine.\n\tI think so.");
  }

  #[test]
  fn spaces_away_from_a_filler_stay() {
    assert_eq!(ru("Мы\tпошли  домой, э-э, потом"), "Мы\tпошли  домой, потом");
    assert_eq!(en("a\t\tb   c um d"), "a\t\tb   c d");
  }

  /// Only a capital that is the first word's one, in a script other than Latin.
  #[test]
  fn a_text_going_on_loses_its_first_capital() {
    assert_eq!(going_on("Потом домой. Да"), "потом домой. Да");
    assert_eq!(going_on("«Потом» домой"), "«потом» домой");
    assert_eq!(going_on("Ύστερα σπίτι"), "ύστερα σπίτι");
    assert_eq!(going_on("Я пошёл"), "я пошёл");

    let kept = [
      "Python и API",
      "Élan",
      "iPhone",
      "ВКонтакте",
      "МГУ",
      "А. Пушкин",
      "1990 год",
      "— Потом",
      "",
    ];
    for text in kept {
      assert_eq!(going_on(text), text);
    }
  }
}
