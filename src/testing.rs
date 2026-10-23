// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Test helpers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{env, fs};

/// A fresh directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
  pub fn new(name: &str) -> Self {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("wavo-test-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    Self(path)
  }

  pub fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}
