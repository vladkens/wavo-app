// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The History page's toolbar, one row above its search: Keep (the history's retention), then
//! Import from Handy, shown when Handy's history is on this computer
//! ([`crate::transfer::handy::found`], looked at off the main thread, what it would bring in its
//! tooltip); Export…, which writes the history into the Downloads folder (nimbo has no file picker)
//! and shows the archive there; and Show Recordings. How an import or an export goes is said in the
//! search row ([`Prefs::data_note`]). Importing an archive is `wavo-app import <file>` for now.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nimbo::{Align, Select, Style, Ui, grow, paths};

use super::Prefs;
use super::general::{folder_caption, thousands};
use super::models::size_text;
use crate::history::recordings::day_of;
use crate::history::{Retention, local_time, now_ms};
use crate::transfer::Report;
use crate::transfer::archive::Exported;
use crate::transfer::handy::Summary;

pub const RETENTION: &str = "retention";
pub const IMPORT: &str = "import-handy";
pub const EXPORT: &str = "export";
pub const RECORDINGS: &str = "recordings-folder";

const RETENTIONS: [Retention; 7] = [
  Retention::Unlimited,
  Retention::Count(100),
  Retention::Count(1000),
  Retention::Count(10_000),
  Retention::Days3,
  Retention::Weeks2,
  Retention::Months3,
];

/// How an import from Handy goes, as the shell tells it.
#[derive(Debug, Clone, PartialEq)]
pub enum Import {
  /// Recordings copied of how many.
  Copying(usize, usize),
  Done(Result<Report, String>),
}

/// How an export goes.
#[derive(Debug, Clone, PartialEq)]
enum Export {
  Running,
  /// The archive, and what it holds.
  Done(Result<(PathBuf, Exported), String>),
}

/// What the shell's threads hand back, and the window shows.
#[derive(Default)]
struct Shared {
  /// Handy's history, once found.
  handy: Option<Summary>,
  import: Option<Import>,
  export: Option<Export>,
  /// The export started after the import: its note is the one shown.
  export_last: bool,
}

/// The toolbar's state; the shell's threads fill it and wake the window.
#[derive(Default)]
pub struct State {
  shared: Arc<Mutex<Shared>>,
  /// Handy's history is asked for: not again until the window looks again.
  pub asked: bool,
}

/// After "Keep:": `Forever`, `10,000 dictations`, `2 weeks`.
fn retention_title(r: Retention) -> String {
  match r {
    Retention::Unlimited => "Forever".into(),
    Retention::Count(n) => format!("{} dictations", thousands(n)),
    Retention::Days3 => "3 days".into(),
    Retention::Weeks2 => "2 weeks".into(),
    Retention::Months3 => "3 months".into(),
  }
}

/// `March 3, 2025`.
fn date(ms: i64) -> String {
  local_time(ms).map_or_else(String::new, |t| format!("{} {}, {}", t.month_name(), t.day, t.year))
}

/// What Handy's history would bring: `1,204 dictations from March 3, 2025 to October 20, 2026,
/// 1,180 recordings · 2.1 GB`.
fn summary_text(s: &Summary) -> String {
  let what = if s.entries == 1 { "dictation" } else { "dictations" };
  let mut text = format!("{} {what}", thousands(s.entries));
  if let (Some(first), Some(last)) = (s.first, s.last) {
    text += &format!(" from {} to {}", date(first), date(last));
  }

  format!("{text}, {} recordings · {}", thousands(s.recordings), size_text(s.bytes))
}

/// How an import went.
fn report_text(r: &Report) -> String {
  let mut text = format!("Imported {} dictations", thousands(r.added));
  if r.entries > r.added {
    text += &format!(" ({} were here already)", thousands(r.entries - r.added));
  }

  text += &format!(", {} recordings", thousands(r.copied));
  if r.failed > 0 {
    text += &format!("; {} could not be copied, the next import tries again", thousands(r.failed));
  }

  text + "."
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
  /// Asks the shell for Handy's history the first time the page shows after a refresh.
  fn handy(&mut self, ui: &Ui) -> Option<Summary> {
    if !self.data.asked {
      self.data.asked = true;
      let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
      self.shell.handy(Box::new(move |found| {
        shared.lock().unwrap().handy = found;
        waker.wake();
      }));
    }

    self.data.shared.lock().unwrap().handy.clone()
  }

  /// How the last import or export goes, for the search row, and whether it failed: `Importing
  /// from Handy: 420 of 1,180 recordings…`, `Exported 3 dictations · 1 MB to ~/Downloads/…`.
  pub fn data_note(&self) -> Option<(String, bool)> {
    let shared = self.data.shared.lock().unwrap();
    let import = || match shared.import.as_ref()? {
      Import::Copying(done, total) => Some((
        format!("Importing from Handy: {} of {} recordings…", thousands(done), thousands(total)),
        false,
      )),
      Import::Done(Ok(r)) => Some((report_text(r), false)),
      Import::Done(Err(e)) => Some((format!("Couldn't import: {e}"), true)),
    };
    let export = || match shared.export.as_ref()? {
      Export::Running => Some(("Exporting…".to_owned(), false)),
      Export::Done(Ok((file, done))) => {
        let (n, size) = (thousands(done.entries), size_text(done.bytes));
        Some((format!("Exported {n} dictations · {size} to {}", paths::display(file)), false))
      }
      Export::Done(Err(e)) => Some((format!("Couldn't export: {e}"), true)),
    };
    match shared.export_last {
      true => export().or_else(import),
      false => import().or_else(export),
    }
  }

  /// The toolbar: Keep at the left; Import from Handy (when found), Export… and Show Recordings at
  /// the right, Import first so that its coming changes no other's place.
  pub(super) fn history_tools(&mut self, ui: &mut Ui, s: Style) {
    let mut all = RETENTIONS.to_vec();
    if !all.contains(&self.settings.history_retention) {
      all.insert(1, self.settings.history_retention);
    }

    let items: Vec<(usize, String)> =
      all.iter().enumerate().map(|(i, &r)| (i, retention_title(r))).collect();
    let items: Vec<(usize, &str)> = items.iter().map(|(i, l)| (*i, l.as_str())).collect();
    let mut at = all.iter().position(|&r| r == self.settings.history_retention).unwrap_or(0);
    let handy = self.handy(ui);
    let (importing, exporting) = {
      let shared = self.data.shared.lock().unwrap();
      let importing = matches!(shared.import, Some(Import::Copying(..)));
      (importing, matches!(shared.export, Some(Export::Running)))
    };
    let recordings = self.recordings.dir.clone();
    let about = folder_caption(&recordings, self.recordings_size(ui));
    let (mut keep, mut import, mut export, mut reveal) = (false, false, false, false);
    ui.row(s.align(Align::Start, Align::Center), |ui| {
      // The pop-up's label close after "Keep:", as one control.
      ui.row(Style::new().align(Align::Start, Align::Center), |ui| {
        ui.text("Keep:", ui.theme().label());
        keep = Select::new(&mut at, &items).show(ui.id(RETENTION)).changed;
      });
      ui.el(Style::new().w(grow()));
      if let Some(summary) = &handy {
        let r = ui.disabled(importing).id(IMPORT).button("Import from Handy");
        ui.tooltip(r.id, &summary_text(summary));
        import = r.clicked;
      }

      let r = ui.disabled(exporting).id(EXPORT).button("Export…");
      let tip = "Into Downloads as a .tar archive; to bring one back, run wavo-app import <file>";
      ui.tooltip(r.id, tip);
      export = r.clicked;
      let r = ui.disabled(!recordings.is_dir()).id(RECORDINGS).button("Show Recordings");
      ui.tooltip(r.id, &format!("{about}: the history’s recordings, a folder a day"));
      reveal = r.clicked;
    });
    if keep {
      let r = all[at];
      self.edit(move |s| s.history_retention = r);
    }

    if import && let Some(summary) = handy {
      self.import_handy(ui, summary.dir);
    }

    if export {
      self.export(ui);
    }

    if reveal {
      self.shell.reveal(&recordings);
    }
  }

  /// The shell imports Handy's history from `dir` and tells how it goes.
  fn import_handy(&mut self, ui: &Ui, dir: PathBuf) {
    self.history.note = None;
    let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
    let mut s = shared.lock().unwrap();
    (s.import, s.export_last) = (Some(Import::Copying(0, 0)), false);
    drop(s);
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
    self.history.note = None;
    let file = export_file();
    let (shared, waker) = (self.data.shared.clone(), ui.waker().clone());
    let mut s = shared.lock().unwrap();
    (s.export, s.export_last) = (Some(Export::Running), true);
    drop(s);
    let done_file = file.clone();
    self.shell.export(
      file,
      Box::new(move |done| {
        shared.lock().unwrap().export = Some(Export::Done(done.map(|d| (done_file, d))));
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
    let r = Report { entries: 1_204, added: 1_204, copied: 1_180, failed: 0 };
    assert_eq!(report_text(&r), "Imported 1,204 dictations, 1,180 recordings.");
    let s = Summary {
      dir: PathBuf::new(),
      entries: 2,
      recordings: 1,
      bytes: 2_100_000,
      first: None,
      last: None,
    };
    assert_eq!(summary_text(&s), "2 dictations, 1 recordings · 2 MB");
    assert!(export_file().to_string_lossy().contains("/Downloads/wavo-export-"));
    assert_eq!(retention_title(Retention::Count(1000)), "1,000 dictations");
  }
}
