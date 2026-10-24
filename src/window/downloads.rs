// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The models downloading, each on a thread of its own through wavo's [`Hub::pull`], which
//! resumes a broken download and, on a Mac, prepares Parakeet and GigaAM for the Neural Engine
//! afterwards. They outlive the settings window: [`Downloads`] is the app's, the window shows
//! what it holds. A cancelled download leaves its part in the cache for the next one to resume.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use wavo::hub::{Hub, ModelInfo};

/// How often a running download tells of its progress, at most.
const TELL: Duration = Duration::from_millis(100);
/// The speed shows once a download has run this long.
const RATE_AFTER: Duration = Duration::from_secs(1);

/// How a model's download goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Download {
  /// The file comes: bytes on disk of `total` (a part an earlier pull left counts), and bytes a
  /// second once it has run a while.
  Running { done: u64, total: u64, rate: Option<f64> },
  /// Prepared for the Neural Engine after the download: the system compiles it.
  Preparing,
  /// Why it stopped; the next one resumes it.
  Failed(String),
}

/// A step of a pull, as [`Downloads::start`]'s `pull` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
  Bytes { done: u64, total: u64 },
  Preparing,
}

/// What a running download holds: its state and its cancel.
struct Job {
  state: Download,
  cancel: Arc<AtomicBool>,
}

/// The downloads by model name; cheap to clone.
#[derive(Clone, Default)]
pub struct Downloads(Arc<Mutex<HashMap<&'static str, Job>>>);

impl Downloads {
  pub fn get(&self, name: &str) -> Option<Download> {
    self.0.lock().unwrap().get(name).map(|j| j.state.clone())
  }

  /// The download of `name` goes on as `state`, unless it is gone.
  fn set(&self, name: &str, state: Download) {
    if let Some(job) = self.0.lock().unwrap().get_mut(name) {
      job.state = state;
    }
  }

  /// Stops the download of `name` before its next chunk (or the next step of preparing), or
  /// forgets its failure.
  pub fn cancel(&self, name: &str) {
    let mut jobs = self.0.lock().unwrap();
    match jobs.get(name) {
      Some(Job { state: Download::Failed(_), .. }) => _ = jobs.remove(name),
      Some(job) => job.cancel.store(true, Ordering::Relaxed),
      None => {}
    }
  }

  /// Downloads `model` into `hub` on a thread of its own, unless it downloads already.
  /// `changed` runs on that thread as it goes (at most every [`TELL`]), and with `true` once it
  /// has ended, whichever way.
  pub fn pull(&self, hub: Hub, model: &'static ModelInfo, changed: impl Fn(bool) + Send + 'static) {
    let pull = move |step: &mut dyn FnMut(Step) -> ControlFlow<()>| {
      let progress = |p: wavo::hub::Progress| match p.preparing {
        Some(_) => step(Step::Preparing),
        None => step(Step::Bytes { done: p.done, total: p.total }),
      };
      let path = hub.pull(model.name, progress).map_err(|e| e.to_string())?;
      info!("model {} downloaded: {}", model.name, path.display());
      Ok(())
    };
    self.start(model, pull, changed);
  }

  /// [`pull`](Self::pull) with what pulls: `pull` reports each step and gets told to go on or
  /// stop.
  pub fn start(
    &self,
    model: &'static ModelInfo,
    pull: impl FnOnce(&mut dyn FnMut(Step) -> ControlFlow<()>) -> Result<(), String> + Send + 'static,
    changed: impl Fn(bool) + Send + 'static,
  ) {
    let cancel = Arc::new(AtomicBool::new(false));
    {
      let mut jobs = self.0.lock().unwrap();
      if jobs.get(model.name).is_some_and(|j| !matches!(j.state, Download::Failed(_))) {
        return;
      }

      let state = Download::Running { done: 0, total: model.size, rate: None };
      jobs.insert(model.name, Job { state, cancel: cancel.clone() });
    }

    let downloads = self.clone();
    let work = move || {
      let (started, mut told) = (Instant::now(), None::<Instant>);
      let mut from = None;
      let mut step = |s: Step| {
        let state = match s {
          Step::Preparing => Download::Preparing,
          Step::Bytes { done, total } => {
            let from = *from.get_or_insert(done);
            let secs = started.elapsed().as_secs_f64();
            let rate =
              (started.elapsed() >= RATE_AFTER && done > from).then(|| (done - from) as f64 / secs);
            Download::Running { done, total, rate }
          }
        };
        let tell = state == Download::Preparing || told.is_none_or(|t| t.elapsed() >= TELL);
        downloads.set(model.name, state);
        if tell {
          told = Some(Instant::now());
          changed(false);
        }

        match cancel.load(Ordering::Relaxed) {
          true => ControlFlow::Break(()),
          false => ControlFlow::Continue(()),
        }
      };

      match pull(&mut step) {
        Err(e) if !cancel.load(Ordering::Relaxed) => {
          warn!("download {}: {e}", model.name);
          downloads.set(model.name, Download::Failed(e));
        }
        _ => _ = downloads.0.lock().unwrap().remove(model.name),
      }

      changed(true);
    };

    let spawned = std::thread::Builder::new().name("download".into()).spawn(work);
    if let Err(e) = spawned {
      self.set(model.name, Download::Failed(format!("could not start: {e}")));
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::mpsc;

  use super::*;
  use crate::testing::poll;

  /// A pull that reports the steps it is sent, one at a time, and ends with what it is sent last.
  fn fake(
    steps: mpsc::Receiver<Result<Step, Result<(), String>>>,
  ) -> impl FnOnce(&mut dyn FnMut(Step) -> ControlFlow<()>) -> Result<(), String> + Send {
    move |step| {
      for s in steps {
        match s {
          Ok(s) if step(s).is_break() => return Err("cancelled".into()),
          Ok(_) => {}
          Err(end) => return end,
        }
      }

      Ok(())
    }
  }

  #[test]
  fn a_download_runs_prepares_and_ends() {
    let model = wavo::hub::lookup("whisper-tiny").unwrap();
    let downloads = Downloads::default();
    let (tx, rx) = mpsc::channel();
    let (ended_tx, ended) = mpsc::channel();
    downloads.start(model, fake(rx), move |end| _ = end.then(|| ended_tx.send(())));
    let starting = Download::Running { done: 0, total: model.size, rate: None };
    assert_eq!(downloads.get(model.name), Some(starting));

    tx.send(Ok(Step::Bytes { done: 10, total: 100 })).unwrap();
    let running = Download::Running { done: 10, total: 100, rate: None };
    poll("progress", Duration::from_secs(5), || {
      (downloads.get(model.name)? == running).then_some(())
    });
    tx.send(Ok(Step::Preparing)).unwrap();
    poll("preparing", Duration::from_secs(5), || {
      (downloads.get(model.name)? == Download::Preparing).then_some(())
    });

    tx.send(Err(Ok(()))).unwrap();
    ended.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(downloads.get(model.name), None, "done");
  }

  #[test]
  fn a_cancelled_download_is_gone_and_a_failed_one_stays_until_cancelled() {
    let model = wavo::hub::lookup("whisper-tiny").unwrap();
    let downloads = Downloads::default();
    let (tx, rx) = mpsc::channel();
    let (ended_tx, ended) = mpsc::channel();
    let tell = move |end: bool| _ = end.then(|| ended_tx.send(()));
    downloads.start(model, fake(rx), tell.clone());
    downloads.cancel(model.name);
    tx.send(Ok(Step::Bytes { done: 10, total: 100 })).unwrap();
    ended.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(downloads.get(model.name), None, "cancelled");

    let (tx, rx) = mpsc::channel();
    downloads.start(model, fake(rx), tell);
    tx.send(Err(Err("connection reset".into()))).unwrap();
    ended.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(downloads.get(model.name), Some(Download::Failed("connection reset".into())));
    downloads.cancel(model.name);
    assert_eq!(downloads.get(model.name), None, "forgotten");
  }
}
