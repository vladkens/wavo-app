// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Speech models: wavo's models found in the Hugging Face cache.

mod gguf;

use std::fs;
use std::path::PathBuf;

use wavo::hub::{self, Hub};

#[derive(Debug)]
pub struct ModelFile {
  /// File name without extension, e.g. `gigaam-v3-e2e-rnnt-Q8_0`.
  pub name: String,
  pub path: PathBuf,
  /// `general.architecture`, e.g. `whisper`.
  pub arch: String,
  pub size: u64,
}

/// Every speech model: wavo's models in the cache `hub` ([`Hub::find`], which never uses the
/// network), in the order of [`hub::MODELS`]. Files that are not speech models (an LLM GGUF, an
/// architecture wavo does not run) are skipped.
pub fn discover(hub: &Hub) -> Vec<ModelFile> {
  (hub::MODELS.iter()).filter_map(|m| probe(hub.find(m.name)?)).collect()
}

fn probe(path: PathBuf) -> Option<ModelFile> {
  // `metadata` follows symlinks: HF snapshots link into `blobs/`.
  let meta = fs::metadata(&path).ok().filter(|m| m.is_file())?;
  let arch = match gguf::speech_arch(&path) {
    Ok(arch) => arch?,
    Err(e) => {
      tracing::debug!("skip {}: {e}", path.display());
      return None;
    }
  };

  let name = path.file_stem()?.to_string_lossy().into_owned();
  Some(ModelFile { name, path, arch, size: meta.len() })
}

// The cache is made with symbolic links.
#[cfg(all(test, unix))]
mod tests {
  use std::path::Path;

  use super::gguf::speech_header;
  use super::*;
  use crate::testing::TempDir;

  fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
  }

  /// wavo's model `name` in `hub` as `hf` lays it out: a blob holding `bytes`, the `main` ref and
  /// the snapshot's file, a symbolic link to the blob. The file's path.
  fn cache(hub: &Hub, name: &str, bytes: &[u8]) -> PathBuf {
    let m = hub::lookup(name).unwrap();
    let repo = hub.root().join(format!("models--{}", m.repo().replace('/', "--")));
    write(&repo.join("blobs/sha"), bytes);
    write(&repo.join("refs/main"), b"c1");
    let file = repo.join("snapshots/c1").join(m.file());
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../../blobs/sha", &file).unwrap();
    file
  }

  #[test]
  fn discovers_wavos_models_in_the_cache() {
    let dir = TempDir::new("models-discover");
    let hub = Hub::at(dir.path().join("hub"));
    let tiny = cache(&hub, "whisper-tiny", &speech_header("whisper"));
    let gigaam = cache(&hub, "gigaam-v3", &speech_header("gigaam"));
    cache(&hub, "moonshine-tiny", b"not a model");
    cache(&hub, "whisper-base", &speech_header("llama"));
    // Another file of a known repo, and a repo wavo does not know.
    write(&gigaam.with_file_name("gigaam-v3-e2e-rnnt-Q4_0.gguf"), &speech_header("gigaam"));
    write(&hub.root().join("models--x--y-gguf/snapshots/c1/y.gguf"), &speech_header("whisper"));

    let found = discover(&hub);
    let summary: Vec<_> = found.iter().map(|m| (m.name.as_str(), m.arch.as_str())).collect();
    assert_eq!(summary, [("gigaam-v3-e2e-rnnt-Q8_0", "gigaam"), ("whisper-tiny-Q8_0", "whisper")]);
    assert_eq!((&found[1].path, found[1].size), (&tiny, speech_header("whisper").len() as u64));
    assert!(discover(&Hub::at(dir.path().join("missing"))).is_empty());
  }
}
