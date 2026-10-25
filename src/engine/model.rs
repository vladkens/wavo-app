// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What the engine thread runs: a [`Loader`] that loads a model file into a [`SpeechModel`],
//! wavo's ([`Wavo`]) or a test's.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

/// Loads model files; called on an engine thread, once per thread.
pub trait Loader: Send + Sync {
  fn load(&self, path: &Path) -> Result<Box<dyn SpeechModel>>;
}

/// A piece of a run that is done, as [`SpeechModel::transcribe`] passes it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
  /// Its position among the pieces, from 0.
  pub index: usize,
  /// How many pieces the audio is cut into.
  pub count: usize,
}

/// A loaded model. It lives and is dropped on the engine thread that loaded it.
pub trait SpeechModel {
  /// The device it runs on, as wavo shows it: `Apple M2 (Metal, integrated)`.
  fn device(&self) -> String;

  /// The longest audio it was trained on, in ms, which bounds each piece's deadline; `None` for a
  /// model without one (Parakeet on the GPU, which wavo still cuts at 60 s): the deadline is for
  /// the rest of the run.
  fn max_audio_ms(&self) -> Option<u32>;

  /// The text of 16 kHz mono audio of any length, cut at pauses into pieces the model takes, one
  /// run each; `language` `None` leaves it to the model. `on_piece` gets each piece once it is
  /// done: a break stops the run there, as an error.
  fn transcribe(
    &self,
    pcm: &[f32],
    language: Option<&str>,
    on_piece: &mut dyn FnMut(Piece) -> ControlFlow<()>,
  ) -> Result<String>;
}

/// wavo's models.
pub struct Wavo;

impl Loader for Wavo {
  fn load(&self, path: &Path) -> Result<Box<dyn SpeechModel>> {
    prepare(path);
    Ok(Box::new(wavo::Model::load(path)?))
  }
}

/// A Parakeet or GigaAM file not yet prepared for the Neural Engine in this program (one wavo or
/// another app downloaded) is prepared on a thread of its own, 15–40 s once; until then it loads
/// on the GPU, and the next load after that runs on the Neural Engine.
fn prepare(path: &Path) {
  static PREPARING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
  if wavo::Model::prepared(path) != Some(false) {
    return;
  }

  let mut preparing = PREPARING.lock().unwrap();
  if preparing.iter().any(|p| p == path) {
    return;
  }

  preparing.push(path.to_owned());
  let path = path.to_owned();
  thread::spawn(move || {
    let started = Instant::now();
    match wavo::Model::prepare(&path, |_| ControlFlow::Continue(())) {
      Ok(_) => info!("{}: prepared for the Neural Engine in {:?}", path.display(), started.elapsed()),
      Err(e) => warn!("{}: not prepared for the Neural Engine: {e}", path.display()),
    }

    PREPARING.lock().unwrap().retain(|p| *p != path);
  });
}

impl SpeechModel for wavo::Model {
  fn device(&self) -> String {
    wavo::Model::device(self).to_string()
  }

  fn max_audio_ms(&self) -> Option<u32> {
    wavo::Model::max_audio_ms(self)
  }

  /// A language the model cannot be told (any for Parakeet v3, another than its own for a model of
  /// one) is not sent: the model goes by the speech, and a model meant for one language that hears
  /// another makes nothing of it.
  fn transcribe(
    &self,
    pcm: &[f32],
    language: Option<&str>,
    on_piece: &mut dyn FnMut(Piece) -> ControlFlow<()>,
  ) -> Result<String> {
    let mut options = wavo::Options::default();
    let listed = language.and_then(|c| self.languages().iter().find(|l| l.eq_ignore_ascii_case(c)));
    match (language, listed) {
      (_, Some(code)) if self.takes_language() => options.language = Some(code.clone()),
      (Some(code), _) => warn!("language {code} not sent: the model cannot be told it"),
      (None, _) => {}
    }

    let started = Instant::now();
    let transcript = self
      .transcribe_long(pcm, &options, |p| on_piece(Piece { index: p.index, count: p.count }))?;

    debug!(samples = pcm.len(), ms = started.elapsed().as_millis() as u64, "run");
    Ok(transcript.text.trim().to_owned())
  }
}
