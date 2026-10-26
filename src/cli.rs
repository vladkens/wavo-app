// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The command line: `uninstall` and `quit`; without a command the app runs.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

use anyhow::{Result, bail};
use nimbo::autostart;
use wavo::hub::Hub;

use crate::INFO;
use crate::ctl::{self, Claim};

/// `cmd` to the running app.
pub fn send(cmd: &str) -> Result<()> {
  if ctl::send(&crate::home(), cmd).is_err() {
    bail!("Wavo is not running");
  }

  Ok(())
}

// MARK: Uninstall

/// `uninstall`'s help: what it removes on this OS.
pub const UNINSTALL: &str = match () {
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

/// What `uninstall` does on this OS besides removing files.
const STEPS: &str = match () {
  _ if cfg!(target_os = "macos") => {
    "quit Wavo, turn launch at login off, reset its permissions and defaults"
  }
  _ if cfg!(windows) => "quit Wavo, turn launch at login off",
  _ => "quit Wavo, turn launch at login off, remove its launcher entry",
};

/// Removes Wavo and all it left: quits it, turns launch at login off, removes its launcher entry
/// (Linux), resets its permissions and macOS defaults, deletes its data, its caches (wavo's, the
/// models built for the Neural Engine, too), the models it downloaded into the Hugging Face cache,
/// and the app it runs from ([app]).
pub fn uninstall(dry_run: bool, yes: bool) -> Result<()> {
  let home = crate::home();
  let hub = Hub::new();
  let models = hub.downloaded();
  // nimbo's own files, such as the Wayland paste's portal token, where they are not the data dir:
  // on macOS both are one, which a `WAVO_HOME` stands in for and must not reach past.
  let config = nimbo::paths::config_dir(INFO.id);
  let config = (config != nimbo::paths::data_dir(INFO.id)).then_some(config);
  let caches = [nimbo::paths::cache_dir(INFO.id), nimbo::paths::cache_dir("wavo")];
  let dirs = [home.clone()].into_iter().chain(config).chain(caches).collect();
  let mut paths: Vec<PathBuf> = outermost(dirs).into_iter().filter(|p| p.exists()).collect();
  paths.extend(app());
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
    println!("would {STEPS}, and remove:");
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

  let off = autostart::state(&INFO).and_then(|s| match s.wanted() {
    true => autostart::set(&INFO, false),
    false => Ok(s),
  });
  match off {
    Ok(_) => println!("launch at login off"),
    Err(e) => println!("launch at login: {e}"),
  }

  if cfg!(target_os = "linux") {
    match nimbo::desktop::uninstall(&INFO) {
      Ok(()) => println!("the launcher entry removed"),
      Err(e) => println!("the launcher entry: {e}"),
    }
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

  // The app last: this may run from it. One that can't go is told and left.
  let mut failed = 0;
  for path in &paths {
    match remove(path) {
      Ok(()) => println!("removed {}", path.display()),
      Err(e) if e.kind() == io::ErrorKind::NotFound => {}
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

/// `dirs` but those inside another, which go with it (Windows: the cache dir in the data dir).
fn outermost(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
  let inside = |p: &PathBuf| dirs.iter().any(|d| d != p && p.starts_with(d));
  dirs.iter().filter(|p| !inside(p)).cloned().collect()
}

/// The app this runs from: macOS's `Wavo.app`, Linux's AppImage; none for a bare binary (a build)
/// or on Windows, where a running exe can't be deleted.
fn app() -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  if cfg!(target_os = "macos") {
    return bundle(&exe).map(Path::to_owned);
  }

  // The AppImage's runtime sets both; one that came from another AppImage (a terminal that is one)
  // is not where this runs.
  if cfg!(target_os = "linux") {
    let var = |k| std::env::var_os(k).map(PathBuf::from);
    let (image, mount) = (var("APPIMAGE")?, var("APPDIR")?);
    return (exe.starts_with(mount) && image.is_file()).then_some(image);
  }

  None
}

/// The bundle `exe` is the binary of, by its layout: `<name>.app/Contents/MacOS/<exe>`.
fn bundle(exe: &Path) -> Option<&Path> {
  let app = exe.parent().filter(|p| p.ends_with("Contents/MacOS"))?.parent()?.parent()?;
  app.extension().is_some_and(|e| e == "app").then_some(app)
}

/// Deletes `path`, a folder with all in it or a file; again, for up to 5 s, while an error may
/// pass. macOS can leave entries out of a folder's listing while it is emptied (a delete once
/// stopped "not empty" with 51 folders never visited). On Windows the app that just quit may hold
/// its files open after its lock is free (history.db with its -wal and -shm, its log).
fn remove(path: &Path) -> io::Result<()> {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    let done = match path.is_dir() {
      true => fs::remove_dir_all(path),
      false => fs::remove_file(path),
    };
    match done {
      Err(e) if passing(&e) && Instant::now() < deadline => {
        thread::sleep(Duration::from_millis(50))
      }
      done => return done,
    }
  }
}

/// A folder not empty yet; on Windows also a file another process has open (a sharing or lock
/// violation) or is deleting (access denied).
fn passing(e: &io::Error) -> bool {
  #[cfg(unix)]
  let codes = [libc::ENOTEMPTY];
  #[cfg(windows)]
  let codes = {
    use windows_sys::Win32::Foundation::{
      ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    };

    [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY]
      .map(|c| c as i32)
  };
  e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

fn confirm() -> Result<bool> {
  print!("Uninstall Wavo? [y/N] ");
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_dir_inside_another_goes_with_it() {
    let p = PathBuf::from;
    let dirs = vec![p("/data/wavo"), p("/data/wavo2"), p("/data/wavo/cache"), p("/cache/wavo")];
    assert_eq!(outermost(dirs), [p("/data/wavo"), p("/data/wavo2"), p("/cache/wavo")]);
  }

  #[test]
  fn a_bundle_by_its_layout() {
    let got = |exe: &str| bundle(Path::new(exe)).map(|p| p.to_str().unwrap().to_owned());
    let app = Some("/Applications/Wavo.app".to_owned());
    assert_eq!(got("/Applications/Wavo.app/Contents/MacOS/wavo-app"), app);
    assert_eq!(got("/src/foo.app/target/release/wavo-app"), None, "a build in a .app folder");
    assert_eq!(got("/Wavo.app/Contents/Resources/wavo-app"), None);
    assert_eq!(got("/src/Contents/MacOS/wavo-app"), None);
  }

  #[test]
  fn removes_a_folder_or_a_file() {
    let dir = crate::testing::TempDir::new("remove");
    let (folder, file) = (dir.path().join("home"), dir.path().join("Wavo.AppImage"));
    fs::create_dir_all(folder.join("recordings")).unwrap();
    fs::write(folder.join("recordings/a.wav"), "").unwrap();
    fs::write(&file, "").unwrap();
    remove(&folder).unwrap();
    remove(&file).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(remove(&file).unwrap_err().kind(), io::ErrorKind::NotFound, "at once");
  }
}
