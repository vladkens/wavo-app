// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What the engine thread runs: a [`Loader`] that loads a model file into a [`SpeechModel`],
//! wavo's ([`Wavo`]) or a test's; and which model file that is ([`find`]).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info};
use wavo::hub::{self, Hub};

/// Loads model files; called on an engine thread, once per thread.
pub trait Loader: Send + Sync {
  fn load(&self, path: &Path) -> Result<Box<dyn SpeechModel>>;
}

/// A loaded model. It lives and is dropped on the engine thread that loaded it.
pub trait SpeechModel {
  /// The device it runs on, as wavo shows it: `Apple M2 (Metal, integrated)`.
  fn device(&self) -> String;

  /// The longest audio it was trained on, in ms: the engine cuts longer input into pieces of up
  /// to this. `None` takes any length in one run.
  fn max_audio_ms(&self) -> Option<u32>;

  /// The text of one piece of 16 kHz mono audio; `language` `None` leaves it to the model.
  fn transcribe(&self, pcm: &[f32], language: Option<&str>) -> Result<String>;
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

  fn transcribe(&self, pcm: &[f32], language: Option<&str>) -> Result<String> {
    let mut options = wavo::Options::default();
    options.language = language.map(str::to_owned);
    let started = Instant::now();
    let transcript = self.transcribe_with(pcm, &options)?;
    debug!(samples = pcm.len(), ms = started.elapsed().as_millis() as u64, "run");
    Ok(transcript.text.trim().to_owned())
  }
}

/// The model file to load: `name` as a path to a file, else as one of wavo's models (short or full
/// name) in `hub`; without a name the first of wavo's models found there.
pub fn find(name: Option<&str>, hub: &Hub) -> Result<PathBuf> {
  let Some(name) = name else {
    let first = hub::MODELS.iter().find_map(|m| Some((m.name, hub.find(m.name)?)));
    let (name, path) = first.with_context(|| {
      format!("no speech models in {}: download one with `wavo pull <name>`", hub.root().display())
    })?;
    info!("using {name}, the first model found");
    return Ok(path);
  };

  if Path::new(name).is_file() {
    return Ok(name.into());
  }

  let model = hub::lookup(name)?;
  hub.find(name).with_context(|| format!("{} is not downloaded: `wavo pull {0}`", model.name))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::testing::{TempDir, download};

  #[test]
  fn finds_a_model_by_name_or_path_else_the_first_downloaded() {
    let dir = TempDir::new("model-find");
    let hub = Hub::at(dir.path());
    let err = find(None, &hub).unwrap_err().to_string();
    assert!(err.starts_with("no speech models in "), "{err}");

    let gigaam = download(&hub, "gigaam-v3");
    let whisper = download(&hub, "whisper-tiny");
    assert_eq!(find(None, &hub).unwrap(), gigaam, "wavo's order");
    assert_eq!(find(Some("whisper-tiny"), &hub).unwrap(), whisper);
    assert_eq!(find(Some("gigaam-v3-e2e-rnnt"), &hub).unwrap(), gigaam, "the full name");
    assert_eq!(find(Some(gigaam.to_str().unwrap()), &hub).unwrap(), gigaam);

    let err = find(Some("whisper-base"), &hub).unwrap_err().to_string();
    assert_eq!(err, "whisper-base is not downloaded: `wavo pull whisper-base`");
    let err = find(Some("gigaam3"), &hub).unwrap_err().to_string();
    assert!(err.contains("did you mean gigaam-v3?"), "{err}");
  }
}
