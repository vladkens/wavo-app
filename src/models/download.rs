// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Model downloads into the Hugging Face cache through wavo's hub ([`Hub::pull`]). wavo resumes a
//! part an earlier pull left (`blobs/<etag>.incomplete`), retries a download that broke off and
//! checks the size. Progress goes to a callback, at most every [`PROGRESS_EVERY`], and on the first
//! and the last bytes.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use wavo::hub::{Hub, ModelInfo};

const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Test hook: bytes per second a download may take, to interrupt it midway and see the next one
/// resume.
const RATE_VAR: &str = "WAVO_DOWNLOAD_RATE";

#[derive(Debug, PartialEq)]
pub struct Progress {
  /// The model's file name, e.g. `whisper-tiny-Q8_0.gguf`.
  pub file: String,
  /// Bytes of the file so far, those of an earlier download included.
  pub received: u64,
  pub total: u64,
  /// What an earlier, interrupted download had left.
  pub resumed_from: u64,
}

/// Downloads `model` into `hub` on this thread, unless its latest revision is there already, and
/// returns the file's path; `on_progress` hears how far it got.
pub fn pull(
  hub: &Hub,
  model: &ModelInfo,
  mut on_progress: impl FnMut(&Progress),
) -> wavo::Result<PathBuf> {
  let rate = std::env::var(RATE_VAR).ok().and_then(|v| v.parse().ok()).filter(|&r| r > 0);
  let mut t = Tracker::new(model.file(), rate);
  let started = Instant::now();
  let path = hub.pull(model.name, |p| {
    if let Some(e) = p.retrying {
      warn!("{}: {e}; trying again", t.progress.file);
    }

    if t.step(p.done, p.total, Instant::now()) {
      on_progress(&t.progress);
    }

    ControlFlow::Continue(())
  })?;

  let p = &t.progress;
  let fetched = p.received.saturating_sub(p.resumed_from);
  let secs = started.elapsed().as_secs_f64();
  info!(
    "{}: downloaded, {} bytes ({} resumed) in {secs:.1} s, {:.1} MB/s: {}",
    p.file,
    p.received,
    p.resumed_from,
    fetched as f64 / secs.max(1e-3) / 1e6,
    path.display()
  );
  Ok(path)
}

/// The progress of one download, from wavo's callbacks.
struct Tracker {
  progress: Progress,
  /// Bytes per second at most ([`RATE_VAR`]).
  rate: Option<u64>,
  /// When the transfer started, for the rate.
  since: Option<Instant>,
  reported: Option<Instant>,
}

impl Tracker {
  fn new(file: String, rate: Option<u64>) -> Self {
    let progress = Progress { file, received: 0, total: 0, resumed_from: 0 };
    Self { progress, rate, since: None, reported: None }
  }

  /// wavo is at `done` of `total` bytes at `now`: whether to report. The first call says what an
  /// earlier download left. With a rate, it sleeps to keep to it.
  fn step(&mut self, done: u64, total: u64, now: Instant) -> bool {
    let p = &mut self.progress;
    let since = *self.since.get_or_insert_with(|| {
      p.resumed_from = done;
      if done > 0 {
        info!("{}: resuming at {done} bytes", p.file);
      }

      now
    });

    p.received = done;
    p.total = total;
    if let Some(rate) = self.rate {
      thread::sleep(pace(rate, done.saturating_sub(p.resumed_from), now.duration_since(since)));
    }

    let due = self.reported.is_none_or(|t| now.duration_since(t) >= PROGRESS_EVERY);
    let report = due || done == total;
    if report {
      self.reported = Some(now);
    }

    report
  }
}

/// How long to wait so that `fetched` bytes after `elapsed` keep to `rate` bytes per second.
fn pace(rate: u64, fetched: u64, elapsed: Duration) -> Duration {
  Duration::from_secs_f64(fetched as f64 / rate as f64).saturating_sub(elapsed)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tracks_resume_and_reports() {
    let mut t = Tracker::new("m.gguf".into(), None);
    let t0 = Instant::now();
    let ms = |n| t0 + Duration::from_millis(n);
    assert!(t.step(40, 100, ms(0)));
    assert_eq!(t.progress.resumed_from, 40);
    assert!(!t.step(50, 100, ms(50)), "too soon to report");
    assert!(t.step(60, 100, ms(100)));
    let expected = Progress { file: "m.gguf".into(), received: 60, total: 100, resumed_from: 40 };
    assert_eq!(t.progress, expected);
    assert!(t.step(100, 100, ms(120)), "the last bytes");
  }

  #[test]
  fn paces_to_the_rate() {
    let s = Duration::from_secs;
    assert_eq!(pace(1_000, 2_000, s(1)), s(1));
    assert_eq!(pace(1_000, 2_000, s(3)), Duration::ZERO);
    assert_eq!(pace(1_000, 0, Duration::ZERO), Duration::ZERO);
  }
}
