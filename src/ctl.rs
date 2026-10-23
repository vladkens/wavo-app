// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Control socket: `wavo-app ctl` and a second launch talk to the running app over a Unix socket.
//! One request per connection: a JSON line in, a JSON line out.
//!
//! The path is `/tmp/wavo-<uid>-<device>-<inode of the app's home>.sock`: one socket per home, so
//! test instances with their own `WAVO_HOME` never meet the person's app, and short whatever the
//! home (`sun_path` holds 104 bytes on macOS, 108 on Linux). The socket is `0600`, and a socket
//! file is only used if it is a socket of this user: `/tmp` is shared, but its sticky bit keeps
//! others from replacing ours.
//!
//! Single instance: the server holds an `flock` on `wavo.lock` in the home for its whole life (the
//! kernel drops it when the process dies, however it dies). Whoever gets the lock owns the socket
//! path, so a socket file found there is a dead instance's and is replaced; whoever does not get
//! it talks to the instance that has it. Two launches at once cannot both serve.
//!
//! The server is a thread blocked in `accept`: nothing runs while nobody asks.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

/// Longest socket path macOS accepts (`sun_path` is 104 bytes with the terminating NUL).
pub const MAX_PATH: usize = 103;
/// Requests are a few dozen bytes; anything this long is not one.
const MAX_REQUEST: u64 = 64 << 10;
/// How long a connection may take to send its request or read the reply.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the client waits for a reply: starting a recording opens the microphone.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a second launch waits for the instance holding the lock to answer: it may be starting.
const STARTING_WAIT: Duration = Duration::from_secs(3);
/// The single-instance lock in the app's home.
pub const LOCK_FILE: &str = "wavo.lock";

/// A request, and `wavo-app ctl`'s subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::Subcommand)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
  /// Start or stop dictation
  Toggle,
  /// Start dictation
  Start,
  /// Stop dictation and transcribe
  Stop,
  /// Cancel recording or transcription
  Cancel,
  /// Print the app's state as JSON
  Status,
  /// Transcribe a history entry's recording again
  Retranscribe { id: i64 },
  /// Delete a history entry and its recording
  Delete { id: i64 },
  /// Free the model's memory now; the next dictation loads it again
  Unload,
  /// Quit the running app
  Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<String>,
  #[serde(default, skip_serializing_if = "Value::is_null")]
  pub result: Value,
}

impl Reply {
  pub fn ok(result: impl Serialize) -> Self {
    match serde_json::to_value(result) {
      Ok(result) => Self { error: None, result },
      Err(e) => Self::error(format!("encode the reply: {e}")),
    }
  }

  pub fn error(message: impl Into<String>) -> Self {
    Self { error: Some(message.into()), result: Value::Null }
  }
}

impl From<anyhow::Result<Value>> for Reply {
  fn from(result: anyhow::Result<Value>) -> Self {
    match result {
      Ok(result) => Self { error: None, result },
      Err(e) => Self::error(format!("{e:#}")),
    }
  }
}

/// The socket of the app whose home is `home`, which must exist. The same home under another
/// spelling (a symlink, `..`) is the same file, so the same app.
pub fn socket_path(home: &Path) -> io::Result<PathBuf> {
  let meta = fs::metadata(home)?;
  Ok(PathBuf::from(format!("/tmp/wavo-{}-{:x}-{:x}.sock", uid(), meta.dev(), meta.ino())))
}

fn uid() -> u32 {
  // SAFETY: getuid has no preconditions.
  unsafe { libc::getuid() }
}

/// The outcome of [`claim`].
pub enum Claim {
  /// This process is the instance for the home now.
  Server(Server),
  /// Another instance is, and answers on the socket.
  Running,
}

/// Becomes the instance of the app whose home is `home` (which must exist), unless another one is:
/// see the module docs.
pub fn claim(home: &Path) -> io::Result<Claim> {
  let path = socket_path(home)?;
  if path.as_os_str().len() > MAX_PATH {
    return Err(io::Error::other(format!("socket path too long: {}", path.display())));
  }

  let lock = File::options().create(true).append(true).mode(0o600).open(home.join(LOCK_FILE))?;
  match lock.try_lock() {
    Ok(()) => {}
    Err(fs::TryLockError::WouldBlock) => return wait_for_instance(&path),
    Err(fs::TryLockError::Error(e)) => return Err(e),
  }

  // The lock is ours, so whatever is at the path is a dead instance's.
  if fs::symlink_metadata(&path).is_ok() {
    check_ours(&path)?;
    debug!("removing the stale socket {}", path.display());
    fs::remove_file(&path)?;
  }

  let listener = UnixListener::bind(&path)?;
  let server = Server { listener: Some(listener), path, lock: Some(lock) };
  fs::set_permissions(&server.path, fs::Permissions::from_mode(0o600))?;
  Ok(Claim::Server(server))
}

/// Another process holds the lock: it answers once it has bound the socket.
fn wait_for_instance(path: &Path) -> io::Result<Claim> {
  let deadline = Instant::now() + STARTING_WAIT;
  loop {
    if fs::symlink_metadata(path).is_ok() {
      check_ours(path)?;
      match UnixStream::connect(path) {
        Ok(_) => return Ok(Claim::Running),
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {}
        Err(e) => return Err(e),
      }
    }

    if Instant::now() >= deadline {
      return Err(io::Error::other(format!(
        "another instance holds the lock but does not answer on {}",
        path.display()
      )));
    }

    thread::sleep(Duration::from_millis(20));
  }
}

/// A socket file is only used if it is a socket of this user.
fn check_ours(path: &Path) -> io::Result<()> {
  let meta = fs::symlink_metadata(path)?;
  if !meta.file_type().is_socket() || meta.uid() != uid() {
    return Err(io::Error::other(format!("{} is not a socket of this user", path.display())));
  }

  Ok(())
}

/// A claimed socket, not serving yet. Dropped unused, it removes the socket file.
pub struct Server {
  /// Both taken by [`Server::spawn`].
  listener: Option<UnixListener>,
  path: PathBuf,
  /// Held until the socket file is removed.
  lock: Option<File>,
}

impl Drop for Server {
  fn drop(&mut self) {
    if self.listener.is_some() {
      let _ = fs::remove_file(&self.path);
    }
  }
}

impl Server {
  /// Serves requests with `handler` on a "ctl" thread until the returned handle is dropped.
  pub fn spawn(
    mut self,
    handler: impl Fn(Request) -> Reply + Send + 'static,
  ) -> io::Result<Serving> {
    let stop = Arc::new(AtomicBool::new(false));
    let (listener, lock) = (self.listener.take().unwrap(), self.lock.take().unwrap());
    let path = self.path.clone();
    let stopping = stop.clone();
    let thread = thread::Builder::new().name("ctl".into()).spawn(move || {
      for stream in listener.incoming() {
        if stopping.load(Ordering::Acquire) {
          return;
        }

        match stream {
          Ok(stream) => {
            if let Err(e) = serve(stream, &handler) {
              debug!("ctl connection: {e}");
            }
          }
          Err(e) => {
            warn!("ctl accept: {e}");
            thread::sleep(Duration::from_millis(50));
          }
        }
      }
    })?;

    Ok(Serving { path, stop, thread: Some(thread), _lock: lock })
  }
}

/// A serving socket. Dropping it stops the thread and removes the socket file, then releases the
/// lock.
pub struct Serving {
  path: PathBuf,
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
  _lock: File,
}

impl Drop for Serving {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    // Wakes the thread from `accept`. With the socket file gone (removed behind the app's back)
    // nothing can: the thread is left blocked rather than the drop.
    let woken = UnixStream::connect(&self.path).is_ok();
    if let Some(thread) = self.thread.take().filter(|_| woken) {
      let _ = thread.join();
    }

    let _ = fs::remove_file(&self.path);
  }
}

/// One request read from `stream`, answered by `handler`, the reply written back.
fn serve(stream: UnixStream, handler: &impl Fn(Request) -> Reply) -> io::Result<()> {
  stream.set_read_timeout(Some(IO_TIMEOUT))?;
  stream.set_write_timeout(Some(IO_TIMEOUT))?;
  let mut line = String::new();
  BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line)?;
  if line.trim().is_empty() {
    return Ok(()); // a wake-up or a probe
  }

  let reply = match serde_json::from_str::<Request>(&line) {
    Ok(request) => {
      debug!(?request, "ctl");
      handler(request)
    }
    Err(e) => Reply::error(format!("bad request: {e}")),
  };

  let mut out = serde_json::to_vec(&reply)?;
  out.push(b'\n');
  (&stream).write_all(&out)
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
  #[error("wavo-app is not running (no answer on {})", .0.display())]
  NotRunning(PathBuf),
  #[error("control socket {}: {error}", .path.display())]
  Io { path: PathBuf, error: io::Error },
}

/// Sends `request` to the instance serving `path` and waits for its reply.
pub fn request(path: &Path, request: &Request) -> Result<Reply, ClientError> {
  let io = |error: io::Error| ClientError::Io { path: path.to_owned(), error };
  match fs::symlink_metadata(path) {
    Err(e) if e.kind() == ErrorKind::NotFound => return Err(ClientError::NotRunning(path.into())),
    _ => check_ours(path).map_err(io)?,
  }

  let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
    ErrorKind::NotFound | ErrorKind::ConnectionRefused => ClientError::NotRunning(path.into()),
    _ => io(e),
  })?;
  stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
  stream.set_read_timeout(Some(REPLY_TIMEOUT)).map_err(io)?;
  let mut line = serde_json::to_vec(request).map_err(|e| io(e.into()))?;
  line.push(b'\n');
  (&stream).write_all(&line).map_err(io)?;

  let mut reply = String::new();
  BufReader::new(&stream).read_line(&mut reply).map_err(io)?;
  serde_json::from_str(&reply).map_err(|e| io(e.into()))
}

#[cfg(test)]
mod tests {
  use std::sync::{Barrier, Mutex};

  use super::*;
  use crate::testing::TempDir;

  type Seen = Arc<Mutex<Vec<Request>>>;

  /// The instance for `home`, answering `{"state": "idle"}` and noting each request.
  fn serve_echo(home: &Path) -> (Serving, PathBuf, Seen) {
    let Claim::Server(server) = claim(home).unwrap() else { panic!("{} is taken", home.display()) };
    let seen = Seen::default();
    let noted = seen.clone();
    let serving = server
      .spawn(move |request| {
        noted.lock().unwrap().push(request);
        Reply::ok(serde_json::json!({ "state": "idle" }))
      })
      .unwrap();
    (serving, socket_path(home).unwrap(), seen)
  }

  #[test]
  fn socket_path_is_short_and_per_home() {
    let dir = TempDir::new("ctl-path");
    let long = dir.path().join("a".repeat(200)).join("b".repeat(200));
    fs::create_dir_all(&long).unwrap();
    let path = socket_path(&long).unwrap();
    assert!(path.as_os_str().len() <= MAX_PATH, "{} bytes", path.as_os_str().len());
    assert!(path.starts_with("/tmp"), "{}", path.display());
    let same = long.join("..").join("b".repeat(200));
    assert_eq!(path, socket_path(&same).unwrap(), "same home");
    assert_ne!(path, socket_path(dir.path()).unwrap(), "another home");

    // And it works: an instance on it answers.
    let (serving, socket, _) = serve_echo(&long);
    assert_eq!(socket, path);
    assert!(request(&path, &Request::Status).unwrap().error.is_none());
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    drop(serving);
    assert!(!path.exists(), "removed on drop");
  }

  #[test]
  fn requests_round_trip_as_json_lines() {
    let dir = TempDir::new("ctl-roundtrip");
    let (_serving, path, seen) = serve_echo(dir.path());
    let requests = [
      Request::Toggle,
      Request::Retranscribe { id: 42 },
      Request::Delete { id: 7 },
      Request::Unload,
    ];
    for r in &requests {
      let reply = request(&path, r).unwrap();
      assert_eq!(reply.result["state"], "idle");
    }

    assert_eq!(*seen.lock().unwrap(), requests);
    let json = serde_json::to_string(&Request::Delete { id: 7 }).unwrap();
    assert_eq!(json, r#"{"cmd":"delete","id":7}"#);

    // A malformed request gets an error, and the server goes on.
    let mut raw = UnixStream::connect(&path).unwrap();
    raw.write_all(b"{\"cmd\":\"dance\"}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(&raw).read_line(&mut reply).unwrap();
    assert!(reply.contains("bad request"), "{reply}");
    assert!(request(&path, &Request::Status).is_ok());
  }

  #[test]
  fn nobody_listening_is_not_running() {
    let dir = TempDir::new("ctl-none");
    let path = socket_path(dir.path()).unwrap();
    assert!(matches!(request(&path, &Request::Status), Err(ClientError::NotRunning(_))));
  }

  #[test]
  fn a_stale_socket_is_replaced() {
    let dir = TempDir::new("ctl-stale");
    let path = socket_path(dir.path()).unwrap();
    // A dead instance: the file stays, nobody accepts, nobody holds the lock.
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    assert!(matches!(request(&path, &Request::Status), Err(ClientError::NotRunning(_))));

    let (_serving, _, seen) = serve_echo(dir.path());
    assert!(request(&path, &Request::Status).unwrap().error.is_none());
    assert_eq!(seen.lock().unwrap().len(), 1);
  }

  #[test]
  fn a_second_instance_finds_the_first() {
    let dir = TempDir::new("ctl-second");
    let (_first, path, seen) = serve_echo(dir.path());
    assert!(matches!(claim(dir.path()).unwrap(), Claim::Running));
    assert_eq!(request(&path, &Request::Status).unwrap().result["state"], "idle");
    assert_eq!(*seen.lock().unwrap(), [Request::Status]);
  }

  #[test]
  fn launches_at_the_same_time_make_one_instance() {
    let dir = TempDir::new("ctl-race");
    let home = dir.path().to_owned();
    let barrier = Arc::new(Barrier::new(8));
    let claims: Vec<_> = (0..8)
      .map(|_| {
        let (home, barrier) = (home.clone(), barrier.clone());
        thread::spawn(move || {
          barrier.wait();
          claim(&home).unwrap()
        })
      })
      .collect();
    let claims: Vec<Claim> = claims.into_iter().map(|t| t.join().unwrap()).collect();
    let servers = claims.iter().filter(|c| matches!(c, Claim::Server(_))).count();
    assert_eq!(servers, 1, "exactly one instance");
  }

  #[test]
  fn a_file_that_is_not_our_socket_is_left_alone() {
    let dir = TempDir::new("ctl-file");
    let path = socket_path(dir.path()).unwrap();
    fs::write(&path, b"data").unwrap();
    let claimed = claim(dir.path()).err().map(|e| e.to_string());
    let asked = request(&path, &Request::Status).err().map(|e| e.to_string());
    let kept = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(claimed.unwrap().contains("not a socket"));
    assert!(asked.unwrap().contains("not a socket"));
    assert_eq!(kept, b"data");
  }
}
