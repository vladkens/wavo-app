// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The pieces rows are made of over nimbo's form, which no single widget covers: the height of
//! its two lines, a line of progress with its caption, and the head of a list's plate.

use nimbo::{Align, FormRow, FormStyle, Id, Style, TextStyle, Ui, grow, px};

use crate::ui::PROGRESS_W;

/// A row of a name and a line under it: the padding, the two lines and nimbo's 2 points between.
pub fn two_line_height(ui: &mut Ui, fs: &FormStyle) -> f32 {
  2.0 * fs.pad_y + ui.line_height(fs.label) + 2.0 + ui.line_height(fs.caption)
}

/// A progress bar, moving without a known fraction, and its text, as high as a caption's line.
pub fn progress_line(ui: &mut Ui, fs: &FormStyle, fraction: Option<f32>, text: &str) {
  let line = Style::new().w(grow()).h(px(ui.line_height(fs.caption))).gap(8.0);
  ui.row(line.align(Align::Start, Align::Center), |ui| {
    ui.col(Style::new().w(px(PROGRESS_W)), |ui| ui.progress(fraction));
    ui.text(text, fs.caption.nowrap());
  });
}

/// The head of a list's plate: its title, and a note at its right, on one line, as a day's in
/// History and the downloaded models' in Models. The caller's row places and aligns it.
pub fn list_head(ui: &mut Ui, fs: &FormStyle, title: &str, note: &str) {
  ui.text(title, fs.head);
  ui.spacer();
  ui.text(note, fs.note);
}

/// What a row of work that runs shows: its caption, else its progress while it runs. A failure
/// is said in the caption, in red.
pub struct DataRowState {
  pub caption: Option<String>,
  /// How far it is (0..1; `None` without a known total), and the text beside the bar.
  pub progress: Option<(Option<f32>, String)>,
  pub failed: bool,
}

/// A form row with what it [DataRowState]: the progress under the title, a failure in red.
pub fn data_row<'a>(
  ui: &mut Ui,
  fs: &FormStyle,
  id: Id,
  row: FormRow<'a>,
  state: &'a DataRowState,
  controls: impl FnOnce(&mut Ui),
) {
  let danger = ui.theme().danger;
  let caption = move |ui: &mut Ui, style: TextStyle| {
    if let Some(caption) = &state.caption {
      // A failure in red; cut short, it shows whole in its tooltip.
      let style = if state.failed { style.color(danger) } else { style };
      if state.failed {
        ui.help(caption);
      }

      ui.with_id(id.key("caption")).text(caption, style);
    }

    if let Some((fraction, text)) = &state.progress {
      progress_line(ui, fs, *fraction, text);
    }
  };
  ui.with_id(id).form_row(fs, row.caption(caption), controls);
}
