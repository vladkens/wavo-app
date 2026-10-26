// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Single instance: the app holds an `flock` on `wavo.lock` in its home while it runs (the kernel
//! drops it however the process ends) and takes one-line commands (`open`, `quit`) on a Unix socket
//! from a second launch and `wavo-app quit`.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub enum Claim {
  Server(Server),
  Running,
}

/// The lock and the socket, both let go on drop.
pub struct Server {
  /// Until [Server::serve] moves it to its thread: one descriptor for the socket.
  listener: Option<UnixListener>,
  path: PathBuf,
  _lock: File,
}

impl Drop for Server {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.path);
  }
}

impl Server {
  /// Calls `on` with each command, on a thread of its own; once.
  pub fn serve(&mut self, on: impl Fn(&str) + Send + 'static) -> io::Result<()> {
    let Some(listener) = self.listener.take() else { return Ok(()) };
    std::thread::Builder::new().name("ctl".into()).spawn(move || {
      for stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line).is_ok() {
          on(line.trim());
        }
      }
    })?;

    Ok(())
  }
}

// One socket per home (dev and inode), so a test's WAVO_HOME never meets the person's app; in /tmp,
// as `sun_path` holds 104 bytes on macOS.
fn socket_path(home: &Path) -> io::Result<PathBuf> {
  let meta = fs::metadata(home)?;
  let uid = unsafe { libc::getuid() };
  Ok(PathBuf::from(format!("/tmp/wavo-{uid}-{:x}-{:x}.sock", meta.dev(), meta.ino())))
}

/// Becomes the instance for `home` (which must exist), unless another one is.
pub fn claim(home: &Path) -> io::Result<Claim> {
  let lock = File::options().create(true).append(true).mode(0o600).open(home.join("wavo.lock"))?;
  match lock.try_lock() {
    Ok(()) => {}
    Err(fs::TryLockError::WouldBlock) => return Ok(Claim::Running),
    Err(fs::TryLockError::Error(e)) => return Err(e),
  }

  // The lock is ours: a socket file there is a dead instance's.
  let path = socket_path(home)?;
  let _ = fs::remove_file(&path);
  let listener = UnixListener::bind(&path)?;
  fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
  Ok(Claim::Server(Server { listener: Some(listener), path, _lock: lock }))
}

/// Sends `cmd` to the instance for `home`.
pub fn send(home: &Path, cmd: &str) -> io::Result<()> {
  let mut stream = UnixStream::connect(socket_path(home)?)?;
  stream.write_all(format!("{cmd}\n").as_bytes())
}

#[cfg(test)]
mod tests {
  use std::sync::mpsc;

  use super::*;
  use crate::testing::TempDir;

  fn serve(home: &Path) -> (Server, mpsc::Receiver<String>) {
    let Claim::Server(mut server) = claim(home).unwrap() else {
      panic!("{} is taken", home.display())
    };
    let (tx, rx) = mpsc::channel();
    server.serve(move |cmd| tx.send(cmd.to_owned()).unwrap()).unwrap();
    (server, rx)
  }

  #[test]
  fn a_second_instance_finds_the_first() {
    let dir = TempDir::new("ctl-second");
    let (server, rx) = serve(dir.path());
    assert!(matches!(claim(dir.path()).unwrap(), Claim::Running));
    send(dir.path(), "open").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "open");

    drop(server);
    assert!(send(dir.path(), "quit").is_err(), "gone with the server");
  }

  #[test]
  fn a_dead_instances_socket_is_replaced() {
    let dir = TempDir::new("ctl-stale");
    drop(UnixListener::bind(socket_path(dir.path()).unwrap()).unwrap());
    let (_server, rx) = serve(dir.path());
    send(dir.path(), "quit").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "quit");
  }
}
