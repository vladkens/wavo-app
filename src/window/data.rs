// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The Advanced page's Data section: Import from Handy, shown when Handy's history is on this
//! computer ([`handy::found`], looked at off the main thread), with what it would bring and the
//! copies' progress in place of its caption while it runs; and Export, which writes the history
//! into the Downloads folder (nimbo has no file picker) and shows the archive there. Importing an
//! archive is `wavo-app import <file>` for now.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nimbo::{Align, FormRow, FormStyle, Icon, Style, Tile, Ui, grow, paths, px};

use super::Prefs;
use super::general::row as row_id;
use crate::history::{local_time, now_ms};
use crate::transfer::archive::Exported;
use crate::transfer::handy::Summary;
use crate::transfer::{self, Report};

pub const IMPORT: &str = "import-handy";
pub const EXPORT: &str = "export";

/// How an import from Handy goes, as the shell tells it.
#[derive(Debug, Clone, PartialEq)]
pub enum Import {
  /// Recordings copied of how many.
  Copying(usize, usize),
  Done(Result<Report, String>),
}

/// What the shell's threads hand back, and the window shows.
#[derive(Default)]
struct Shared {
  /// Handy's history: `None` until known.
  handy: Option<Option<Summary>>,
  import: Option<Import>,
  /// The export running (`None` inside), or how it went.
  export: Option<Option<Result<(PathBuf, Exported), String>>>,
}

/// The section's state; the shell's threads fill it and wake the window.
#[derive(Default)]
pub struct State {
  shared: Arc<Mutex<Shared>>,
  /// Handy's history is asked for: not again until the window looks again.
  pub asked: bool,
  /// The history the last import brought is loaded.
  seen: bool,
}

/// `March 3, 2025`.
fn date(ms: i64) -> String {
  local_time(ms).map_or_else(String::new, |t| format!("{} {}, {}", t.month_name(), t.day, t.year))
}

/// What Handy's history would bring: `1,204 dictations from March 3, 2025 to October 20, 2026,
/// 1,180 recordings · 2.1 GB`.
fn summary_text(s: &Summary) -> String {
  let what = if s.entries == 1 { "dictation" } else { "dictations" };
  let mut text = format!("{} {what}", s.entries);
  if let (Some(first), Some(last)) = (s.first, s.last) {
    text += &format!(" from {} to {}", date(first), date(last));
  }

  format!("{text}, {} recordings · {}", s.recordings, transfer::size(s.bytes))
}

/// How an import went.
fn report_text(r: &Report) -> String {
  let mut text = format!("Imported {} dictations", r.added);
  if r.entries > r.added {
    text += &format!(" ({} were here already)", r.entries - r.added);
  }

  text += &format!(", {} recordings", r.copied);
  if r.failed > 0 {
    text += &format!("; {} could not be copied, the next import tries again", r.failed);
  }

  text + "."
}

/// A new archive in the Downloads folder: `wavo-export-2026-10-24.tar`, `-2` and so on if taken.
fn export_file() -> PathBuf {
  let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
  let day = local_time(now_ms())
    .map(|t| format!("{:04}-{:02}-{:02}", t.year, t.month, t.day))
    .unwrap_or_default();
  let dir = home.join("Downloads");
  (1..)
    .map(|n| match n {
      1 => dir.join(format!("wavo-export-{day}.tar")),
      n => dir.join(format!("wavo-export-{day}-{n}.tar")),
    })
    .find(|f| !f.exists())
    .expect("a free name")
}

impl Prefs {
  /// Asks the shell for Handy's history the first time the page shows after a refresh.
  fn handy(&mut self, ui: &Ui) -> Option<Summary> {
    if !self.data.asked {
      self.data.asked = true;
      let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
      self.shell.handy(Box::new(move |found| {
        shared.lock().unwrap().handy = Some(found);
        waker.wake();
      }));
    }

    self.data.shared.lock().unwrap().handy.clone().flatten()
  }

  pub(super) fn data(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    let handy = self.handy(ui);
    let (import, export) = {
      let shared = self.data.shared.lock().unwrap();
      (shared.import.clone(), shared.export.clone())
    };
    // The history the import brought shows on the History page.
    if matches!(import, Some(Import::Done(Ok(_)))) && !self.data.seen {
      self.data.seen = true;
      self.history.reload();
    }

    let (mut import_clicked, mut export_clicked) = (false, false);
    ui.form_section(fs, "Data", |ui| {
      if let Some(summary) = &handy {
        let (caption, color) = match &import {
          None | Some(Import::Copying(..)) => (summary_text(summary), None),
          Some(Import::Done(Ok(r))) => (report_text(r), None),
          Some(Import::Done(Err(e))) => (format!("Couldn't import: {e}"), Some(t.danger)),
        };
        let copying = match import {
          Some(Import::Copying(done, total)) => Some((done, total)),
          _ => None,
        };
        let mut row = FormRow::new("Import from Handy").tile(Tile::Icon(Icon::DOWNLOAD));
        if copying.is_none() {
          row = row.caption(&caption);
        }

        let tinted = color.map(|c| super::caption_in(fs, c));
        let below = |ui: &mut Ui| {
          if let Some((done, total)) = copying {
            let line = Style::new().w(grow()).h(px(fs.caption.line_h)).gap(8.0);
            ui.row(line.align(Align::Start, Align::Center), |ui| {
              let k = done as f32 / total.max(1) as f32;
              ui.col(Style::new().w(px(120.0)), |ui| ui.progress(Some(k)));
              ui.text(format!("Copying recordings: {done} of {total}"), fs.caption.nowrap());
            });
          }
        };
        let row_fs = tinted.as_ref().unwrap_or(fs);
        ui.with_id(row_id(IMPORT)).form_row_with(row_fs, row, below, |ui| {
          let busy = copying.is_some();
          import_clicked = ui.disabled(busy).id(IMPORT).button("Import").clicked;
        });
      }

      let (caption, color) = match &export {
        None => (
          "Into Downloads as a .tar archive; to bring one back, run wavo-app import <file>."
            .to_owned(),
          None,
        ),
        Some(None) => ("Exporting…".to_owned(), None),
        Some(Some(Ok((file, done)))) => {
          let size = transfer::size(done.bytes);
          (format!("{} · {} dictations · {size}", paths::display(file), done.entries), None)
        }
        Some(Some(Err(e))) => (format!("Couldn't export: {e}"), Some(t.danger)),
      };
      let tinted = color.map(|c| super::caption_in(fs, c));
      let row = FormRow::new("Export the history").caption(&caption).tile(Tile::Icon(Icon::DOC));
      ui.with_id(row_id(EXPORT)).form_row(tinted.as_ref().unwrap_or(fs), row, |ui| {
        let busy = matches!(export, Some(None));
        export_clicked = ui.disabled(busy).id(EXPORT).button("Export").clicked;
      });
    });

    if import_clicked && let Some(summary) = handy {
      self.import_handy(ui, summary.dir);
    }

    if export_clicked {
      self.export(ui);
    }
  }

  /// The shell imports Handy's history from `dir` and tells how it goes.
  fn import_handy(&mut self, ui: &Ui, dir: PathBuf) {
    self.data.seen = false;
    let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
    shared.lock().unwrap().import = Some(Import::Copying(0, 0));
    self.shell.import_handy(
      dir,
      Box::new(move |import| {
        shared.lock().unwrap().import = Some(import);
        waker.wake();
      }),
    );
  }

  /// The shell writes the archive into the Downloads folder; the window shows it there.
  fn export(&mut self, ui: &Ui) {
    let file = export_file();
    let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
    shared.lock().unwrap().export = Some(None);
    let done_file = file.clone();
    self.shell.export(
      file,
      Box::new(move |done| {
        shared.lock().unwrap().export = Some(Some(done.map(|d| (done_file, d))));
        waker.wake();
      }),
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn what_an_import_says() {
    let r = Report { entries: 10, added: 7, copied: 6, failed: 1 };
    let want = "Imported 7 dictations (3 were here already), 6 recordings; 1 could not be copied, \
      the next import tries again.";
    assert_eq!(report_text(&r), want);
    let s = Summary {
      dir: PathBuf::new(),
      entries: 2,
      recordings: 1,
      bytes: 2_100_000,
      first: None,
      last: None,
    };
    assert_eq!(summary_text(&s), "2 dictations, 1 recordings · 2.1 MB");
    assert!(export_file().to_string_lossy().contains("/Downloads/wavo-export-"));
  }
}
