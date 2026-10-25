// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! `wavo-app reset`: back to a clean install. Quits the running Wavo, turns launch at login off,
//! resets its permissions (macOS) and deletes its data home and caches; with `--app` the app it
//! runs from (macOS). The Hugging Face cache and other apps' data stay.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

use anyhow::{Result, bail};
use nimbo::autostart;

use crate::INFO;
use crate::ctl::{self, ClientError, Reply, Request};

/// How long the running Wavo has to stop answering after `quit`.
const QUIT_WAIT: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 5_000 });

pub fn run(app: bool, dry_run: bool, yes: bool) -> Result<()> {
  let home = crate::home();
  let paths: Vec<PathBuf> = paths(&home, app)?.into_iter().filter(|p| p.exists()).collect();
  if dry_run {
    println!("would quit Wavo, turn launch at login off and reset its permissions");
    return remove(&paths, true);
  }

  for path in &paths {
    println!("{}", path.display());
  }

  if !yes && !confirm()? {
    println!("nothing changed");
    return Ok(());
  }

  quit(|request| ask(&home, request))?;

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
  } else {
    println!("no per-app permissions to reset on this OS");
  }

  remove(&paths, false)
}

/// What is deleted: the data home, the cache, on macOS the Metal shader cache and with `app` the
/// app this binary runs from.
fn paths(home: &Path, app: bool) -> Result<Vec<PathBuf>> {
  let mut paths = vec![home.to_owned(), nimbo::paths::cache_dir(INFO.id)];
  #[cfg(target_os = "macos")]
  paths.extend(darwin_cache_dir().map(|dir| dir.join(INFO.id)));
  if app && cfg!(target_os = "macos") {
    let exe = std::env::current_exe()?;
    match exe.ancestors().nth(3).filter(|a| a.extension().is_some_and(|e| e == "app")) {
      Some(bundle) => paths.push(bundle.to_owned()),
      None => println!("not run from an app bundle: no app to delete"),
    }
  }

  Ok(paths)
}

/// The per-user cache folder the system gives (`getconf DARWIN_USER_CACHE_DIR`).
#[cfg(target_os = "macos")]
fn darwin_cache_dir() -> Option<PathBuf> {
  use std::os::unix::ffi::OsStrExt;

  let mut buf = [0u8; libc::PATH_MAX as usize];
  // SAFETY: confstr writes at most `buf.len()` bytes, the NUL included, into the buffer.
  let n =
    unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buf.as_mut_ptr().cast(), buf.len()) };
  if n == 0 || n > buf.len() {
    return None;
  }

  Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n - 1])))
}

fn confirm() -> Result<bool> {
  print!("Quit Wavo, turn launch at login off, reset its permissions and delete the above? [y/N] ");
  io::stdout().flush()?;
  let mut answer = String::new();
  io::stdin().read_line(&mut answer)?;
  Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `request` to the Wavo of `home`, as `wavo-app ctl` sends it.
fn ask(home: &Path, request: &Request) -> Result<Reply, ClientError> {
  let socket = ctl::socket_path(home).map_err(|_| ClientError::NotRunning(home.to_owned()))?;
  ctl::request(&socket, request)
}

/// `ctl quit`, then waits until nothing answers; none answering is fine.
fn quit(ask: impl Fn(&Request) -> Result<Reply, ClientError>) -> Result<()> {
  if let Err(ClientError::NotRunning(_)) = ask(&Request::Quit) {
    return Ok(());
  }

  let start = Instant::now();
  while start.elapsed() < QUIT_WAIT {
    thread::sleep(Duration::from_millis(50));
    if let Err(ClientError::NotRunning(_)) = ask(&Request::Status) {
      println!("Wavo quit");
      return Ok(());
    }
  }

  bail!("quit Wavo first")
}

/// Deletes each of `paths` (with `dry_run` only says so), printing it.
fn remove(paths: &[PathBuf], dry_run: bool) -> Result<()> {
  for path in paths {
    if dry_run {
      println!("would remove {}", path.display());
      continue;
    }

    match path.is_dir() {
      true => fs::remove_dir_all(path)?,
      false => fs::remove_file(path)?,
    }

    println!("removed {}", path.display());
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use super::*;
  use crate::testing::TempDir;

  fn not_running() -> ClientError {
    ClientError::NotRunning(PathBuf::new())
  }

  #[test]
  fn quit_goes_on_once_nothing_answers() {
    assert!(quit(|_| Err(not_running())).is_ok(), "none running");

    let asked = Cell::new(0);
    let quits = quit(|_| {
      asked.set(asked.get() + 1);
      if asked.get() < 3 { Ok(Reply::ok("quitting")) } else { Err(not_running()) }
    });
    assert!(quits.is_ok());

    let stays = quit(|_| Ok(Reply::ok("idle")));
    assert_eq!(stays.unwrap_err().to_string(), "quit Wavo first");
  }

  #[test]
  fn remove_deletes_only_the_paths_and_not_in_a_dry_run() {
    let dir = TempDir::new("reset");
    let (home, cache, other) =
      (dir.path().join("home"), dir.path().join("cache"), dir.path().join("hf"));
    for d in [&home, &cache, &other] {
      fs::create_dir_all(d.join("sub")).unwrap();
      fs::write(d.join("sub/file"), "x").unwrap();
    }

    let paths = [home.clone(), cache.clone()];
    remove(&paths, true).unwrap();
    assert!(home.exists() && cache.exists(), "a dry run deletes nothing");
    remove(&paths, false).unwrap();
    assert!(!home.exists() && !cache.exists() && other.join("sub/file").exists());
  }

  /// The data home is `WAVO_HOME`'s, and nothing of the Hugging Face cache or other apps is listed.
  #[test]
  fn paths_are_wavos_only() {
    let home = PathBuf::from("/tmp/wavo-reset-home");
    let paths = paths(&home, false).unwrap();
    assert_eq!(paths[0], home);
    for path in &paths {
      let path = path.to_string_lossy();
      let others = ["huggingface", "Handy", "lalo", "wavo-gui"];
      assert!(!others.iter().any(|o| path.contains(o)), "{path}");
    }
  }
}
