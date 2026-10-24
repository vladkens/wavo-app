// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The session's text output. The target is the app in focus and its focused window when the
//! recording starts ([Focused]); the paste runs on the main thread: the checks of [focus], then
//! [paste]. A copy (auto-paste off) goes on the clipboard there too.
//!
//! The session thread waits for the main thread at most [`PASTE_WAIT`]. A paste the main thread
//! gets to after that is dropped before it changes anything ([`Gate`]), so the history never says
//! "not pasted" about a text that was.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crossbeam_channel::bounded;
use nimbo::focus::Focused;
use nimbo::{Clipboard, SystemClipboard};
use tracing::warn;

use super::{focus, paste, proxy};
use crate::session::output::{NotPasted, TextOutput};

/// How long the session thread waits for the main thread for the target.
const MAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// How long it waits for a paste or copy to start (and then again for it to end).
const PASTE_WAIT: Duration = Duration::from_secs(2);

pub struct GuiOutput;

impl TextOutput for GuiOutput {
  type Target = Option<Focused>;

  fn target(&mut self) -> Option<Focused> {
    proxy().call(|_| focus::current(), MAIN_TIMEOUT).flatten()
  }

  fn paste(&mut self, text: &str, target: &Option<Focused>, keep: bool) -> Result<(), NotPasted> {
    let Some(target) = target.clone() else {
      return Err(NotPasted::Failed(
        "the app in focus when the dictation started is unknown".into(),
      ));
    };

    let text = text.to_owned();
    let pasted = gated(move |gate| {
      if !nimbo::permissions::accessibility(false) {
        return Err(NotPasted::Failed("no Accessibility access: ⌘V cannot be sent".into()));
      }

      focus::check(&target)?;
      if nimbo::permissions::secure_input() {
        warn!("paste: Secure Input is on (some app holds it): ⌘V may not arrive");
      }

      // Again right before ⌘V, after the clipboard's snapshot, which may take a while.
      paste::paste(&text, keep, || gate.start(), || focus::check(&target))
    });
    pasted.unwrap_or_else(|e| Err(NotPasted::Failed(e.into())))
  }

  fn copy(&mut self, text: &str) -> anyhow::Result<()> {
    let text = text.to_owned();
    match gated(move |gate| gate.start().then(|| SystemClipboard.write(&text))) {
      Ok(Some(())) => Ok(()),
      Ok(None) | Err(_) => anyhow::bail!("the app was too busy to copy in time"),
    }
  }
}

/// Runs `job` on the main thread; the session waits [`PASTE_WAIT`] for it to start changing
/// something (`gate.start()`), and as long again for its end once it has. A job that did not start
/// in time finds the gate closed and changes nothing.
fn gated<R: Send + 'static>(
  job: impl FnOnce(&Gate) -> R + Send + 'static,
) -> Result<R, &'static str> {
  let gate = Gate::new();
  let (tx, rx) = bounded(1);
  let job_gate = gate.clone();
  proxy().post(move |_| _ = tx.send(job(&job_gate)));

  match rx.recv_timeout(PASTE_WAIT) {
    Ok(result) => Ok(result),
    Err(_) if gate.abandon() => Err("the app was too busy to paste in time"),
    // It started meanwhile: it ends quickly.
    Err(_) => rx.recv_timeout(PASTE_WAIT).map_err(|_| "the paste did not finish in time"),
  }
}

/// Whether a paste the session waits for may still start.
#[derive(Clone)]
struct Gate(Arc<AtomicU8>);

impl Gate {
  const WAITING: u8 = 0;
  const STARTED: u8 = 1;
  const ABANDONED: u8 = 2;

  fn new() -> Self {
    Self(Arc::new(AtomicU8::new(Self::WAITING)))
  }

  /// The main thread is about to change something; false if the session gave up.
  fn start(&self) -> bool {
    self.swap(Self::STARTED)
  }

  /// The session gives up; false if the paste started already.
  fn abandon(&self) -> bool {
    self.swap(Self::ABANDONED)
  }

  fn swap(&self, to: u8) -> bool {
    self.0.compare_exchange(Self::WAITING, to, Ordering::AcqRel, Ordering::Acquire).is_ok()
  }
}
