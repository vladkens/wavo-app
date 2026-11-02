// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The command line: `uninstall` and `quit`; without a command the app runs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use nimbo::instance::{self, Claim};
use nimbo::{AppInfo, uninstall};
use wavo::hub::Hub;

use crate::INFO;

/// `cmd` to the running app.
pub fn send(cmd: &str) -> Result<()> {
  if instance::send_in(&crate::home(), cmd).is_err() {
    bail!("{} is not running", INFO.name);
  }

  Ok(())
}

// MARK: Uninstall

/// `uninstall`'s help: what it removes on this OS.
pub const UNINSTALL: &str = match () {
  _ if cfg!(wavo_dev) => {
    "Remove Wavo Dev's data, launch at login and permissions; the caches and the models it shares \
     with the installed Wavo stay"
  }
  _ if cfg!(target_os = "macos") => {
    "Remove Wavo and all it left: its data, caches, defaults, permissions, the models it \
     downloaded and the app"
  }
  _ if cfg!(windows) => {
    "Remove Wavo and all it left: its data, caches and the models it downloaded; the folder with \
     Wavo.exe is left to delete by hand, as Windows can't delete a running app"
  }
  _ => {
    "Remove Wavo and all it left: its data, caches, launcher entry, the models it downloaded and \
     the AppImage it runs from"
  }
};

/// `uninstall --yes`'s help. Windows: cmd and PowerShell do not wait for a GUI program such as this
/// one, so a question would come while the shell reads the next command; it is not asked there.
pub const YES: &str = match cfg!(windows) {
  true => "Do it; without, only say what it would do",
  false => "Do not ask first",
};

/// What `uninstall` does on this OS besides quitting the app and removing files.
const STEPS: &str = match () {
  _ if cfg!(target_os = "macos") => "turn launch at login off, reset its permissions and defaults",
  _ if cfg!(windows) => "turn launch at login off",
  _ => "turn launch at login off, remove its launcher entry",
};

/// Whether `uninstall` removes what the app shares and the app itself: wavo's cache, the models in
/// the Hugging Face cache, the app it runs from. Wavo does; Wavo Dev ([crate::DEV]) leaves them,
/// as the installed Wavo uses the caches and the models, and its app is a build in `target/`.
fn shares(info: &AppInfo) -> bool {
  info.id == crate::RELEASE.id
}

/// The folders `uninstall` removes, the outermost of them: the app's data (`home`), nimbo's config
/// where it is not the data (the Wayland paste's portal token), the app's cache and, when the app
/// [shares], wavo's (the models built for the Neural Engine, too).
fn folders(info: &AppInfo, home: PathBuf) -> Vec<PathBuf> {
  // On macOS config and data are one, which a `WAVO_HOME` stands in for and must not reach past.
  let config = nimbo::paths::config_dir(info.id);
  let config = (config != nimbo::paths::data_dir(info.id)).then_some(config);
  let wavo = shares(info).then(|| nimbo::paths::cache_dir("wavo"));
  let caches = [nimbo::paths::cache_dir(info.id)].into_iter().chain(wavo);
  uninstall::outermost([home].into_iter().chain(config).chain(caches))
}

/// Removes the app and all it left: quits it, undoes what nimbo set up in the OS
/// ([uninstall::system]: launch at login, the launcher entry, permissions and macOS defaults),
/// deletes its [folders] and, when it [shares], the models it downloaded into the Hugging Face
/// cache and the app it runs from ([uninstall::app_path]).
pub fn uninstall(dry_run: bool, yes: bool) -> Result<()> {
  let home = crate::home();
  let hub = Hub::new();
  let models = if shares(&INFO) { hub.downloaded() } else { Vec::new() };
  let mut paths: Vec<PathBuf> =
    folders(&INFO, home.clone()).into_iter().filter(|p| p.exists()).collect();
  paths.extend(uninstall::app_path().filter(|_| shares(&INFO)));
  // Windows can't delete the exe that runs this: its folder is left to the person.
  let left = std::env::current_exe().ok().filter(|_| cfg!(windows)).and_then(|exe| {
    let (dir, name) = (exe.parent()?.display(), exe.file_name()?.display());
    Some(format!("left for you to delete: {dir}, the folder with {name}"))
  });
  let list = || {
    paths.iter().for_each(|p| println!("{}", p.display()));
    models.iter().for_each(|(m, _)| println!("the model {} in {hub}", m.name));
  };
  // Windows: never asked ([YES]).
  if dry_run || (cfg!(windows) && !yes) {
    println!("would quit {}, {STEPS}, and remove:", INFO.name);
    list();
    left.iter().for_each(|l| println!("{l}"));
    if !dry_run {
      println!("nothing changed: again with --yes to uninstall");
    }

    return Ok(());
  }

  list();
  if !yes && !confirm()? {
    println!("nothing changed");
    return Ok(());
  }

  quit_and_wait(&home)?;

  for step in uninstall::system(&INFO) {
    println!("{step}");
  }

  for (m, _) in &models {
    match hub.remove(m.name) {
      Ok(_) => println!("removed the model {}", m.name),
      Err(e) => println!("the model {}: {e}", m.name),
    }
  }

  // The app last: this may run from it. One that can't go is told and left.
  let mut failed = 0;
  for path in &paths {
    match uninstall::remove(path) {
      Ok(()) => println!("removed {}", path.display()),
      Err(e) => {
        println!("could not remove {}: {e}", path.display());
        failed += 1;
      }
    }
  }

  if let Some(left) = &left {
    println!("{left}");
  }

  if failed > 0 {
    bail!("{failed} left, remove them by hand");
  }

  Ok(())
}

fn confirm() -> Result<bool> {
  print!("Uninstall {}? [y/N] ", INFO.name);
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

  let running = matches!(instance::claim_in(home)?, Claim::Running);
  if instance::quit_and_wait_in(home, Duration::from_secs(5)).is_err() {
    bail!("quit {} first", INFO.name);
  }

  if running {
    println!("{} quit", INFO.name);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{DEV, RELEASE};

  #[test]
  fn wavo_dev_leaves_what_wavo_uses() {
    let home = PathBuf::from("/wavo-home");
    let wavo = nimbo::paths::cache_dir("wavo");
    assert!(shares(&RELEASE));
    assert!(folders(&RELEASE, home.clone()).contains(&wavo));

    // Only its own: the data folder and what is under its id; no models, no app.
    assert!(!shares(&DEV));
    let dev = folders(&DEV, home.clone());
    assert!(!dev.contains(&wavo));
    assert!(dev.iter().all(|p| *p == home || p.to_string_lossy().contains(DEV.id)), "{dev:?}");
  }
}
