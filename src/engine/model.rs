// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What the engine thread runs: a [`Loader`] that loads a model file into a [`SpeechModel`],
//! wavo's ([`Wavo`]) or a test's.

use std::ops::ControlFlow;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tracing::debug;

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
  /// Where it starts, in ms from the start of the audio.
  pub start_ms: u32,
}

/// A loaded model. It lives and is dropped on the engine thread that loaded it.
pub trait SpeechModel {
  /// The device it runs on, as wavo shows it: `Apple M2 (Metal, integrated)`.
  fn device(&self) -> String;

  /// The longest audio it was trained on, in ms, which no piece of a run is longer than; `None`
  /// for a model without a window (Parakeet on the GPU).
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
    Ok(Box::new(wavo::Model::load(path)?))
  }
}

impl SpeechModel for wavo::Model {
  fn device(&self) -> String {
    wavo::Model::device(self).to_string()
  }

  fn max_audio_ms(&self) -> Option<u32> {
    wavo::Model::max_audio_ms(self)
  }

  /// A model that cannot be told a language (Parakeet v3) is not told one: it detects it.
  fn transcribe(
    &self,
    pcm: &[f32],
    language: Option<&str>,
    on_piece: &mut dyn FnMut(Piece) -> ControlFlow<()>,
  ) -> Result<String> {
    let mut options = wavo::Options::default();
    options.language = language.filter(|_| self.takes_language()).map(str::to_owned);
    if let (Some(code), None) = (language, &options.language) {
      debug!("language {code} not sent: the model detects it");
    }

    let started = Instant::now();
    let transcript = self.transcribe_long(pcm, &options, |p| {
      on_piece(Piece { index: p.index, count: p.count, start_ms: p.start_ms })
    })?;

    debug!(samples = pcm.len(), ms = started.elapsed().as_millis() as u64, "run");
    Ok(transcript.text.trim().to_owned())
  }
}
