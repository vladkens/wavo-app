// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The files work's own parts; the session runs it ([crate::session]). One transcript runs at a
//! time: a thread reads its file a chunk ahead ([read]), each chunk is a background job of the
//! engine, and its segments go into the store as it is done. A dictation goes first: the chunk in
//! the engine is cancelled and goes again after it.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, bounded};
use tracing::warn;

use crate::decode::{self, Chunk, Chunks};
use crate::engine::Engine;
use crate::segments::Segment;
use crate::transcripts::Transcript;

/// The audio of one engine job: a yield to a dictation or a quit redoes at most this much.
pub const CHUNK_MS: u32 = 60_000;

/// What the window and the menu show of the transcript running now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
  pub id: i64,
  pub name: String,
  /// How far into its audio the text goes.
  pub done_ms: i64,
  /// How long its audio is, if the file tells.
  pub duration_ms: Option<i64>,
  /// How long the rest takes at this run's speed, once a chunk of it is done.
  pub left_ms: Option<i64>,
  /// It waits for the dictation in progress.
  pub waiting: bool,
}

/// What [read] tells, in this order: `Opened`, then the chunks, then `End` or `Failed`.
pub enum Read {
  /// The file opened, its audio this long if it tells.
  Opened(Option<i64>),
  Chunk(Chunk),
  End,
  Failed(String),
}

/// The audio of the file at `path` from `from_ms` on, read on a thread of its own a chunk ahead of
/// the one in the engine; the thread ends once the receiver is dropped.
pub fn read(path: PathBuf, from_ms: i64) -> Receiver<Read> {
  let (tx, rx) = bounded(1);
  let run = move || {
    crate::user_initiated();
    let mut chunks = match Chunks::open(&path, from_ms, CHUNK_MS) {
      Ok(chunks) => chunks,
      Err(e) => return drop(tx.send(Read::Failed(format!("{e:#}")))),
    };

    if tx.send(Read::Opened(chunks.duration_ms())).is_err() {
      return;
    }

    loop {
      let (read, last) = match chunks.next() {
        Ok(Some(chunk)) => (Read::Chunk(chunk), false),
        Ok(None) => (Read::End, true),
        Err(e) => (Read::Failed(format!("{e:#}")), true),
      };

      if tx.send(read).is_err() || last {
        return;
      }
    }
  };

  // Without its thread the receiver is closed: the run fails.
  if let Err(e) = thread::Builder::new().name("files".into()).spawn(run) {
    warn!("files: the reader did not start: {e}");
  }

  rx
}

/// Whether a file of `path`'s extension is one Wavo tries to read: the picker's [EXTENSIONS], and
/// those mostly of Opus, which the decoder tells apart.
///
/// [EXTENSIONS]: decode::EXTENSIONS
pub fn readable(path: &Path) -> bool {
  let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
  ext.is_some_and(|e| decode::EXTENSIONS.contains(&e.as_str()) || decode::MOSTLY_OPUS.contains(&&*e))
}

/// A transcript running: its file read a chunk at a time, each chunk an engine job.
pub struct Run {
  pub id: i64,
  pub name: String,
  /// The model's file and the language it is asked in.
  pub model: PathBuf,
  pub language: Option<String>,
  pub reads: Receiver<Read>,
  pub duration_ms: Option<i64>,
  pub done_ms: i64,
  /// The chunk read last, until its segments are in.
  pub chunk: Option<Chunk>,
  /// The chunk's job, on `engine`.
  pub job: Option<u64>,
  /// The engine the chunks go to, and whether it is the dictation's, which keeps the model loaded
  /// for them until released.
  pub engine: Option<(Engine, bool)>,
  left_ms: Option<i64>,
  /// Where this run started in the audio, and when: its speed tells the time left.
  from: (i64, Instant),
  _awake: nimbo::activity::Activity,
}

impl Run {
  /// `t` from its `done_ms` on, with the model at `model`.
  pub fn start(t: &Transcript, model: PathBuf) -> Self {
    Self {
      id: t.id,
      name: t.name.clone(),
      model,
      language: t.language.clone(),
      reads: read(t.path.clone(), t.done_ms),
      duration_ms: t.duration_ms,
      done_ms: t.done_ms,
      chunk: None,
      job: None,
      engine: None,
      left_ms: None,
      from: (t.done_ms, Instant::now()),
      _awake: nimbo::activity::begin("Transcribing a file"),
    }
  }

  /// The chunk's job is done with `segments` from its start: they and the time done, offset into
  /// the audio, and the time left at the speed so far.
  pub fn done(&mut self, segments: Vec<Segment>) -> (Vec<Segment>, i64) {
    self.job = None;
    let Some(chunk) = self.chunk.take() else { return (Vec::new(), self.done_ms) };
    let at = chunk.start_ms;
    let segments = segments.into_iter().map(|s| Segment {
      start_ms: s.start_ms + at,
      end_ms: s.end_ms + at,
      ..s
    });

    self.done_ms = chunk.end_ms();
    let (from, started) = self.from;
    let speed = (self.done_ms - from) as f64 / started.elapsed().as_millis().max(1) as f64;
    let rest = self.duration_ms.map(|d| (d - self.done_ms).max(0) as f64);
    self.left_ms = rest.filter(|_| speed > 0.0).map(|r| (r / speed) as i64);
    (segments.collect(), self.done_ms)
  }

  pub fn status(&self, waiting: bool) -> FileStatus {
    let (id, name, done_ms, duration_ms) =
      (self.id, self.name.clone(), self.done_ms, self.duration_ms);
    FileStatus { id, name, done_ms, duration_ms, left_ms: self.left_ms, waiting }
  }
}

impl Drop for Run {
  /// Its chunk in the engine goes; the reader ends with the receiver.
  fn drop(&mut self) {
    if let (Some(id), Some((engine, _))) = (self.job, &self.engine) {
      engine.cancel(id);
    }
  }
}
