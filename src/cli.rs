// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The command line: `reset` and `quit`; without a command the app runs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, thread};

use anyhow::{Result, bail};
use nimbo::autostart;

use crate::INFO;
use crate::ctl::{self, Claim};

pub fn quit() -> Result<()> {
  if ctl::send(&crate::home(), "quit").is_err() {
    bail!("Wavo is not running");
  }

  Ok(())
}

// MARK: Reset

/// Back to a clean install: quits Wavo, turns launch at login off, resets its permissions and
/// deletes its data, its caches (wavo's, the models built for the Neural Engine, too) and its
/// macOS defaults. The Hugging Face cache stays.
pub fn reset(dry_run: bool, yes: bool) -> Result<()> {
  let home = crate::home();
  let paths = [home.clone(), nimbo::paths::cache_dir(INFO.id), nimbo::paths::cache_dir("wavo")];
  let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
  if dry_run {
    println!("would quit Wavo, turn launch at login off and reset its permissions and defaults");
    paths.iter().for_each(|p| println!("would remove {}", p.display()));
    return Ok(());
  }

  paths.iter().for_each(|p| println!("{}", p.display()));
  if !yes && !confirm()? {
    println!("nothing changed");
    return Ok(());
  }

  quit_and_wait(&home)?;

  let off = autostart::state(&INFO).and_then(|s| match s.wanted() {
    true => autostart::set(&INFO, false),
    false => Ok(s),
  });
  match off {
    Ok(_) => println!("launch at login off"),
    Err(e) => println!("launch at login: {e}"),
  }

  if cfg!(target_os = "macos") {
    let tccutil = std::process::Command::new("tccutil").args(["reset", "All", INFO.id]).status();
    let done = tccutil.is_ok_and(|s| s.success());
    println!("permissions reset: {}", if done { "done" } else { "failed" });

    // Through cfprefsd, which caches them: the open and save panels' sizes and places.
    let defaults = std::process::Command::new("defaults").args(["delete", INFO.id]).output();
    let done = defaults.is_ok_and(|o| o.status.success());
    println!("defaults: {}", if done { "removed" } else { "none" });
  }

  for path in &paths {
    match path.is_dir() {
      true => remove_dir(path)?,
      false => fs::remove_file(path)?,
    }

    println!("removed {}", path.display());
  }

  Ok(())
}

/// Deletes `dir` and all in it. macOS can leave entries out of a folder's listing while it is
/// emptied (`make reset` once stopped "not empty" with 51 folders never visited): again, until
/// it is gone.
fn remove_dir(dir: &Path) -> io::Result<()> {
  for _ in 0..10 {
    match fs::remove_dir_all(dir) {
      Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => continue,
      done => return done,
    }
  }

  fs::remove_dir_all(dir)
}

fn confirm() -> Result<bool> {
  print!(
    "Quit Wavo, turn launch at login off, reset its permissions and defaults, delete the above? [y/N] "
  );
  io::stdout().flush()?;
  let mut answer = String::new();
  io::stdin().read_line(&mut answer)?;
  Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Asks the running Wavo to quit and waits up to 5 s for its lock.
fn quit_and_wait(home: &Path) -> Result<()> {
  if !home.exists() {
    return Ok(());
  }

  let asked = ctl::send(home, "quit").is_ok();
  for _ in 0..100 {
    if let Claim::Server(_) = ctl::claim(home)? {
      if asked {
        println!("Wavo quit");
      }

      return Ok(());
    }

    thread::sleep(Duration::from_millis(50));
  }

  bail!("quit Wavo first")
}
