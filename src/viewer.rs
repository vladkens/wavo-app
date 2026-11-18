// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A transcript to read: its segments in paragraphs, each with the time it starts at, in a column
//! that builds only the paragraphs in view ([Ui::virtual_col]), so the thousand paragraphs of a
//! 4.5-hour lecture cost what a screenful does. Each paragraph's height is measured once, at the
//! column's width, as the layout wraps it. Its text can be selected, a paragraph at a time; a
//! transcript still running grows at its end, and the scroll stays where it is. The Files tab
//! shows it; meetings will.

use nimbo::{Align, FieldStyle, Id, Style, TextStyle, Ui, grow, px};

use crate::segments::{self, Segment, clock};

/// The times' column, room for `4:30:00`, and the room after it.
const TIME_W: f32 = 54.0;
const TIME_GAP: f32 = 12.0;
/// Under each paragraph.
const GAP: f32 = 12.0;

pub struct Viewer {
  /// The transcript's.
  pub id: i64,
  segments: Vec<Segment>,
  /// Each paragraph's start and text, none empty.
  paragraphs: Vec<(i64, String)>,
  /// The heights of the first paragraphs at `width`, the gap under them included: the rest are
  /// measured when shown next.
  heights: Vec<f32>,
  /// The column's, as laid out last.
  width: f32,
}

impl Viewer {
  pub fn new(id: i64) -> Self {
    Self { id, segments: Vec::new(), paragraphs: Vec::new(), heights: Vec::new(), width: 0.0 }
  }

  /// The transcript's segments now; the paragraphs that stay the same keep their heights.
  pub fn set(&mut self, segments: Vec<Segment>) {
    let paragraphs: Vec<(i64, String)> = (segments::paragraphs(&segments).into_iter())
      .map(|p| (segments[p.start].start_ms, segments::paragraph_text(&segments[p])))
      .filter(|(_, text)| !text.is_empty())
      .collect();
    let same = self.paragraphs.iter().zip(&paragraphs).take_while(|(a, b)| a == b).count();
    self.heights.truncate(same);
    (self.paragraphs, self.segments) = (paragraphs, segments);
  }

  pub fn segments(&self) -> &[Segment] {
    &self.segments
  }

  pub fn is_empty(&self) -> bool {
    self.paragraphs.is_empty()
  }

  /// The paragraphs as column `id`, filling the width it is given (`width` until it is laid out):
  /// the times in `time`, the text selectable in `text`. Inside a scroll area, which it builds
  /// only the shown part of.
  pub fn show(&mut self, ui: &mut Ui, id: Id, width: f32, text: &FieldStyle, time: TextStyle) {
    let width = ui.rect(id).map_or(width, |r| r.w);
    if width != self.width {
      (self.width, self.heights) = (width, Vec::new());
    }

    let line = ui.line_height(text.text);
    let text_w = (width - TIME_W - TIME_GAP).max(1.0);
    for (_, p) in &self.paragraphs[self.heights.len()..] {
      let h = ui.measure_wrapped(p, text.text, text_w).h.max(line);
      self.heights.push(h + GAP);
    }

    // Paragraph `i` is `id.key(i)`, its text `id.key(i).key("text")`.
    let paragraphs = &self.paragraphs;
    ui.with_id(id).virtual_col(
      self.heights.iter().copied(),
      |i| i,
      |ui, i| {
        let (start, p) = &paragraphs[i];
        let row = Style::new().w(grow()).gap(TIME_GAP).align(Align::Start, Align::Start);
        ui.row(row, |ui| {
          // On the first line's middle: the times are smaller.
          let at = Style::new().w(px(TIME_W)).h(px(line)).align(Align::End, Align::Center);
          ui.col(at, |ui| ui.text(clock(*start), time.nowrap()));
          let col = Style::new().w(grow());
          ui.col(col, |ui| _ = ui.with_id(id.key(i).key("text")).selectable_text(p, text));
        });
      },
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn seg(start_ms: i64, text: &str) -> Segment {
    Segment { start_ms, end_ms: start_ms + 1_000, speaker: None, text: text.into() }
  }

  /// A transcript that grows keeps the heights of its paragraphs but the last, which may grow too.
  #[test]
  fn a_growing_transcript_measures_only_its_new_end() {
    let mut v = Viewer::new(1);
    v.set(vec![seg(0, "One."), seg(5_000, "Two."), seg(10_000, "Three.")]);
    assert_eq!(v.paragraphs.len(), 3, "a pause apart");
    v.heights = vec![30.0; 3];

    v.set(vec![seg(0, "One."), seg(5_000, "Two."), seg(10_000, "Three."), seg(11_000, "Four.")]);
    assert_eq!(v.paragraphs[2], (10_000, "Three. Four.".into()));
    assert_eq!(v.heights, [30.0; 2], "the last one is measured again");

    v.set(vec![seg(0, ""), seg(5_000, "Two.")]);
    assert_eq!(v.paragraphs, [(5_000, "Two.".into())], "none empty");
    assert!(v.heights.is_empty());
  }
}
