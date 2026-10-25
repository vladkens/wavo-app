// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The Advanced page's Data section: Export, which writes the history into the Downloads folder
//! (nimbo has no file picker) and shows the archive there. Importing an archive is
//! `wavo-app import <file>` for now; Handy's history comes over with scripts/handy.py.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nimbo::{FormRow, FormStyle, Icon, Tile, Ui, paths};

use super::Prefs;
use super::general::{row as row_id, thousands};
use super::models::size_text;
use crate::history::{day_of, now_ms};
use crate::transfer::Exported;

pub const EXPORT: &str = "export";

/// How an export goes.
#[derive(Debug, Clone, PartialEq)]
enum Export {
  Running,
  /// The archive, and what it holds.
  Done(Result<(PathBuf, Exported), String>),
}

/// The section's state; the shell's thread fills it and wakes the window.
#[derive(Default)]
pub struct State {
  export: Arc<Mutex<Option<Export>>>,
}

/// A new archive in the Downloads folder: `wavo-export-2026-10-24.tar`, `-2` and so on if taken
/// or being written.
fn export_file() -> PathBuf {
  let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
  let day = day_of(now_ms()).unwrap_or_default();
  let dir = home.join("Downloads");
  (1..)
    .map(|n| match n {
      1 => dir.join(format!("wavo-export-{day}.tar")),
      n => dir.join(format!("wavo-export-{day}-{n}.tar")),
    })
    .find(|f| !f.exists() && !crate::transfer::part_of(f).exists())
    .expect("a free name")
}

impl Prefs {
  pub(super) fn data(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let export = self.data.export.lock().unwrap().clone();
    let mut export_clicked = false;
    ui.form_section(fs, "Data", |ui| {
      let (caption, color) = match &export {
        None => (
          "Into Downloads as a .tar archive; to bring one back, run wavo-app import <file>."
            .to_owned(),
          None,
        ),
        Some(Export::Running) => ("Exporting…".to_owned(), None),
        Some(Export::Done(Ok((file, done)))) => {
          let (file, n) = (paths::display(file), thousands(done.entries));
          (format!("{file} · {n} dictations · {}", size_text(done.bytes)), None)
        }
        Some(Export::Done(Err(e))) => (format!("Couldn't export: {e}"), Some(t.danger)),
      };
      let tinted = color.map(|c| super::caption_in(fs, c));
      let row = FormRow::new("Export the history").caption(&caption).tile(Tile::Icon(Icon::DOC));
      ui.with_id(row_id(EXPORT)).form_row(tinted.as_ref().unwrap_or(fs), row, |ui| {
        let busy = matches!(export, Some(Export::Running));
        export_clicked = ui.disabled(busy).id(EXPORT).button("Export").clicked;
      });
    });

    if export_clicked {
      self.export(ui);
    }
  }

  /// The shell writes the archive into the Downloads folder; the window shows it there.
  fn export(&mut self, ui: &Ui) {
    let file = export_file();
    let (shared, waker) = (self.data.export.clone(), ui.waker().clone());
    *shared.lock().unwrap() = Some(Export::Running);
    let done_file = file.clone();
    self.shell.export(
      file,
      Box::new(move |done| {
        *shared.lock().unwrap() = Some(Export::Done(done.map(|d| (done_file, d))));
        waker.wake();
      }),
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_export_goes_into_downloads() {
    assert!(export_file().to_string_lossy().contains("/Downloads/wavo-export-"));
  }
}
