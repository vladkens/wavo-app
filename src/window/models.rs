// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The Models page: how long a model stays loaded and where the models are, then wavo's models
//! ([`hub::MODELS`]) in two plates, those downloaded into the Hugging Face cache and the rest
//! available, each in wavo's order (a model keeps its place when another is used), under a search
//! field (by name, family, maker and language: [`Search`]) and a language pop-up. The model in use
//! is marked "✓ In use" in its place with its state, and stays shown whatever the search. Use picks
//! a downloaded model, the bin deletes one after asking in the window's dialog, with the size it
//! frees (for the model in use, what takes its place), Download fetches one with its progress in
//! place of its caption and ✕ cancels, ⓘ opens the original model's card. A chosen model whose
//! file is gone stays in its place among the downloaded, "Not on disk" in the warning colour,
//! with Download. Every row is one line of name and one of caption in every state (idle,
//! downloading, preparing, failed, deleting, in use): the list never moves under the pointer.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use isolang::Language;
use nimbo::{Align, FormRow, FormStyle, Icon, Id, Style, Tile, Ui, Weight, grow, open, px};
use tracing::{info, warn};
use wavo::hub::{self, Hub, ModelInfo};

use super::downloads::Download;
use super::general::{folder_caption, pick, row as row_id};
use super::{Ask, Prefs, Problem};
use crate::engine::supervisor::Phase as EnginePhase;
use crate::history::fold;
use crate::language;
use crate::session::controller::{NoModel, Phase, label};
use crate::settings::Settings;

/// The search field and the language pop-up above the plates.
pub const SEARCH: &str = "model-search";
pub const LANG: &str = "model-lang";
/// A model's row, `(ROW, name)`, and its buttons: `(USE, name)` and so on; `(INFO, name)` its
/// card.
pub const ROW: &str = "model";
pub const USE: &str = "use";
pub const DOWNLOAD: &str = "download";
pub const CANCEL: &str = "cancel";
pub const DELETE: &str = "delete";
pub const INFO: &str = "info";
/// The unload pop-up and the model folder's button, above the plates.
pub const UNLOAD: &str = "unload";
pub const FOLDER: &str = "model-folder";

const UNLOADS: [Option<u32>; 7] = [None, Some(0), Some(2), Some(5), Some(10), Some(15), Some(60)];
const UNLOAD_HELP: &str = "Frees the model's memory after Wavo has been idle this long. The next \
  dictation loads it again, which takes a moment.";

/// The width of a download's progress bar, in place of its row's caption.
const BAR: f32 = 120.0;

/// What the page is doing.
#[derive(Default)]
pub struct State {
  /// What it says about the last action (a failed deletion).
  pub note: Option<String>,
  /// The models being deleted, by name.
  pub deleting: Vec<&'static str>,
  pub search: String,
  /// The language filter: an index of [`LANGUAGES`], 0 for all.
  pub lang: usize,
}

pub fn size_text(bytes: u64) -> String {
  let mb = bytes as f64 / 1e6;
  if mb >= 1000.0 { format!("{:.1} GB", mb / 1000.0) } else { format!("{mb:.0} MB") }
}

/// `114 of 272 MB`, `0.4 of 1.7 GB`.
fn done_text(done: u64, total: u64) -> String {
  let (d, t) = (done as f64, total as f64);
  match total >= 1_000_000_000 {
    true => format!("{:.1} of {:.1} GB", d / 1e9, t / 1e9),
    false => format!("{:.0} of {:.0} MB", d / 1e6, t / 1e6),
  }
}

/// A downloaded model: wavo's, and its file.
pub type Found = (&'static ModelInfo, PathBuf);

/// The models `found` on disk that are ready: not those still downloading or being prepared for
/// the Neural Engine (its file is there already), which `state` tells.
pub fn ready(mut found: Vec<Found>, state: impl Fn(&str) -> Option<Download>) -> Vec<Found> {
  found
    .retain(|f| !matches!(state(f.0.name), Some(Download::Running { .. } | Download::Preparing)));
  found
}

/// Which model transcribes, as the session picks it: the chosen one when it is on disk, else the
/// first downloaded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pick {
  /// Its index among the downloaded; `None` for a file of its own outside the cache, or none.
  pub active: Option<usize>,
  /// The chosen model when it is none of the downloaded: what it is called, and whether its file
  /// is there (outside the cache) or gone.
  pub other: Option<(String, bool)>,
}

impl Pick {
  /// The pick of `query` (the settings' model) among `found`, the models downloaded into `hub`.
  pub fn new(hub: &Hub, query: Option<&str>, found: &[Found]) -> Self {
    let first = (!found.is_empty()).then_some(0);
    let Some(query) = query else { return Self { active: first, other: None } };
    match hub.resolve(query) {
      Ok(path) => match found.iter().position(|f| f.1 == path) {
        Some(i) => Self { active: Some(i), other: None },
        None => Self { active: None, other: Some((label(query), true)) },
      },
      Err(_) => Self { active: first, other: Some((label(query), false)) },
    }
  }

  /// The chosen model when it is not on disk: what it is called.
  pub fn gone(&self) -> Option<&str> {
    self.other.as_ref().filter(|(_, there)| !there).map(|(name, _)| name.as_str())
  }
}

/// How long the model stays loaded, after "Unload the model after": `Each dictation`, `5 minutes`.
fn unload_title(minutes: Option<u32>) -> String {
  match minutes {
    None => "Never".into(),
    Some(0) => "Each dictation".into(),
    Some(1) => "1 minute".into(),
    Some(m) => format!("{m} minutes"),
  }
}

/// Whether model choice `query` names `model` in `hub`: by its name, or by the path of its file
/// there (a file of the same name elsewhere is another).
fn names(hub: &Hub, query: &str, model: &ModelInfo) -> bool {
  match super::lookup(query) {
    Some(m) => m.name == model.name,
    None => {
      let path = Path::new(query);
      path.starts_with(hub.root()) && path.file_name().is_some_and(|f| f == model.file().as_str())
    }
  }
}

/// `settings` once `model` is deleted from `hub`: a choice that names it goes back to the first
/// model found.
pub fn forget(settings: &mut Settings, hub: &Hub, model: &ModelInfo) {
  if settings.model.as_deref().is_some_and(|q| names(hub, q, model)) {
    settings.model = None;
  }
}

/// Deletes `model` from the cache in `hub`: `Hub::remove`, which waits while another process
/// downloads the model, so never on the main thread. The bytes freed, or why not.
pub fn delete(hub: &Hub, model: &ModelInfo) -> Result<u64, String> {
  match hub.remove(model.name) {
    Ok(Some(freed)) => {
      info!("settings: deleted {} ({freed} bytes)", model.name);
      Ok(freed)
    }
    Ok(None) => Err("it is not downloaded".into()),
    Err(e) => {
      warn!("delete {}: {e}", model.name);
      Err(e.to_string())
    }
  }
}

/// Every language of wavo's models by name, after "All languages": the language filter's
/// choices, `(label, code)`.
pub static LANGUAGES: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
  let mut codes: Vec<&str> = hub::MODELS.iter().flat_map(|m| m.langs.iter().copied()).collect();
  codes.sort_unstable();
  codes.dedup();
  let mut all: Vec<(String, &str)> = codes.into_iter().map(|c| (language::name(c), c)).collect();
  all.sort();
  all.insert(0, ("All languages".to_owned(), ""));
  all
});

/// The codes of the languages search word `w` (folded) names: by code, ISO 639-3 code (`rus`) or
/// the start of a word of the name (`russ`).
fn languages_named(w: &str) -> Vec<&'static str> {
  let named = |&(ref name, code): &(String, &str)| {
    let long = Language::from_639_1(code).map(|l| l.to_639_3());
    code.eq_ignore_ascii_case(w)
      || long == Some(w)
      || fold(name).split_whitespace().any(|n| n.starts_with(w))
  };
  LANGUAGES.iter().skip(1).filter(|l| named(l)).map(|l| l.1).collect()
}

/// What the search field asks for: its words, any case, each with the languages it names.
pub struct Search {
  words: Vec<(String, Vec<&'static str>)>,
}

impl Search {
  pub fn new(text: &str) -> Self {
    let text = fold(text);
    let words = text.split_whitespace().map(|w| (w.to_owned(), languages_named(w))).collect();
    Self { words }
  }

  /// Whether it finds model `m`: every word is in its name, starts its family's name, a word of
  /// its maker's or of its languages' line (`European`), or names one of its languages.
  pub fn finds(&self, m: &ModelInfo) -> bool {
    let (name, family, maker, about) =
      (fold(m.full_name), fold(m.family), fold(m.maker), fold(m.languages));
    let starts = |text: &str, w: &str| text.split_whitespace().any(|t| t.starts_with(w));
    self.words.iter().all(|(w, langs)| {
      name.contains(w.as_str())
        || m.name.contains(w.as_str())
        || family.starts_with(w.as_str())
        || starts(&maker, w)
        || starts(&about, w)
        || m.langs.iter().any(|c| langs.contains(c))
    })
  }
}

/// What a row asked for.
enum Act {
  Use(&'static str),
  Ask(Ask),
  Card(&'static ModelInfo),
  Download(&'static ModelInfo),
  Cancel(&'static str),
}

impl Prefs {
  /// The search field and the language pop-up, above the scrolling plates.
  pub(super) fn models_head(&mut self, ui: &mut Ui, s: Style) {
    let items: Vec<(usize, &str)> = LANGUAGES.iter().enumerate().map(|(i, l)| (i, &*l.0)).collect();
    let m = &mut self.models;
    ui.row(s.align(Align::Start, Align::Center), |ui| {
      ui.id(SEARCH).search(&mut m.search, "Search models");
      ui.id(LANG).select(&mut m.lang, &items);
    });
  }

  /// Above the plates: how long the model stays loaded, and the models' folder (its path and size
  /// in the ⓘ).
  fn options(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let mut unloads = UNLOADS.to_vec();
    if !unloads.contains(&self.settings.unload_after_minutes) {
      unloads.push(self.settings.unload_after_minutes);
    }

    let labels: Vec<String> = unloads.iter().map(|&u| unload_title(u)).collect();
    let dir = self.hub.root().to_owned();
    let size: u64 = self.found.iter().map(|f| f.0.size).sum();
    let about = format!(
      "{}: the Hugging Face cache, which other apps may share.",
      folder_caption(&dir, Some(size))
    );
    let mut reveal = false;
    ui.form_section(fs, "", |ui| {
      let row = FormRow::new("Unload the model after").info(UNLOAD_HELP);
      ui.with_id(row_id(UNLOAD)).form_row(fs, row, |ui| {
        let current = self.settings.unload_after_minutes;
        let at = unloads.iter().position(|&u| u == current).unwrap_or(0);
        if let Some(i) = pick(ui, UNLOAD, &labels, at, &[0]) {
          let minutes = unloads[i];
          self.edit(move |s| s.unload_after_minutes = minutes);
        }
      });
      let row = FormRow::new("Model folder").info(&about);
      ui.with_id(row_id(FOLDER)).form_row(fs, row, |ui| {
        let show = format!("Show in {}", open::file_manager());
        reveal = ui.disabled(!dir.is_dir()).id(FOLDER).button(&show).clicked;
      });
    });
    if reveal {
      self.shell.reveal(&dir);
    }
  }

  /// What the app does with model `m`, the one in use: `Loaded`, `Loading…`, `Recording…`, or a
  /// problem (`true`): it does not load.
  fn in_use_state(&self, m: &Found) -> (&'static str, bool) {
    let Some(status) = &self.status else { return ("Ready", false) };
    if let Some(NoModel::LoadFailed { model, .. }) = &status.no_model
      && model == m.0.full_name
    {
      return ("Could not load", true);
    }

    // The engine's phase is this model's only when it runs this model.
    let stem = m.1.file_stem().and_then(|s| s.to_str());
    if status.engine.model.as_deref() != stem {
      return ("Ready", false);
    }

    let loading = status.engine.phase == EnginePhase::Loading;
    let state = match status.state {
      Phase::Recording => "Recording…",
      Phase::Transcribing | Phase::Idle if loading => "Loading…",
      Phase::Transcribing => "Transcribing…",
      Phase::Pasting => "Pasting…",
      Phase::Idle if status.engine.phase == EnginePhase::Stopped => "Ready",
      Phase::Idle => "Loaded",
    };
    (state, false)
  }

  /// The options, then wavo's models in two plates, each row one line for the name and one for its
  /// caption (or a download's progress in its place): every row has the same height in every
  /// state.
  pub(super) fn models(&mut self, ui: &mut Ui, fs: &FormStyle) {
    let t = ui.theme();
    // The notes above the plates wrap; the rows' captions stay on their line.
    let note = fs.caption;
    let fs = &FormStyle { caption: fs.caption.ellipsis(), ..*fs };
    if let Some(text) = &self.models.note {
      ui.text(text, note.color(t.danger));
    }

    if let Some(problem @ Problem::LoadFailed { .. }) = self.model_problem() {
      ui.text(problem.text(), note.color(t.warning));
    }

    self.options(ui, fs);
    let found = &self.found;
    let active = self.active().map(|f| f.0.name);
    // The chosen model when it is gone from the disk: in its place among the downloaded.
    let gone =
      self.pick.gone().and_then(|_| self.settings.model.as_deref().and_then(super::lookup));
    let mut act = None;

    let search = Search::new(&self.models.search);
    let lang = LANGUAGES.get(self.models.lang).map_or("", |l| l.1);
    let shown = |m: &ModelInfo| search.finds(m) && (lang.is_empty() || m.langs.contains(&lang));
    let in_use = |m: &ModelInfo| active == Some(m.name);
    let is_gone = |m: &ModelInfo| gone.is_some_and(|g| g.name == m.name);
    let downloaded: Vec<&Found> = found.iter().filter(|f| in_use(f.0) || shown(f.0)).collect();
    if found.is_empty() && gone.is_none() {
      ui.form_section(fs, "Downloaded", |ui| {
        let row = FormRow::new("No model yet")
          .caption("Download one below: dictation starts working once it is here.")
          .tile(Tile::Icon(Icon::DOWNLOAD));
        ui.form_row(fs, row, |_| {});
      });
    } else if !downloaded.is_empty() || gone.is_some() {
      let size: u64 = downloaded.iter().map(|f| f.0.size).sum();
      let note = format!("{} · {}", count(downloaded.len(), "model"), size_text(size));
      // Only the model gone from the disk: nothing to count.
      let head = |ui: &mut Ui| {
        if !downloaded.is_empty() {
          ui.text(&note, fs.note);
        }
      };
      ui.form_section_with(fs, "Downloaded", head, |ui| {
        // In wavo's order, the chosen model that is gone in its place.
        for info in hub::MODELS {
          let a = match downloaded.iter().find(|f| f.0.name == info.name) {
            Some(f) => self.downloaded(ui, fs, f, in_use(info)),
            None if is_gone(info) => self.remote(ui, fs, info, true),
            None => None,
          };
          if let Some(a) = a {
            act = Some(a);
          }
        }
      });
    }

    let remote: Vec<&'static ModelInfo> = (hub::MODELS.iter())
      .filter(|m| !found.iter().any(|f| f.0.name == m.name) && !is_gone(m) && shown(m))
      .collect();
    if !remote.is_empty() {
      let note = count(remote.len(), "model");
      let head = |ui: &mut Ui| {
        ui.text(&note, fs.note);
      };
      ui.form_section_with(fs, "Available", head, |ui| {
        for &m in &remote {
          if let Some(a) = self.remote(ui, fs, m, false) {
            act = Some(a);
          }
        }
      });
    }

    if downloaded.is_empty() && remote.is_empty() && gone.is_none() {
      ui.text("No model matches.", fs.caption);
    }

    match act {
      Some(Act::Use(name)) => self.use_model(name),
      Some(Act::Ask(ask)) => self.ask = Some(ask),
      Some(Act::Card(m)) => self.shell.open_url(m.card),
      Some(Act::Download(m)) => {
        self.models.note = None;
        self.shell.download(m);
      }
      Some(Act::Cancel(name)) => self.shell.cancel_download(name),
      None => {}
    }
  }

  /// Uses downloaded model `name` from now on: Use here, the General page's pop-up.
  pub(super) fn use_model(&mut self, name: &'static str) {
    info!("settings: use model {name}");
    self.edit(move |s| s.model = Some(name.to_owned()));
  }

  /// [`Ask::Model`] said Delete: the shell deletes it off the main thread and hands the result
  /// to [`Prefs::model_deleted`]; the row says "Deleting…" meanwhile.
  pub(super) fn delete_model(&mut self, m: &'static ModelInfo) {
    self.models.note = None;
    self.models.deleting.push(m.name);
    self.shell.delete_model(m);
  }

  /// A downloaded model's row: Use (or "✓ In use" with the app's state), the bin and ⓘ;
  /// "Deleting…" while it goes.
  fn downloaded(&self, ui: &mut Ui, fs: &FormStyle, f: &Found, in_use: bool) -> Option<Act> {
    let t = ui.theme();
    let m = f.0;
    let deleting = self.models.deleting.contains(&m.name);
    let (state, error) = match in_use {
      true => self.in_use_state(f),
      false => ("", false),
    };
    let mut caption = format!("{} · {}", m.languages, size_text(m.size));
    if in_use {
      caption = format!("{caption} · {state}");
    }

    let warned = super::caption_in(fs, t.warning);
    let row_fs = if error { &warned } else { fs };
    let next = self.found.iter().find(|o| o.0.name != m.name).map(|o| o.0.full_name);
    let mut act = None;
    ui.id((ROW, m.name)).form_row(row_fs, model_row(m, Some(&caption)), |ui| {
      if in_use {
        ui.row(Style::new().gap(4.0).align(Align::Start, Align::Center), |ui| {
          ui.icon_stroke(Icon::CHECK, 14.0, t.accent, 2.2);
          let s = t.label().color(t.accent).font(t.font.weight(Weight::Medium)).nowrap();
          ui.text("In use", s);
        });
      }

      if deleting {
        ui.text("Deleting…", fs.caption.nowrap());
        return;
      }

      if !in_use && ui.id((USE, m.name)).button("Use").clicked {
        act = Some(Act::Use(m.name));
      }

      let r = ui.id((DELETE, m.name)).icon_button(Icon::TRASH);
      ui.tooltip(r.id, "Delete…");
      if r.clicked {
        act = Some(Act::Ask(Ask::Model { model: m, in_use, next }));
      }

      if card(ui, m) {
        act = Some(Act::Card(m));
      }
    });
    act
  }

  /// A model that is not on disk: Download (Retry after a failure), or its download's progress
  /// in place of its caption with ✕. `gone`: the chosen model, gone from the disk, which says so.
  fn remote(&self, ui: &mut Ui, fs: &FormStyle, m: &'static ModelInfo, gone: bool) -> Option<Act> {
    let t = ui.theme();
    let state = self.shell.download_state(m.name);
    let about = format!("{} · {}", m.languages, size_text(m.size));
    let caption = match &state {
      Some(Download::Failed(e)) => format!("Could not download: {e}"),
      _ if gone => format!("Not on disk · {about}"),
      _ => about,
    };
    // A download that runs shows its bar and how far it is where the caption is.
    let running = match &state {
      Some(Download::Running { done, total, rate }) => {
        let total = (*total).max(1);
        let k = *done as f32 / total as f32;
        let mut s = format!("{:.0}% · {}", k * 100.0, done_text(*done, total));
        if let Some(rate) = rate {
          s += &format!(" · {:.1} MB/s", rate / 1e6);
        }

        Some((Some(k), s))
      }
      Some(Download::Preparing) => Some((None, "Preparing for the Neural Engine…".to_owned())),
      Some(Download::Failed(_)) | None => None,
    };
    let row = model_row(m, running.is_none().then_some(caption.as_str()));

    let tinted = match &state {
      Some(Download::Failed(_)) => Some(super::caption_in(fs, t.danger)),
      None if gone => Some(super::caption_in(fs, t.warning)),
      _ => None,
    };
    let row_fs = tinted.as_ref().unwrap_or(fs);
    let id = Id::of((ROW, m.name));
    let below = |ui: &mut Ui| {
      if let Some((k, text)) = &running {
        progress_line(ui, fs, *k, text);
      }
    };
    let mut act = None;
    ui.with_id(id).form_row_with(row_fs, row, below, |ui| {
      match &state {
        None | Some(Download::Failed(_)) => {
          let label = if state.is_some() { "Retry" } else { "Download" };
          if ui.id((DOWNLOAD, m.name)).button(label).clicked {
            act = Some(Act::Download(m));
          }
        }
        Some(Download::Running { .. } | Download::Preparing) => {
          let r = ui.id((CANCEL, m.name)).icon_button(Icon::X);
          ui.tooltip(r.id, "Cancel the download");
          if r.clicked {
            act = Some(Act::Cancel(m.name));
          }
        }
      }

      if card(ui, m) {
        act = Some(Act::Card(m));
      }
    });
    // A caption cut short shows whole in its tooltip: a download's error.
    if matches!(state, Some(Download::Failed(_))) {
      ui.tooltip(id.key("caption"), &caption);
    }

    act
  }
}

/// The letter of a model's family on its tile.
fn tile(m: &ModelInfo) -> &'static str {
  m.family.get(..1).unwrap_or("·")
}

/// A model's row: its name on one line, `caption` on one line under it (else a download's
/// progress there).
fn model_row<'a>(m: &'a ModelInfo, caption: Option<&'a str>) -> FormRow<'a> {
  let row = FormRow::new(m.full_name).one_line().tile(Tile::Text(tile(m)));
  match caption {
    Some(c) => row.caption(c),
    None => row,
  }
}

/// A running download in place of its row's caption, as high as that line: the bar (moving
/// without a share), then how far it is (`42% · 114 of 272 MB · 3.2 MB/s`).
fn progress_line(ui: &mut Ui, fs: &FormStyle, k: Option<f32>, text: &str) {
  let line = Style::new().w(grow()).h(px(fs.caption.line_h)).gap(8.0);
  ui.row(line.align(Align::Start, Align::Center), |ui| {
    ui.col(Style::new().w(px(BAR)), |ui| ui.progress(k));
    ui.text(text, fs.caption.nowrap());
  });
}

/// A model's ⓘ, last in its row: whether it was clicked.
fn card(ui: &mut Ui, m: &ModelInfo) -> bool {
  let r = ui.id((INFO, m.name)).info_button();
  ui.tooltip(r.id, "The model’s card: its license, languages and benchmarks");
  r.clicked
}

/// `1 model`, `4 models`.
fn count(n: usize, what: &str) -> String {
  match n {
    1 => format!("1 {what}"),
    n => format!("{n} {what}s"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A model being downloaded or prepared is not ready, though its file is there.
  #[test]
  fn only_finished_models_are_ready() {
    let names = ["parakeet-v3", "parakeet-v2", "whisper-tiny"];
    let found: Vec<Found> = names.map(|n| (hub::lookup(n).unwrap(), n.into())).into();
    let state = |name: &str| match name {
      "parakeet-v3" => Some(Download::Preparing),
      "parakeet-v2" => Some(Download::Failed("x".into())),
      _ => None,
    };
    let ready: Vec<&str> = ready(found, state).iter().map(|f| f.0.name).collect();
    assert_eq!(ready, ["parakeet-v2", "whisper-tiny"]);
  }

  /// The names of wavo's models `query` finds.
  fn found(query: &str) -> Vec<&'static str> {
    let search = Search::new(query);
    hub::MODELS.iter().filter(|m| search.finds(m)).map(|m| m.name).collect()
  }

  #[test]
  fn the_search_finds_languages_families_makers_and_names() {
    let russian = found("ru");
    for name in ["gigaam-v3", "parakeet-v3", "whisper-turbo", "whisper-tiny"] {
      assert!(russian.contains(&name), "ru: {name} in {russian:?}");
    }

    for name in ["parakeet-v2", "whisper-tiny.en", "moonshine-base"] {
      assert!(!russian.contains(&name), "ru: {name} in {russian:?}");
    }

    // By code, the long code, the name, any case.
    for query in ["RU", "rus", "Russian"] {
      assert_eq!(found(query), russian, "{query}");
    }

    let english = found("English");
    assert!(english.contains(&"moonshine-tiny"));
    assert!(!english.iter().any(|n| n.starts_with("gigaam")), "{english:?}");
    // The family, its maker; every word.
    let parakeets = found("parakeet");
    assert!(parakeets.iter().all(|n| n.starts_with("parakeet")) && parakeets.len() > 1);
    assert_eq!(found("nvidia"), ["parakeet-v3", "parakeet-v2"], "its maker");
    let whispers = found("whisper");
    assert!(found("whisper ru").len() < whispers.len());
    assert_eq!(found("  "), hub::MODELS.iter().map(|m| m.name).collect::<Vec<_>>());
    assert!(found("klingon").is_empty());
  }

  #[test]
  fn a_choice_names_its_model_by_name_or_file() {
    let (m, hf) = (hub::lookup("whisper-tiny").unwrap(), Hub::at("/hf"));
    assert!(names(&hf, "whisper-tiny", m) && names(&hf, m.full_name, m));
    assert!(names(&hf, &format!("/hf/models--x/snapshots/c1/{}", m.file()), m));
    assert!(!names(&hf, &format!("/models/{}", m.file()), m), "a file of its own");
    assert!(!names(&hf, "whisper-base", m));
    let mut s = Settings { model: Some("whisper-tiny".into()), ..Settings::default() };
    forget(&mut s, &hf, hub::lookup("whisper-base").unwrap());
    assert!(s.model.is_some());
    forget(&mut s, &hf, m);
    assert_eq!(s.model, None);
    assert_eq!(size_text(1_700_000_000), "1.7 GB");
    assert_eq!(done_text(114_000_000, 272_000_000), "114 of 272 MB");
    assert_eq!(unload_title(Some(1)), "1 minute");
  }
}
