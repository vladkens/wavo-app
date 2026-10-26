// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The command line: `uninstall` and `quit`; without a command the app runs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, thread};

use anyhow::{Result, bail};
use nimbo::autostart;
use wavo::hub::Hub;

use crate::INFO;
use crate::ctl::{self, Claim};

pub fn quit() -> Result<()> {
  if ctl::send(&crate::home(), "quit").is_err() {
    bail!("Wavo is not running");
  }

  Ok(())
}

// MARK: Uninstall

/// Removes Wavo and all it left: quits it, turns launch at login off, resets its permissions and
/// macOS defaults, deletes its data, its caches (wavo's, the models built for the Neural Engine,
/// too), the models it downloaded into the Hugging Face cache, and the app it runs from.
pub fn uninstall(dry_run: bool, yes: bool) -> Result<()> {
  let home = crate::home();
  let hub = Hub::new();
  let models = hub.downloaded();
  let dirs = [home.clone(), nimbo::paths::cache_dir(INFO.id), nimbo::paths::cache_dir("wavo")];
  let mut paths: Vec<PathBuf> = dirs.into_iter().filter(|p| p.exists()).collect();
  paths.extend(app_bundle());
  let list = || {
    paths.iter().for_each(|p| println!("{}", p.display()));
    models.iter().for_each(|(m, _)| println!("the model {} in {hub}", m.name));
  };
  if dry_run {
    println!("would quit Wavo, turn launch at login off, reset its permissions and defaults");
    list();
    return Ok(());
  }

  list();
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
    // cfprefsd leaves the file behind, empty.
    if let Some(home) = std::env::var_os("HOME") {
      let plist = PathBuf::from(home).join(format!("Library/Preferences/{}.plist", INFO.id));
      let _ = fs::remove_file(plist);
    }
  }

  for (m, _) in &models {
    match hub.remove(m.name) {
      Ok(_) => println!("removed the model {}", m.name),
      Err(e) => println!("the model {}: {e}", m.name),
    }
  }

  // The app last: this may run from it.
  for path in &paths {
    match path.is_dir() {
      true => remove_dir(path)?,
      false => fs::remove_file(path)?,
    }

    println!("removed {}", path.display());
  }

  Ok(())
}

/// The app this runs from, `/Applications/Wavo.app`; none for a bare binary (a build).
fn app_bundle() -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  let app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"))?;
  Some(app.to_owned())
}

/// Deletes `dir` and all in it. macOS can leave entries out of a folder's listing while it is
/// emptied (a delete once stopped "not empty" with 51 folders never visited): again, until it is
/// gone.
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
    "Quit Wavo, turn launch at login off, reset its permissions and defaults, remove the above? [y/N] "
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
