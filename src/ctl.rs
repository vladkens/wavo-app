// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Single instance: the app holds a lock on `wavo.lock` in its home while it runs (the OS drops it
//! however the process ends) and takes one-line commands (`open`, `quit`) from a second launch and
//! `wavo-app quit`: on a Unix socket, on Windows a named pipe.

use std::path::Path;
use std::time::Duration;
use std::{fs, io, thread};

#[cfg(unix)]
pub use unix::{Server, send};
#[cfg(windows)]
pub use windows::{Server, send};

pub enum Claim {
  Server(Server),
  Running,
}

const LOCK: &str = "wavo.lock";
// How long a client may take to send its command.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Becomes the instance for `home` (which must exist), unless another one is.
pub fn claim(home: &Path) -> io::Result<Claim> {
  #[cfg(unix)]
  let lock = unix::lock_file(home)?;
  #[cfg(windows)]
  let lock = windows::lock_file(home)?;
  match lock.try_lock() {
    Ok(()) => {}
    Err(fs::TryLockError::WouldBlock) => return Ok(Claim::Running),
    Err(fs::TryLockError::Error(e)) => return Err(e),
  }

  Server::bind(home, lock).map(Claim::Server)
}

fn spawn(f: impl FnOnce() + Send + 'static) -> io::Result<thread::JoinHandle<()>> {
  thread::Builder::new().name("ctl".into()).spawn(f)
}

// MARK: Unix

#[cfg(unix)]
mod unix {
  use std::fs::{self, File};
  use std::io::{self, BufRead, BufReader, Write};
  use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
  use std::os::unix::net::{UnixListener, UnixStream};
  use std::path::{Path, PathBuf};

  use super::{LOCK, READ_TIMEOUT};

  pub fn lock_file(home: &Path) -> io::Result<File> {
    File::options().create(true).append(true).mode(0o600).open(home.join(LOCK))
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
    /// The lock is ours: a socket file there is a dead instance's.
    pub(super) fn bind(home: &Path, lock: File) -> io::Result<Self> {
      let path = socket_path(home)?;
      let _ = fs::remove_file(&path);
      let listener = UnixListener::bind(&path)?;
      fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
      Ok(Self { listener: Some(listener), path, _lock: lock })
    }

    /// Calls `on` with each command, on a thread of its own; once.
    pub fn serve(&mut self, on: impl Fn(&str) + Send + 'static) -> io::Result<()> {
      let Some(listener) = self.listener.take() else { return Ok(()) };
      super::spawn(move || {
        for stream in listener.incoming().flatten() {
          let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
          let mut line = String::new();
          if BufReader::new(stream).read_line(&mut line).is_ok() {
            on(line.trim());
          }
        }
      })?;

      Ok(())
    }
  }

  // One socket per home (dev and inode), so a test's WAVO_HOME never meets the person's app; in
  // /tmp, as `sun_path` holds 104 bytes on macOS.
  pub(super) fn socket_path(home: &Path) -> io::Result<PathBuf> {
    let meta = fs::metadata(home)?;
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from(format!("/tmp/wavo-{uid}-{:x}-{:x}.sock", meta.dev(), meta.ino())))
  }

  /// Sends `cmd` to the instance for `home`.
  pub fn send(home: &Path, cmd: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path(home)?)?;
    stream.write_all(format!("{cmd}\n").as_bytes())
  }
}

// MARK: Windows

/// A named pipe, `\\.\pipe\wavo-<hash of the home>`, of one instance that serves one client at a
/// time, local clients only, with the default security (this user, administrators, the system).
/// A client waits until the instance hangs up: it has read the command then.
#[cfg(windows)]
mod windows {
  use std::fs::File;
  use std::io::{self, Read, Write};
  use std::os::windows::ffi::OsStrExt;
  use std::os::windows::fs::OpenOptionsExt;
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use std::path::{Path, PathBuf};
  use std::ptr::{null, null_mut};
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::thread::{self, JoinHandle};
  use std::time::{Duration, Instant};

  use tracing::warn;
  use windows_sys::Win32::Foundation::{
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
  };
  use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
  };
  use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe,
  };
  use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

  use super::{LOCK, READ_TIMEOUT};

  // How long a client waits while the pipe serves another, and the server for its thread to stop.
  const BUSY_WAIT: Duration = Duration::from_secs(2);
  const POLL: Duration = Duration::from_millis(5);
  // The longest command read.
  const MAX_LINE: usize = 256;

  pub fn lock_file(home: &Path) -> io::Result<File> {
    // Write access: LockFileEx refuses an append-only handle.
    File::options().create(true).write(true).truncate(false).open(home.join(LOCK))
  }

  /// The lock and the pipe; on drop the thread stops before the pipe goes, then the lock.
  pub struct Server {
    pipe: Arc<OwnedHandle>,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _lock: File,
  }

  impl Drop for Server {
    /// A client wakes the thread from `ConnectNamedPipe`; one that does not stop keeps the pipe.
    fn drop(&mut self) {
      self.stop.store(true, Ordering::Release);
      let Some(thread) = self.thread.take() else { return };
      let deadline = Instant::now() + BUSY_WAIT;
      while !thread.is_finished() && Instant::now() < deadline {
        let _ = client(&self.path);
        thread::sleep(Duration::from_millis(10));
      }

      match thread.is_finished() {
        true => _ = thread.join(),
        false => warn!("ctl: the pipe's thread did not stop"),
      }
    }
  }

  impl Server {
    /// The first instance of the pipe: a pipe of that name made by another process fails it.
    pub(super) fn bind(home: &Path, lock: File) -> io::Result<Self> {
      let path = pipe_path(home);
      let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
      let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
      let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE;
      // SAFETY: a NUL-terminated name, the default security.
      let pipe = unsafe { CreateNamedPipeW(name.as_ptr(), open, mode, 1, 0, 0, 0, null()) };
      if pipe == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
      }

      // SAFETY: the handle just made, owned from here.
      let pipe = Arc::new(unsafe { OwnedHandle::from_raw_handle(pipe) });
      let stop = Arc::new(AtomicBool::new(false));
      Ok(Self { pipe, path, stop, thread: None, _lock: lock })
    }

    /// Calls `on` with each command, on a thread of its own; once.
    pub fn serve(&mut self, on: impl Fn(&str) + Send + 'static) -> io::Result<()> {
      if self.thread.is_some() {
        return Ok(());
      }

      let (pipe, stop) = (self.pipe.clone(), self.stop.clone());
      let thread = super::spawn(move || {
        let pipe = pipe.as_raw_handle();
        while !stop.load(Ordering::Acquire) {
          // SAFETY: our pipe; blocks until a client connects.
          let connected = unsafe { ConnectNamedPipe(pipe, null_mut()) } != 0
            || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
          if connected
            && !stop.load(Ordering::Acquire)
            && let Some(line) = read_line(pipe)
          {
            on(line.trim());
          }

          // SAFETY: our pipe; the client's reads end.
          unsafe { DisconnectNamedPipe(pipe) };
        }
      })?;

      self.thread = Some(thread);
      Ok(())
    }
  }

  /// The client's line, read as it comes until a newline or its end, within READ_TIMEOUT: polled,
  /// as a blocking read would wait for a client that sends nothing.
  fn read_line(pipe: HANDLE) -> Option<String> {
    let (deadline, mut line) = (Instant::now() + READ_TIMEOUT, Vec::new());
    while !line.contains(&b'\n') && line.len() < MAX_LINE {
      let mut available = 0;
      // SAFETY: our pipe; only the count is asked for.
      let ok =
        unsafe { PeekNamedPipe(pipe, null_mut(), 0, null_mut(), &mut available, null_mut()) };
      if ok == 0 {
        break; // the client is gone
      }

      if available == 0 {
        if Instant::now() >= deadline {
          return None;
        }

        thread::sleep(POLL);
        continue;
      }

      let mut buf = [0u8; MAX_LINE];
      let (len, mut read) = ((available as usize).min(MAX_LINE) as u32, 0);
      // SAFETY: at most `len` bytes into `buf`, as many as are waiting: it does not block.
      if unsafe { ReadFile(pipe, buf.as_mut_ptr(), len, &mut read, null_mut()) } == 0 {
        break;
      }

      line.extend_from_slice(&buf[..read as usize]);
    }

    String::from_utf8(line).ok()
  }

  /// One pipe per home, by its path's FNV-1a hash (the same in every build), so a test's WAVO_HOME
  /// never meets the person's app.
  fn pipe_path(home: &Path) -> PathBuf {
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let bytes = home.as_os_str().as_encoded_bytes();
    let hash = (bytes.iter())
      .fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    PathBuf::from(format!(r"\\.\pipe\wavo-{hash:016x}"))
  }

  /// Connects, waiting while the pipe serves another client. The instance may only identify the
  /// client, not act as it: a pipe of that name made by another process gets no rights of ours.
  fn client(path: &Path) -> io::Result<File> {
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
      let mut options = File::options();
      options.read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION);
      match options.open(path) {
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && Instant::now() < deadline => {
          thread::sleep(Duration::from_millis(10));
        }
        result => return result,
      }
    }
  }

  /// Sends `cmd` to the instance for `home`, back once it has read it.
  pub fn send(home: &Path, cmd: &str) -> io::Result<()> {
    if cmd == "open" {
      // The instance runs in the background: its window comes to the front only if the process
      // in front (this one, a second launch) lets it.
      // SAFETY: no preconditions.
      unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }

    let mut pipe = client(&pipe_path(home))?;
    pipe.write_all(format!("{cmd}\n").as_bytes())?;
    // The instance hangs up once it has the line: the read ends then, with an error.
    let _ = pipe.read_to_end(&mut Vec::new());
    Ok(())
  }
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
    send(dir.path(), "quit").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "quit");

    drop(server);
    assert!(send(dir.path(), "quit").is_err(), "gone with the server");
    assert!(matches!(claim(dir.path()).unwrap(), Claim::Server(_)), "the lock is free");
  }

  #[cfg(unix)]
  #[test]
  fn a_dead_instances_socket_is_replaced() {
    use std::os::unix::net::UnixListener;

    let dir = TempDir::new("ctl-stale");
    drop(UnixListener::bind(unix::socket_path(dir.path()).unwrap()).unwrap());
    let (_server, rx) = serve(dir.path());
    send(dir.path(), "quit").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "quit");
  }
}
