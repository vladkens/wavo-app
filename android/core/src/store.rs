// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

//! The models in the app's folder, and their downloads from Hugging Face. Not `wavo::hub`'s cache:
//! that one takes file locks, which Rust's std doesn't have on Android (`lock() not supported`),
//! and its layout of links is for sharing with other programs, which an app's private folder
//! never does. Here a model is one file, `<root>/<full name>-Q8_0.gguf`, and its download in
//! progress `<that>.part`; one process uses the folder, and the Kotlin side runs one download of
//! a model at a time.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use wavo::hub::{ModelInfo, lookup};

use crate::engine::Result;

/// How many more times a download that broke on the network resumes, after 1, 2 and 4 s.
const RETRIES: u32 = 3;

/// A connection that takes longer, or a response that sends nothing for as long, has failed.
const CONNECT: Duration = Duration::from_secs(15);
const IDLE: Duration = Duration::from_secs(30);

fn text(e: impl std::fmt::Display) -> String {
  e.to_string()
}

fn file(root: &Path, m: &ModelInfo) -> PathBuf {
  root.join(m.file())
}

fn part(root: &Path, m: &ModelInfo) -> PathBuf {
  root.join(format!("{}.part", m.file()))
}

/// The file of a downloaded model.
pub fn find(root: &Path, m: &ModelInfo) -> Option<PathBuf> {
  Some(file(root, m)).filter(|p| p.is_file())
}

/// Why a try failed, and whether the next one may get further.
struct Failed {
  message: String,
  retry: bool,
}

impl Failed {
  fn network(e: impl std::fmt::Display) -> Self {
    Self { message: format!("download failed: {e}"), retry: true }
  }

  fn fatal(e: impl std::fmt::Display) -> Self {
    Self { message: e.to_string(), retry: false }
  }
}

/// Downloads a model into `root` and returns its file; a download cut short resumes from its
/// `.part`. `progress` gets the bytes done and the total, and stops the download by returning
/// `false`, which fails it with `cancelled`.
pub fn pull(root: &Path, name: &str, progress: impl FnMut(u64, u64) -> bool) -> Result<PathBuf> {
  let m = lookup(name).map_err(text)?;
  if let Some(path) = find(root, m) {
    return Ok(path);
  }

  let url = format!("https://huggingface.co/{}/resolve/main/{}", m.repo(), m.file());
  fetch(root, &url, &m.file(), progress)
}

/// Downloads the file at `url` into `root` as `name`, as [pull] does a model: the speaker models,
/// which aren't wavo's. `name` is a plain file name.
pub fn fetch(root: &Path, url: &str, name: &str, mut progress: impl FnMut(u64, u64) -> bool) -> Result<PathBuf> {
  if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
    return Err(format!("{name:?} isn't a file name"));
  }

  let (part, file) = (root.join(format!("{name}.part")), root.join(name));
  if file.is_file() {
    return Ok(file);
  }

  fs::create_dir_all(root).map_err(text)?;
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .timeout_connect(Some(CONNECT))
    .timeout_recv_response(Some(IDLE))
    .timeout_per_read(Some(IDLE))
    .build()
    .into();

  let mut retry = 0;
  loop {
    match download(&agent, url, &part, &mut progress) {
      Ok(()) => break,
      Err(e) if e.retry && retry < RETRIES => {
        std::thread::sleep(Duration::from_secs(1 << retry));
        retry += 1;
      }
      Err(e) => return Err(e.message),
    }
  }

  fs::rename(&part, &file).map_err(text)?;
  Ok(file)
}

/// One try: continues `part` from its length to the end of the file at `url`.
fn download(
  agent: &ureq::Agent,
  url: &str,
  part: &Path,
  progress: &mut impl FnMut(u64, u64) -> bool,
) -> std::result::Result<(), Failed> {
  let mut out = OpenOptions::new().create(true).truncate(false).write(true).open(part);
  let out = out.as_mut().map_err(|e| Failed::fatal(&e))?;
  let have = out.metadata().map_err(Failed::fatal)?.len();

  let resp = agent.get(url).header("Accept-Encoding", "identity").header("Range", format!("bytes={have}-")).call();
  let resp = match resp {
    Ok(resp) => resp,
    // Past the end: the part is the whole file, cut off before its rename.
    Err(ureq::Error::StatusCode(416)) if have > 0 => return Ok(()),
    Err(ureq::Error::StatusCode(code)) => return Err(Failed::fatal(format!("{url}: HTTP {code}"))),
    Err(e) => return Err(Failed::network(e)),
  };

  let length = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
  let length = length.ok_or_else(|| Failed::fatal(format!("{url}: no file size")))?;
  // A server that ignores Range answers 200 with the whole file.
  let mut done = if resp.status() == 206 { have } else { 0 };
  let total = done + length;
  out.set_len(done).map_err(Failed::fatal)?;
  out.seek(SeekFrom::Start(done)).map_err(Failed::fatal)?;

  let mut body = resp.into_body().into_reader();
  let mut buf = vec![0; 1 << 16];
  loop {
    if !progress(done, total) {
      return Err(Failed::fatal("cancelled"));
    }

    match body.read(&mut buf) {
      Ok(0) if done < total => return Err(Failed::network(format!("it ended at {done} of {total} bytes"))),
      Ok(0) => return out.flush().map_err(Failed::fatal),
      Ok(n) => {
        // A full disk, mostly: no retry helps.
        out.write_all(&buf[..n]).map_err(Failed::fatal)?;
        done += n as u64;
      }
      Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
      Err(e) => return Err(Failed::network(e)),
    }
  }
}

/// Deletes a model and what a download left of it; the bytes freed.
pub fn remove(root: &Path, name: &str) -> Result<u64> {
  let m = lookup(name).map_err(text)?;
  let mut freed = 0;
  for path in [file(root, m), part(root, m)] {
    if let Ok(meta) = fs::metadata(&path) {
      fs::remove_file(&path).map_err(text)?;
      freed += meta.len();
    }
  }

  Ok(freed)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn finds_and_removes_a_model_and_its_part() {
    let root = std::env::temp_dir().join(format!("wavo-android-store-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let m = lookup("gigaam-v3").unwrap();
    assert_eq!(find(&root, m), None);
    assert_eq!(remove(&root, "gigaam-v3"), Ok(0));

    fs::write(root.join("gigaam-v3-e2e-rnnt-Q8_0.gguf"), [0; 10]).unwrap();
    fs::write(root.join("gigaam-v3-e2e-rnnt-Q8_0.gguf.part"), [0; 4]).unwrap();
    assert_eq!(find(&root, m), Some(root.join("gigaam-v3-e2e-rnnt-Q8_0.gguf")));
    // A model there is returned without the network.
    assert_eq!(pull(&root, "gigaam-v3", |_, _| true), Ok(root.join("gigaam-v3-e2e-rnnt-Q8_0.gguf")));

    assert_eq!(remove(&root, "gigaam-v3"), Ok(14));
    assert_eq!(find(&root, m), None);
    assert!(remove(&root, "no-such-model").is_err());
    // A name that would leave the folder never reaches the network.
    assert!(fetch(&root, "https://example.invalid/x", "../x", |_, _| true).is_err());
    assert!(fetch(&root, "https://example.invalid/x", "", |_, _| true).is_err());
    fs::remove_dir_all(&root).unwrap();
  }
}
