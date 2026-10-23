// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Speech models: wavo's models found in the Hugging Face cache, and their [`download`]s through
//! wavo's hub.

pub mod download;

use std::fs;
use std::path::PathBuf;

use wavo::hub::{self, Hub, ModelInfo};

/// One of wavo's models, downloaded.
#[derive(Debug)]
pub struct ModelFile {
  pub info: &'static ModelInfo,
  pub path: PathBuf,
  pub size: u64,
}

/// wavo's models in the cache `hub` ([`Hub::find`], which never uses the network), in the order
/// of [`hub::MODELS`].
pub fn discover(hub: &Hub) -> Vec<ModelFile> {
  let found = hub::MODELS.iter().filter_map(|info| {
    let path = hub.find(info.name)?;
    // `metadata` follows symlinks: HF snapshots link into `blobs/`.
    let meta = fs::metadata(&path).ok().filter(|m| m.is_file())?;
    Some(ModelFile { info, path, size: meta.len() })
  });
  found.collect()
}

// The cache is made with symbolic links.
#[cfg(all(test, unix))]
mod tests {
  use std::path::Path;

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
    let tiny = cache(&hub, "whisper-tiny", b"tiny");
    let gigaam = cache(&hub, "gigaam-v3", b"gigaam");
    // Another file of a known repo, and a repo wavo does not know.
    write(&gigaam.with_file_name("gigaam-v3-e2e-rnnt-Q4_0.gguf"), b"q4");
    write(&hub.root().join("models--x--y-gguf/snapshots/c1/y.gguf"), b"y");

    let found = discover(&hub);
    let names: Vec<_> = found.iter().map(|m| m.info.name).collect();
    assert_eq!(names, ["gigaam-v3", "whisper-tiny"]);
    assert_eq!((&found[1].path, found[1].size), (&tiny, 4));
    assert!(discover(&Hub::at(dir.path().join("missing"))).is_empty());
  }
}
