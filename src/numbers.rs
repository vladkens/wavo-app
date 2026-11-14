// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Numbers a model wrote in Russian words, in digits: `семьдесят шестой` is `76-й`, `тысяча
//! семьсот пятьдесят шесть` is `1756`, `двадцать пять тысяч` is `25 тысяч`. GigaAM writes most
//! numbers in digits itself, and an ordinal, or a number in a case other than the nominative, often
//! in words. As in the cleanup, what is in doubt stays as it is: a number of one word stays in
//! words (`один из`, `в сотый раз`, `минут десять`), but for 11–19, which no phrase has; two
//! numbers in a row stay, which a time or a count may be (`восемнадцать сорок`, `пять шесть`); a
//! scale with nothing after it stays a word, in its case (`на 88 тысячах`).
//! The thousands GigaAM parts with spaces join: `1 000` is `1000`.

use crate::cleanup::words;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
  Unit,
  Teen,
  Ten,
  Hundred,
  Scale,
}

/// A number word: its kind and value, and an ordinal's ending as written after the digits (`й`,
/// `го`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
  kind: Kind,
  value: u64,
  ordinal: Option<String>,
}

impl Word {
  fn new(value: u64, ordinal: Option<String>) -> Self {
    let kind = match value {
      1..10 => Kind::Unit,
      10..20 => Kind::Teen,
      20..100 => Kind::Ten,
      100..1000 => Kind::Hundred,
      _ => Kind::Scale,
    };
    Word { kind, value, ordinal }
  }
}

/// The cardinals in all their cases, `ё` as `е`.
const CARDINALS: [(u64, &[&str]); 31] = [
  (
    1,
    &[
      "один",
      "одна",
      "одно",
      "одни",
      "одного",
      "одной",
      "одному",
      "одним",
      "одном",
      "одну",
      "одними",
      "одних",
    ],
  ),
  (2, &["два", "две", "двух", "двум", "двумя"]),
  (3, &["три", "трех", "трем", "тремя"]),
  (4, &["четыре", "четырех", "четырем", "четырьмя"]),
  (5, &["пять", "пяти", "пятью"]),
  (6, &["шесть", "шести", "шестью"]),
  (7, &["семь", "семи", "семью"]),
  (8, &["восемь", "восьми", "восемью", "восьмью"]),
  (9, &["девять", "девяти", "девятью"]),
  (10, &["десять", "десяти", "десятью"]),
  (11, &["одиннадцать", "одиннадцати", "одиннадцатью"]),
  (12, &["двенадцать", "двенадцати", "двенадцатью"]),
  (13, &["тринадцать", "тринадцати", "тринадцатью"]),
  (14, &["четырнадцать", "четырнадцати", "четырнадцатью"]),
  (15, &["пятнадцать", "пятнадцати", "пятнадцатью"]),
  (16, &["шестнадцать", "шестнадцати", "шестнадцатью"]),
  (17, &["семнадцать", "семнадцати", "семнадцатью"]),
  (18, &["восемнадцать", "восемнадцати", "восемнадцатью"]),
  (19, &["девятнадцать", "девятнадцати", "девятнадцатью"]),
  (20, &["двадцать", "двадцати", "двадцатью"]),
  (30, &["тридцать", "тридцати", "тридцатью"]),
  (40, &["сорок", "сорока"]),
  (50, &["пятьдесят", "пятидесяти", "пятьюдесятью"]),
  (60, &["шестьдесят", "шестидесяти", "шестьюдесятью"]),
  (70, &["семьдесят", "семидесяти", "семьюдесятью"]),
  (80, &["восемьдесят", "восьмидесяти", "восемьюдесятью", "восьмьюдесятью"]),
  (90, &["девяносто", "девяноста"]),
  (100, &["сто", "ста"]),
  (200, &["двести", "двухсот", "двумстам", "двумястами", "двухстах"]),
  (300, &["триста", "трехсот", "тремстам", "тремястами", "трехстах"]),
  (400, &["четыреста", "четырехсот", "четыремстам", "четырьмястами", "четырехстах"]),
];

/// The hundreds from 500, of one pattern, by two stems: `пятьсот`, `пятисот`, `пятистам`,
/// `пятьюстами`, `пятистах`.
const HUNDREDS: [(u64, &str, &str); 5] = [
  (500, "пять", "пяти"),
  (600, "шесть", "шести"),
  (700, "семь", "семи"),
  (800, "восемь", "восьми"),
  (900, "девять", "девяти"),
];

/// The scales and their endings in all cases.
const SCALES: [(u64, &str, &[&str]); 3] = [
  (1_000, "тысяч", &["а", "и", "е", "у", "ей", "ью", "", "ам", "ами", "ах"]),
  (1_000_000, "миллион", &["", "а", "у", "ом", "е", "ы", "ов", "ам", "ами", "ах"]),
  (1_000_000_000, "миллиард", &["", "а", "у", "ом", "е", "ы", "ов", "ам", "ами", "ах"]),
];

/// The ordinals' stems.
const ORDINALS: [(u64, &str); 39] = [
  (1, "перв"),
  (2, "втор"),
  (3, "трет"),
  (4, "четверт"),
  (5, "пят"),
  (6, "шест"),
  (7, "седьм"),
  (8, "восьм"),
  (9, "девят"),
  (10, "десят"),
  (11, "одиннадцат"),
  (12, "двенадцат"),
  (13, "тринадцат"),
  (14, "четырнадцат"),
  (15, "пятнадцат"),
  (16, "шестнадцат"),
  (17, "семнадцат"),
  (18, "восемнадцат"),
  (19, "девятнадцат"),
  (20, "двадцат"),
  (30, "тридцат"),
  (40, "сороков"),
  (50, "пятидесят"),
  (60, "шестидесят"),
  (70, "семидесят"),
  (80, "восьмидесят"),
  (90, "девяност"),
  (100, "сот"),
  (200, "двухсот"),
  (300, "трехсот"),
  (400, "четырехсот"),
  (500, "пятисот"),
  (600, "шестисот"),
  (700, "семисот"),
  (800, "восьмисот"),
  (900, "девятисот"),
  (1_000, "тысячн"),
  (1_000_000, "миллионн"),
  (1_000_000_000, "миллиардн"),
];

/// An ordinal's endings in all cases, genders and numbers; `третий`'s are its own.
const ENDINGS: [&str; 24] = [
  "ый", "ой", "ий", "ая", "яя", "ое", "ее", "ые", "ие", "ого", "его", "ому", "ему", "ым", "им",
  "ом", "ем", "ую", "юю", "ых", "их", "ыми", "ими", "ей",
];
const THIRD: [&str; 12] =
  ["ий", "ьего", "ьему", "ьим", "ьем", "ья", "ье", "ьи", "ьих", "ьими", "ьей", "ью"];

/// The scales that may open a number without one before them.
const BARE: [&str; 3] = ["тысяча", "миллион", "миллиард"];

const VOWELS: &str = "аеиоуыэюя";

/// `text` with its Russian numbers of more than one word, and those of 11–19, in digits, and
/// the thousands of a number in digits not parted by spaces.
pub fn digits(text: &str) -> String {
  let text = &ungroup(text);
  let chunks: Vec<(&str, &str)> = words(text).collect();
  let mut out = String::with_capacity(text.len());
  let mut i = 0;
  while i < chunks.len() {
    // A run of number words, and numbers the model wrote in digits, with nothing between them
    // but spaces: a line break or a tab parts rows, and a run with digits in it stays as it is.
    let mut end = i;
    while end < chunks.len() && (word(chunks[end].1).is_some() || in_digits(chunks[end].1)) {
      let (_, chunk) = chunks[end];
      end += 1;
      let next = chunks
        .get(end)
        .map(|(space, c)| space.chars().all(|c| c == ' ') && c.starts_with(char::is_alphanumeric));
      if !chunk.ends_with(char::is_alphanumeric) || next == Some(false) {
        break;
      }
    }

    let end = end.max(i + 1);
    match number(&chunks[i..end]) {
      Some(number) => out.push_str(&format!("{}{number}", chunks[i].0)),
      None => {
        chunks[i..end].iter().for_each(|(space, chunk)| out.push_str(&format!("{space}{chunk}")))
      }
    }

    i = end;
  }

  out + &text[text.trim_end().len()..]
}

/// `text` with the spaces GigaAM writes between a number's thousands taken out: `2 300 000 ₽` is
/// `2300000 ₽`; groups with more digits around them (a phone's) and a fraction's stay.
fn ungroup(text: &str) -> String {
  let chunks: Vec<(&str, &str)> = words(text).collect();
  let spaces = |s: &str| s.chars().all(|c| matches!(c, ' ' | '\u{a0}' | '\u{202f}'));
  let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
  let mut out = String::with_capacity(text.len());
  let mut i = 0;
  while i < chunks.len() {
    let (space, lead) = chunks[i];
    let core = lead.trim_start_matches(|c: char| !c.is_alphanumeric() && !"+,.".contains(c));
    let mut end = i + 1;
    let mut last = "";
    let after = i > 0 && spaces(space) && chunks[i - 1].1.ends_with(|c: char| c.is_ascii_digit());
    if digits(core) && core.len() <= 3 && !after {
      while let Some(&(gap, group)) = chunks.get(end) {
        let three = group.get(..3).is_some_and(digits) && !digits(group.get(3..4).unwrap_or(""));
        if !spaces(gap) || !three {
          break;
        }

        (end, last) = (end + 1, group);
        if group.len() > 3 {
          break;
        }
      }
    }

    let row = chunks
      .get(end)
      .is_some_and(|(gap, c)| spaces(gap) && c.starts_with(|c: char| c.is_ascii_digit()));
    if end == i + 1 || (last.len() == 3 && row) {
      end = i + 1;
    }

    out.push_str(space);
    chunks[i..end].iter().for_each(|(_, chunk)| out.push_str(chunk));
    i = end;
  }

  out + &text[text.trim_end().len()..]
}

/// `chunks`, a run of number words, as one number in digits with the marks around it; `None`
/// when they are more numbers than one, or one that stays in words.
fn number(chunks: &[(&str, &str)]) -> Option<String> {
  let words: Vec<Word> = chunks.iter().map(|(_, c)| word(c)).collect::<Option<_>>()?;
  let single = matches!(words.as_slice(), [w] if w.kind == Kind::Teen && w.value > 10);
  if words.len() < 2 && !single {
    return None;
  }

  let core = |i: usize| chunks[i].1.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
  let (mut total, mut group, mut stage, mut scale) = (0, 0, 0, u64::MAX);
  for (i, w) in words.iter().enumerate() {
    if w.ordinal.is_some() && i + 1 < words.len() {
      return None;
    }

    // `тысячных` after a number is a fraction, `сто первых` the first hundred, and a scale with
    // no number before it, but `тысяча`, an approximate one: `рублей тысяч пять`.
    let fraction = w.kind == Kind::Scale && w.ordinal.is_some();
    let firsts = w.value == 1
      && w.ordinal.is_some()
      && ["ые", "ых", "ыми"].iter().any(|e| core(i).ends_with(e));
    let bare = w.kind == Kind::Scale && group == 0 && (i > 0 || !BARE.contains(&core(i).as_str()));
    if fraction || firsts || bare {
      return None;
    }

    // A group is hundreds, then tens or a teen, then units; a scale ends it, each smaller.
    stage = match w.kind {
      Kind::Hundred if stage < 1 => 1,
      Kind::Ten if stage < 2 => 2,
      Kind::Teen if stage < 2 => 3,
      Kind::Unit if stage < 3 => 3,
      Kind::Scale if w.value < scale => {
        (total, group, scale) = (total + group.max(1) * w.value, 0, w.value);
        0
      }
      _ => return None,
    };
    group += if w.kind == Kind::Scale { 0 } else { w.value };
  }

  let (first, last) = (chunks[0].1, chunks[chunks.len() - 1].1);
  let lead = &first[..first.len() - first.trim_start_matches(|c: char| !c.is_alphanumeric()).len()];
  let core = last.trim_end_matches(|c: char| !c.is_alphanumeric());
  let trail = &last[core.len()..];
  let ending = words.last().unwrap();
  let written = match &ending.ordinal {
    Some(suffix) => format!("{}-{suffix}", total + group),
    // A scale with nothing after it stays a word: `25 тысяч`, `на 88 тысячах`; but for one
    // after a larger scale: `2300000`, not `2300 тысяч`.
    None if ending.kind == Kind::Scale && total / scale < 1000 => {
      let word = core.trim_start_matches(|c: char| !c.is_alphanumeric());
      format!("{} {word}", total / scale)
    }
    None => (total + group).to_string(),
  };

  Some(format!("{lead}{written}{trail}"))
}

/// Whether `chunk` is a number in digits, as `2` or `2026,`.
fn in_digits(chunk: &str) -> bool {
  let core = chunk.trim_matches(|c: char| !c.is_alphanumeric());
  !core.is_empty() && core.chars().all(|c| c.is_ascii_digit())
}

/// The number word in `chunk`, without the marks around it.
fn word(chunk: &str) -> Option<Word> {
  let core = chunk.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase().replace('ё', "е");
  cardinal(&core).or_else(|| ordinal(&core))
}

fn cardinal(word: &str) -> Option<Word> {
  let found = CARDINALS.iter().find(|(_, forms)| forms.contains(&word));
  if let Some(&(value, _)) = found {
    return Some(Word::new(value, None));
  }

  let hundred = HUNDREDS.iter().find(|(_, nom, other)| {
    let cases = [(nom, "сот"), (other, "сот"), (other, "стам"), (nom, "юстами"), (other, "стах")];
    cases.iter().any(|(stem, end)| word.strip_prefix(**stem) == Some(end))
  });
  if let Some(&(value, ..)) = hundred {
    return Some(Word::new(value, None));
  }

  let scale = SCALES
    .iter()
    .find(|(_, stem, ends)| word.strip_prefix(stem).is_some_and(|end| ends.contains(&end)));
  scale.map(|&(value, ..)| Word::new(value, None))
}

/// An ordinal by its stem, the longest that leaves one of its endings; its ending after the
/// digits: the last letter after a vowel, else the last two (`76-й`, `35-го`, `16-ми`).
fn ordinal(word: &str) -> Option<Word> {
  let (value, end) = ORDINALS
    .iter()
    .filter_map(|&(value, stem)| Some((value, stem.len(), word.strip_prefix(stem)?)))
    .filter(|&(value, _, end)| if value == 3 { &THIRD[..] } else { &ENDINGS }.contains(&end))
    .max_by_key(|&(_, len, _)| len)
    .map(|(value, _, end)| (value, end))?;

  let end: Vec<char> = end.trim_start_matches('ь').chars().collect();
  let after_vowel = end.len() < 2 || VOWELS.contains(end[end.len() - 2]);
  let suffix: String = end[end.len() - if after_vowel { 1 } else { 2 }..].iter().collect();
  Some(Word::new(value, Some(suffix)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn numbers_of_more_words_than_one_are_digits() {
    let cases = [
      ("Вот сейчас семьдесят шестой сначала", "Вот сейчас 76-й сначала"),
      ("Шестьдесят второй там", "62-й там"),
      ("Тридцать пятого обоснуй", "35-го обоснуй"),
      ("на двадцать шестой версии", "на 26-й версии"),
      ("начнём с сорок девятого и", "начнём с 49-го и"),
      ("И в пятьдесят второй напиши", "И в 52-й напиши"),
      ("тысяча семьсот пятьдесят шесть", "1756"),
      ("в тысяча девятьсот девяносто пятом году", "в 1995-м году"),
      ("в две тысячи двадцать шестом", "в 2026-м"),
      ("сто пятьдесят тысяч двести рублей", "150200 рублей"),
      ("двадцать пять тысяч", "25 тысяч"),
      ("на восьмидесяти восьми тысячах", "на 88 тысячах"),
      ("три миллиона", "3 миллиона"),
      ("два миллиона триста тысяч рублей", "2300000 рублей"),
      ("тысяча двести", "1200"),
      ("двести двадцать одной", "221"),
      ("пятьюстами двадцатью", "520"),
      ("двадцать третьего", "23-го"),
      ("тридцать третья", "33-я"),
      ("сто первом", "101-м"),
      ("девяносто седьмых", "97-х"),
      ("двадцать четвёртом", "24-м"),
      ("подрезать сорока восьмью секундами", "подрезать 48 секундами"),
      ("триста восьмидесятая", "380-я"),
      ("две тысячи пятисотый", "2500-й"),
    ];
    for (said, written) in cases {
      assert_eq!(digits(said), written, "{said}");
    }
  }

  /// A number of one word stays, as in a phrase, but for 11–19, which none has.
  #[test]
  fn one_word_stays_but_from_eleven_to_nineteen() {
    for said in [
      "один из нас",
      "в сотый раз",
      "минут десять",
      "около тридцати проектов",
      "на миллион вопросов",
      "тысячу раз",
      "Сорока летела",
      "Три сильнее",
      "второй раз",
      "двадцатый век",
    ] {
      assert_eq!(digits(said), said);
    }

    assert_eq!(digits("посмотри на четырнадцатый"), "посмотри на 14-й");
    assert_eq!(digits("Вот почему шестнадцатый так"), "Вот почему 16-й так");
    assert_eq!(digits("одиннадцать человек"), "11 человек");
  }

  /// Two numbers in a row stay: a time, a count, a phone number; so do numbers marks part.
  #[test]
  fn numbers_in_a_row_stay() {
    for said in [
      "восемнадцать сорок четыре",
      "тридцатью сорока восьмью секундами",
      "пять шесть",
      "двадцать двадцать",
      "двадцать пятнадцать",
      "сто сто",
      "пятый шестой",
      "двадцатый двадцать первый",
    ] {
      assert_eq!(digits(said), said);
    }

    assert_eq!(digits("двадцать пять, тридцать два"), "25, 32");
    assert_eq!(digits("двадцать пять пятьдесят"), "двадцать пять пятьдесят");
    assert_eq!(digits("двадцать, пять"), "двадцать, пять");
  }

  /// What reads otherwise than as one number stays.
  #[test]
  fn numbers_of_another_sense_stay() {
    for said in [
      "рублей тысяч пять",
      "миллиона два",
      "тысячи три",
      "сто первых посетителей",
      "двадцать первыми пришли",
      "пять тысячных секунды",
      "две третьих",
      "три пятых",
    ] {
      assert_eq!(digits(said), said);
    }
  }

  #[test]
  fn thousands_in_digits_join() {
    let cases = [
      ("На счету 2 300 000 ₽.", "На счету 2300000 ₽."),
      ("примерно 1 101 строчка", "примерно 1101 строчка"),
      ("(150\u{a0}200), 1 000-й", "(150200), 1000-й"),
      ("Код 734 1, 0,5 075 звёзд", "Код 734 1, 0,5 075 звёзд"),
      ("+7 900 123 45 67, 8 900 123 45 67", "+7 900 123 45 67, 8 900 123 45 67"),
      ("+7 495 12 34 567", "+7 495 12 34 567"),
      ("10 000\n500 штук", "10000\n500 штук"),
      ("по 2 1000", "по 2 1000"),
    ];
    for (said, written) in cases {
      assert_eq!(digits(said), written);
    }
  }

  /// A number partly in digits stays: its words alone would make another one.
  #[test]
  fn a_number_with_digits_in_it_stays() {
    for said in ["2 тысячи двадцать пять", "2 миллиона триста тысяч", "двадцать 5", "в 2026 году"]
    {
      assert_eq!(digits(said), said);
    }

    assert_eq!(digits("в 2026, двадцать пятом"), "в 2026, 25-м");
  }

  /// A line break or a tab parts numbers: rows and columns.
  #[test]
  fn numbers_on_other_lines_stay_apart() {
    assert_eq!(digits("сто\nдвадцать пять"), "сто\n25");
    assert_eq!(digits("двадцать\tпять"), "двадцать\tпять");
    assert_eq!(digits("двадцать  пять"), "25");
  }

  #[test]
  fn the_marks_and_spaces_around_stay() {
    assert_eq!(digits("  «семьдесят шестой», "), "  «76-й», ");
    assert_eq!(digits("(двадцать пять)\nдва"), "(25)\nдва");
    assert_eq!(digits("Двадцать пять."), "25.");
  }

  #[test]
  fn other_words_stay() {
    for said in
      ["пятно", "сотня", "пятерка", "семьюдесятью-восемью", "76-й", "1756", "трёхсотлетие", ""]
    {
      assert_eq!(digits(said), said);
    }
  }
}
