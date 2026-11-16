// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The rows a setting is made of: a switch, a choice of options, and the label both carry. Each
//! reads the setting's value and answers with the new one, `None` while it stays as it was, so a
//! page reads as `if let Some(is_on) = ui.toggle_row(..) { self.set(..) }` and no page repeats
//! the "read a copy, see if it changed, edit" dance.

use std::fmt::Display;

use nimbo::{FormRow, FormStyle, Id, Picker, Tag, TextStyle, Ui};

/// What a row's mark says: nothing, an ⓘ for what one line can't explain, or what is wrong with
/// the setting, in the ⓘ's place.
pub enum RowMark<'s> {
  None,
  Info(&'s str),
  Warning(&'s str),
}

/// A row's label: `name`, then its mark with its tip, the ⓘ named `tip_id` when given.
pub fn row_label<'s>(
  name: impl Display + 's,
  mark: RowMark<'s>,
  tip_id: Option<Id>,
) -> impl FnOnce(&mut Ui, TextStyle) + 's {
  move |ui: &mut Ui, style: TextStyle| {
    ui.text(name, style);
    let help = |ui: &mut Ui, tip: &str| match tip_id {
      Some(tip_id) => _ = ui.help(tip).with_id(tip_id),
      None => _ = ui.help(tip),
    };
    match mark {
      RowMark::None => {}
      RowMark::Info(tip) => {
        help(ui, tip);
        ui.info_button();
      }
      RowMark::Warning(tip) => {
        help(ui, tip);
        ui.warning_button();
      }
    }
  }
}

/// The rows over [Ui], as nimbo's widgets are: a switch and a picker, each with its [FormRow].
pub trait Rows {
  /// A switch `id` at the right of `row`, on while `is_on`: the value it is left at, `None` when
  /// the person didn't change it.
  fn toggle_row<'s>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    is_on: bool,
  ) -> Option<bool>;

  /// A picker `id` over `options`, at the right of `row`, on `current`: the option chosen, `None`
  /// when the person didn't change it.
  fn choice_row<'s, T: PartialEq + Copy>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    current: T,
    options: &[Tag<'_, T>],
  ) -> Option<T>;

  /// A picker `id` over two groups of options, `first_options` above a divider, `more_options`
  /// below it, as macOS puts Recent above its fonts: the option chosen, `None` when the person
  /// didn't change it.
  fn split_choice_row<'s, T: PartialEq + Copy>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    current: T,
    first_options: &[Tag<'_, T>],
    more_options: &[Tag<'_, T>],
  ) -> Option<T>;
}

impl Rows for Ui<'_> {
  fn toggle_row<'s>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    is_on: bool,
  ) -> Option<bool> {
    let (mut value, mut changed) = (is_on, false);
    self.form_row(fs, row, |ui| changed = ui.id(id).toggle(&mut value, "").changed);
    changed.then_some(value)
  }

  fn choice_row<'s, T: PartialEq + Copy>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    current: T,
    options: &[Tag<'_, T>],
  ) -> Option<T> {
    self.split_choice_row(fs, id, row, current, options, &[])
  }

  fn split_choice_row<'s, T: PartialEq + Copy>(
    &mut self,
    fs: &FormStyle,
    id: &str,
    row: FormRow<'s>,
    current: T,
    first_options: &[Tag<'_, T>],
    more_options: &[Tag<'_, T>],
  ) -> Option<T> {
    let (mut value, mut changed) = (current, false);
    self.form_row(fs, row, |ui| {
      let picker = Picker::new(&mut value).options(first_options);
      let picker =
        if more_options.is_empty() { picker } else { picker.divider().options(more_options) };
      changed = picker.show(ui.id(id)).changed;
    });
    changed.then_some(value)
  }
}

/// A picker of `labels` by their place, a divider after each of `dividers`.
pub fn index_picker<'s>(
  current: &'s mut usize,
  labels: impl Iterator<Item = &'s str>,
  dividers: &[usize],
) -> Picker<'s, usize> {
  let mut picker = Picker::new(current);
  for (i, label) in labels.enumerate() {
    picker = picker.option(i, label);
    if dividers.contains(&i) {
      picker = picker.divider();
    }
  }

  picker
}

/// A picker `id` over `labels` by their place, `current` the chosen one, a divider after each of
/// `dividers`: the place chosen, `None` when the person didn't change it.
pub fn pick_index(
  ui: &mut Ui,
  id: &str,
  labels: &[String],
  current: usize,
  dividers: &[usize],
) -> Option<usize> {
  let mut chosen = current;
  let picker = index_picker(&mut chosen, labels.iter().map(String::as_str), dividers);
  picker.show(ui.id(id)).changed.then_some(chosen)
}
