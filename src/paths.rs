// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Where the app keeps its data: settings and history. Models are in the Hugging Face cache.

use std::env;
use std::path::PathBuf;

/// The data home: `$WAVO_HOME`, else the platform's per-user data directory.
pub fn home() -> PathBuf {
  match env::var_os("WAVO_HOME") {
    Some(dir) if !dir.is_empty() => PathBuf::from(dir),
    _ if cfg!(target_os = "macos") => data_dir().join("me.vladkens.wavo"),
    _ if cfg!(windows) => data_dir().join("Wavo"),
    _ => data_dir().join("wavo"),
  }
}

/// `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on Windows, `$XDG_DATA_HOME`
/// (`~/.local/share`) elsewhere.
fn data_dir() -> PathBuf {
  if cfg!(windows) {
    env_dir("LOCALAPPDATA").unwrap_or_else(|| user_home().join("AppData").join("Local"))
  } else if cfg!(target_os = "macos") {
    user_home().join("Library").join("Application Support")
  } else {
    env_dir("XDG_DATA_HOME").unwrap_or_else(|| user_home().join(".local").join("share"))
  }
}

fn user_home() -> PathBuf {
  let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
  env_dir(home).unwrap_or_else(|| PathBuf::from("/"))
}

fn env_dir(name: &str) -> Option<PathBuf> {
  env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}
