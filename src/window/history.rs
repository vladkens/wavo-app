// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The History page: the dictations, newest first, in a plate per day, a page at a time with Show
//! More, found by a search above them. A row is one line of the text (the whole text in a tooltip
//! over it), then the time, length and whether it was not pasted. Its icon buttons: Play / Stop
//! (the recording), Copy, Re-transcribe and Delete, which asks first in the window's dialog with
//! the start of the text ([`super::Ask::Dictation`]). A click selects a row: it shows the model
//! too. Reloaded when the session's history moves.

use std::path::{Path, PathBuf};

use nimbo::{Align, Button, Float, FormRow, FormStyle, Icon, Id, Style, TextStyle, Ui, grow};

use super::{Ask, Prefs};
use crate::audio::playback::Playback;
use crate::history::db::Entry;
use crate::history::{History, Local, local_time, now_ms, shorten};
use crate::session::controller::label;

pub const SEARCH: &str = "history-search";
pub const MORE: &str = "more";
/// A row, `(ROW, id)`: a click selects it; `(TEXT, id)` over its text shows the whole text. Its
/// buttons: `(COPY, id)` and so on.
pub const ROW: &str = "entry";
pub const TEXT: &str = "text";
pub const COPY: &str = "copy";
pub const PLAY: &str = "play";
pub const RETRANSCRIBE: &str = "retranscribe";
pub const DELETE: &str = "delete";

/// Entries a page adds.
pub const PAGE: usize = 50;
/// The longest first line of a row, in characters.
const ROW_CHARS: usize = 300;
/// The most of a text the dialog asking before its delete quotes, in characters.
const START_CHARS: usize = 60;

/// A calendar day: year, month, day.
type Day = (i32, u32, u32);

/// What the page has loaded and is doing.
pub struct State {
  home: PathBuf,
  db: Option<History>,
  /// The session's history changes: a move reloads.
  pub revision: u64,
  pub search: String,
  pub entries: Vec<Entry>,
  /// How many entries are asked for.
  limit: usize,
  /// There are more than the loaded ones.
  more: bool,
  /// What the entries were loaded for: the search, the limit and the revision.
  loaded: Option<(String, usize, u64)>,
  /// The selected entry.
  pub selected: Option<i64>,
  /// The entry whose recording plays.
  pub playing: Option<(i64, Playback)>,
  /// What the search row says (an error, "Copied").
  pub note: Option<String>,
}

impl State {
  pub fn new(home: &Path) -> Self {
    Self {
      home: home.to_owned(),
      db: None,
      revision: 0,
      search: String::new(),
      entries: Vec::new(),
      limit: PAGE,
      more: false,
      loaded: None,
      selected: None,
      playing: None,
      note: None,
    }
  }

  /// Loads the entries again if the search, the limit or the history changed.
  fn load(&mut self) {
    let key = (self.search.trim().to_owned(), self.limit, self.revision);
    if self.loaded.as_ref() == Some(&key) {
      return;
    }

    self.loaded = Some(key.clone());
    if self.db.is_none() {
      match History::open(&self.home) {
        Ok(db) => self.db = Some(db),
        Err(e) => {
          self.note = Some(format!("Could not open the history: {e:#}"));
          return;
        }
      }
    }

    let Some(h) = &self.db else { return };
    match h.db.get_page(&key.0, None, self.limit + 1) {
      Ok(mut entries) => {
        self.more = entries.len() > self.limit;
        entries.truncate(self.limit);
        self.entries = entries;
      }
      Err(e) => self.note = Some(format!("Could not read the history: {e:#}")),
    }
  }

  fn audio(&self, e: &Entry) -> Option<PathBuf> {
    self.db.as_ref()?.audio_file(e)
  }
}

/// A day's plate title: `Today`, `Yesterday, October 17`, `October 3`, `March 3, 2025`.
fn day_title(day: Day, today: Day) -> String {
  let (year, month, d) = day;
  let local = Local { year, month, day: d, hour: 0, minute: 0 };
  let date = format!("{} {d}", local.month_name());
  if day == today {
    return "Today".into();
  }

  if day == previous_day(today) {
    return format!("Yesterday, {date}");
  }

  match year == today.0 {
    true => date,
    false => format!("{date}, {year}"),
  }
}

/// The calendar day before `(year, month, day)`.
fn previous_day((year, month, day): Day) -> Day {
  if day > 1 {
    return (year, month, day - 1);
  }

  let (year, month) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
  let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
  let last = match month {
    2 if leap => 29,
    2 => 28,
    4 | 6 | 9 | 11 => 30,
    _ => 31,
  };
  (year, month, last)
}

fn day_of(ms: i64) -> Day {
  local_time(ms).map(|t| t.date()).unwrap_or_default()
}

fn duration_text(ms: i64) -> String {
  if ms < 60_000 {
    return format!("{:.1} s", ms as f64 / 1000.0);
  }

  let s = ms / 1000;
  format!("{}:{:02}", s / 60, s % 60)
}

/// A row's line: the text on one line.
fn first_line(text: &str) -> String {
  text.chars().take(ROW_CHARS).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Under the text: when, how long, why it was not pasted, and in a selected row the model.
fn details(e: &Entry, selected: bool) -> String {
  let time = local_time(e.created_at).map(Local::hm);
  let mut parts = vec![time.unwrap_or_default(), duration_text(e.duration_ms)];
  if let (false, Some(why)) = (e.pasted, &e.not_pasted) {
    parts.push(format!("Not pasted: {why}"));
  }

  if let (true, Some(model)) = (selected, &e.model) {
    parts.push(label(model));
  }

  parts.retain(|p| !p.is_empty());
  parts.join(" · ")
}

/// `3 dictations · 1:15`.
fn day_note(entries: &[&Entry]) -> String {
  let n = entries.len();
  let s = entries.iter().map(|e| e.duration_ms).sum::<i64>() / 1000;
  let what = if n == 1 { "dictation" } else { "dictations" };
  format!("{n} {what} · {}:{:02}", s / 60, s % 60)
}

/// What a row's buttons asked for.
enum Act {
  Select(i64),
  Copy(String),
  Play(i64),
  Stop,
  Retranscribe(i64),
  Ask(Ask),
}

/// How a row shows.
#[derive(Clone, Copy)]
struct Row {
  selected: bool,
  playing: bool,
  retranscribing: bool,
  /// No model to transcribe with.
  no_model: bool,
}

impl Prefs {
  /// The search field above the scrolling plates, and what the page says.
  pub(super) fn history_head(&mut self, ui: &mut Ui, s: Style) {
    let t = ui.theme();
    let h = &mut self.history;
    ui.row(s.align(Align::Start, Align::Center), |ui| {
      if ui.id(SEARCH).search(&mut h.search, "Search dictations").changed {
        h.limit = PAGE;
        h.selected = None;
      }

      if let Some(note) = &h.note {
        ui.text(note, TextStyle::new(t.small, t.muted).nowrap());
      }
    });
  }

  pub(super) fn history(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let retranscribing = self.status.as_ref().map(|s| s.retranscribing.clone()).unwrap_or_default();
    // Nothing to transcribe with: Re-transcribe waits (a model that does not load may yet).
    let no_model = self.model_problem().is_some_and(|p| p.stops());
    let h = &mut self.history;
    if let Some((_, p)) = h.playing.take_if(|(_, p)| !p.playing())
      && let Some(e) = p.error()
    {
      h.note = Some(format!("Could not play the recording: {e}"));
    }

    h.load();
    if h.entries.is_empty() {
      let empty = match h.search.trim().is_empty() {
        true => "No dictations yet: hold the shortcut and speak.",
        false => "Nothing found",
      };
      ui.form_section(fs, "", |ui| ui.form_row(fs, FormRow::new(empty), |_| {}));
    }

    // The entries by day, newest first as they come.
    let today = day_of(now_ms());
    let mut days: Vec<(Day, Vec<&Entry>)> = Vec::new();
    for e in &h.entries {
      let day = day_of(e.created_at);
      match days.last_mut() {
        Some((d, list)) if *d == day => list.push(e),
        _ => days.push((day, vec![e])),
      }
    }

    let mut act = None;
    for (day, entries) in &days {
      let note = day_note(entries);
      let head = |ui: &mut Ui| {
        ui.text(&note, fs.note);
      };
      ui.form_section_with(fs, &day_title(*day, today), head, |ui| {
        for e in entries {
          let state = Row {
            selected: h.selected == Some(e.id),
            playing: h.playing.as_ref().is_some_and(|(id, _)| *id == e.id),
            retranscribing: retranscribing.contains(&e.id),
            no_model,
          };
          if let Some(a) = row(ui, fs, e, state) {
            act = Some(a);
          }
        }
      });
    }

    if h.more && Button::new("Show More").show(ui.id(MORE)).clicked {
      h.limit += PAGE;
    }

    match act {
      Some(Act::Select(id)) => h.selected = if h.selected == Some(id) { None } else { Some(id) },
      Some(Act::Copy(text)) => {
        ui.clipboard().write(&text);
        h.note = Some("Copied to the clipboard".into());
      }
      Some(Act::Play(id)) => {
        let path = h.entries.iter().find(|e| e.id == id).and_then(|e| h.audio(e));
        let Some(path) = path else {
          h.note = Some("The recording is gone".into());
          return;
        };

        // It opens the file and the device on a thread of its own: a failure shows once done.
        let waker = ui.waker().clone();
        match Playback::start(&path, move || waker.wake()) {
          Ok(p) => h.playing = Some((id, p)),
          Err(e) => h.note = Some(format!("Could not play the recording: {e:#}")),
        }
      }
      Some(Act::Stop) => h.playing = None,
      Some(Act::Retranscribe(id)) => {
        h.note = None;
        self.shell.retranscribe(id);
      }
      Some(Act::Ask(ask)) => {
        h.note = None;
        self.ask = Some(ask);
      }
      None => {}
    }
  }

  /// [`Ask::Dictation`] said Delete: gone from the page at once, the session deletes it and its
  /// recording off the main thread.
  pub(super) fn delete_entry(&mut self, id: i64) {
    let h = &mut self.history;
    h.selected = h.selected.filter(|s| *s != id);
    h.entries.retain(|e| e.id != id);
    if h.playing.as_ref().is_some_and(|(p, _)| *p == id) {
      h.playing = None;
    }

    h.note = None;
    self.shell.delete_entry(id);
  }
}

/// A dictation's row: its line (the whole text in a tooltip over it) and details, then Play,
/// Copy, Re-transcribe and Delete.
fn row(ui: &mut Ui, fs: &FormStyle, e: &Entry, st: Row) -> Option<Act> {
  let line = first_line(&e.text);
  let caption = details(e, st.selected);
  // A click anywhere in the row selects it; a selected row is tinted but keeps its one line.
  let mut fs = *fs;
  fs.row = fs.row.hit();
  if st.selected {
    fs.row = fs.row.bg(ui.theme().ghost);
  }

  let text = Id::of((TEXT, e.id));
  let (mut act, mut on_text) = (None, false);
  // Over the text, the line and the details: a click there selects too.
  let cover = |ui: &mut Ui| {
    let at = Float::parent().clip();
    on_text = ui.with_id(text).float(at).el(Style::new().w(grow()).h(grow())).clicked;
    ui.tooltip(text, &e.text);
  };
  let audio = e.audio_path.is_some();
  let row = FormRow::new(&line).caption(&caption).one_line();
  let r = ui.id((ROW, e.id)).form_row_with(&fs, row, cover, |ui| {
    let r = ui.disabled(!audio).id((PLAY, e.id));
    let r = r.icon_button(if st.playing { Icon::STOP } else { Icon::PLAY });
    ui.tooltip(r.id, if st.playing { "Stop" } else { "Play the recording" });
    if r.clicked {
      act = Some(if st.playing { Act::Stop } else { Act::Play(e.id) });
    }

    let r = ui.id((COPY, e.id)).icon_button(Icon::COPY);
    ui.tooltip(r.id, "Copy");
    if r.clicked {
      act = Some(Act::Copy(e.text.clone()));
    }

    let off = st.retranscribing || st.no_model || !audio;
    let r = ui.disabled(off).id((RETRANSCRIBE, e.id)).icon_button(Icon::RESET);
    let tip = match (st.retranscribing, st.no_model) {
      (true, _) => "Transcribing…",
      (false, true) => "Download a model to transcribe it",
      (false, false) => "Transcribe again",
    };
    ui.tooltip(r.id, tip);
    if r.clicked {
      act = Some(Act::Retranscribe(e.id));
    }

    let r = ui.id((DELETE, e.id)).icon_button(Icon::TRASH);
    ui.tooltip(r.id, "Delete…");
    if r.clicked {
      act =
        Some(Act::Ask(Ask::Dictation { id: e.id, start: shorten(&e.text, START_CHARS), audio }));
    }
  });
  act.or((r.clicked || on_text).then_some(Act::Select(e.id)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn durations_and_days() {
    assert_eq!(duration_text(3_240), "3.2 s");
    assert_eq!(duration_text(125_000), "2:05");
    assert_eq!(previous_day((2026, 3, 1)), (2026, 2, 28));
    assert_eq!(previous_day((2024, 3, 1)), (2024, 2, 29));
    assert_eq!(previous_day((2026, 1, 1)), (2025, 12, 31));
    let today = (2026, 10, 18);
    assert_eq!(day_title(today, today), "Today");
    assert_eq!(day_title((2026, 10, 17), today), "Yesterday, October 17");
    assert_eq!(day_title((2026, 3, 3), today), "March 3");
    assert_eq!(day_title((2025, 3, 3), today), "March 3, 2025");
  }

  #[test]
  fn the_start_of_a_text_for_the_dialog() {
    let text_start = |t: &str| shorten(t, START_CHARS);
    assert_eq!(text_start("  Short\nnote. "), "Short note.");
    let long = "Remind me to send the invoice to Anna before Friday, and the contract, signed.";
    assert_eq!(text_start(long), "Remind me to send the invoice to Anna before Friday, and the…");
    assert_eq!(text_start(&"a".repeat(80)), format!("{}…", "a".repeat(60)));
    let ru = "Отдельно я хотел бы, чтобы ты импортировал эту штуку отдельным экшеном.";
    assert_eq!(text_start(ru), "Отдельно я хотел бы, чтобы ты импортировал эту штуку…");
  }
}
