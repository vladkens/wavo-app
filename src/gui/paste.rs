// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Paste on the main thread: the text goes on the clipboard as nimbo's
//! [offer](SystemClipboard::offer), ⌘V goes to the app in focus, and the previous contents come
//! back once the app has read the text.
//!
//! - The offer is marked for clipboard managers to skip and handed out only when someone reads it:
//!   that read says when the app took it.
//! - Before that a [`Snapshot`] of every item and type on the clipboard is taken. It is put back
//!   [`QUIET`] after the read of the text, but not before [`MIN_HOLD`] after ⌘V (an early reader
//!   that is not the target must not cut the target short), or [`CAP`] after ⌘V without a read; and
//!   only while the clipboard still [holds](SystemClipboard::holds) the offer: what the user copied
//!   meanwhile stays.
//! - A paste while the previous one waits for its restore takes over that one's snapshot: the
//!   user's clipboard comes back, not the previous text.
//! - "Keep the text on the clipboard": the text is written at once as plain text, without the
//!   markers (clipboard managers may keep it, it outlives the app), and nothing is restored. When
//!   the app may not read the clipboard without asking ([`readable`](SystemClipboard::readable):
//!   macOS 15.4+, System Settings → Privacy & Security → Paste from Other Apps), or nothing of it
//!   could be read, the text is offered at once, marked: there is no snapshot to restore.
//! - ⌘V: nimbo's [posted keys](nimbo::keys::post), the key that types "v" with Command in the
//!   current layout, with the Command flag only (a held Option does not make it ⌥⌘V).

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use nimbo::clipboard::{Offer, Snapshot};
use nimbo::{Clipboard, Key, Modifiers, Shortcut, SystemClipboard};
use tracing::{debug, info, warn};

use super::proxy;
use crate::session::NotPasted;

/// The restore waits this long after the read: some apps read more than once per paste.
const QUIET: Duration = Duration::from_millis(200);
/// The text stays at least this long after ⌘V.
const MIN_HOLD: Duration = Duration::from_millis(500);
/// The longest the text stays on the clipboard when nobody reads it (the keys were lost, the app
/// does not paste there).
const CAP: Duration = Duration::from_secs(8);

/// A paste whose restore is pending.
struct Transaction {
  id: u64,
  offer: Offer,
  snapshot: Snapshot,
  posted: Instant,
  read: Option<Instant>,
}

impl Transaction {
  /// How long until the restore may happen, once the text was read.
  fn wait(&self) -> Option<Duration> {
    let read = self.read?;
    let after_read = QUIET.saturating_sub(read.elapsed());
    let after_post = MIN_HOLD.saturating_sub(self.posted.elapsed());
    Some(after_read.max(after_post))
  }
}

thread_local! {
  static PENDING: RefCell<Option<Transaction>> = const { RefCell::new(None) };
  static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Pastes `text` into the app in focus: clipboard, then ⌘V. `commit` is asked before the first
/// change (when it says no, nothing has changed), `last_look` right before it (where the text
/// goes, once more). The checks of where the text goes are the caller's.
pub fn paste(
  text: &str,
  keep: bool,
  commit: impl FnOnce() -> bool,
  last_look: impl FnOnce() -> Result<(), NotPasted>,
) -> Result<(), NotPasted> {
  let started = Instant::now();
  let clipboard = SystemClipboard;
  let unreachable = || NotPasted::Failed("the clipboard can't be reached".into());
  let readable = clipboard.readable();
  if !readable && !keep {
    warn!("paste: Wavo may not read the clipboard without asking: not restored");
  }

  // What comes back afterwards: the user's clipboard, also when the last paste still holds it.
  // Taking it may take long (lazy data of another app, or macOS asking the user): what the user
  // copies meanwhile wins; twice, and nothing is pasted. Where the OS keeps no change count
  // (Linux) such a change goes unseen.
  let (mut snapshot, mut count) = (None, None);
  if !keep && readable {
    for _ in 0..2 {
      let now = clipboard.change_count();
      let previous = PENDING.with_borrow(|p| {
        p.as_ref().filter(|tx| clipboard.holds(tx.offer)).map(|tx| tx.snapshot.clone())
      });
      let taken = previous.or_else(|| clipboard.snapshot()).ok_or_else(unreachable)?;
      if clipboard.change_count() == now {
        (snapshot, count) = (Some(taken), now);
        break;
      }
    }

    if snapshot.is_none() {
      return Err(NotPasted::Failed("the clipboard kept changing: nothing was pasted".into()));
    }
  }

  // Nothing could be read (the user did not allow it): restoring would empty the clipboard.
  if snapshot.as_ref().is_some_and(Snapshot::unreadable) {
    warn!("paste: the clipboard could not be read: not restored");
    snapshot = None;
  }

  if !commit() {
    return Err(NotPasted::Failed("the app was too busy to paste in time".into()));
  }

  last_look()?;
  if snapshot.is_some() && clipboard.change_count() != count {
    return Err(NotPasted::Failed("the clipboard changed just now: nothing was pasted".into()));
  }

  let id = NEXT_ID.replace(NEXT_ID.get() + 1);
  let pending = match snapshot {
    Some(snapshot) => {
      let offer = clipboard.offer(text, Some(Box::new(move || read(id))));
      Some((snapshot, offer.ok_or_else(unreachable)?))
    }
    None if keep => {
      SystemClipboard.write(text);
      None
    }
    // No snapshot to restore (reading is not allowed): the text stays, marked.
    None => {
      clipboard.offer(text, None).ok_or_else(unreachable)?;
      None
    }
  };

  // The previous paste is over: its snapshot, if still due, was taken over.
  PENDING.take();
  let sent = nimbo::keys::post(Shortcut { key: Key::Char('v'), mods: Modifiers::META });
  let posted = Instant::now();
  match pending {
    Some((snapshot, offer)) => {
      debug!("paste: snapshot of the clipboard: {}", summary(&snapshot));
      PENDING.set(Some(Transaction { id, offer, snapshot, posted, read: None }));
      proxy().after(CAP, move |_| settle(id));
    }
    None => debug!("paste: the text stays on the clipboard"),
  }

  if let Err(e) = sent {
    // Nothing reads the text: the clipboard comes back at once.
    settle(id);
    return Err(NotPasted::Failed(format!("⌘V was not sent: {e}")));
  }

  info!("paste: ⌘V, {} chars, {:.1?}", text.chars().count(), started.elapsed());
  Ok(())
}

/// The text was read: the restore follows (see [`Transaction::wait`]).
fn read(id: u64) {
  let first = PENDING.with(|p| {
    let mut p = p.try_borrow_mut().ok()?;
    let tx = p.as_mut().filter(|tx| tx.id == id)?;
    let first = tx.read.is_none().then(|| tx.posted.elapsed());
    tx.read = Some(Instant::now());
    Some(first)
  });
  let Some(first) = first else { return };
  if let Some(after) = first {
    debug!("paste: the text was read {after:.1?} after ⌘V");
  }

  restore_when_due(id);
}

/// Restores when [`Transaction::wait`] says so, checking again until then.
fn restore_when_due(id: u64) {
  let wait = PENDING.with(|p| {
    let p = p.try_borrow().ok()?;
    p.as_ref().filter(|tx| tx.id == id)?.wait()
  });
  match wait {
    Some(Duration::ZERO) => settle(id),
    Some(wait) => proxy().after(wait, move |_| restore_when_due(id)),
    None => {}
  }
}

/// Ends paste `id`, unless that happened already: the snapshot comes back if the clipboard still
/// holds the paste's text.
fn settle(id: u64) {
  let tx = PENDING.with(|p| p.try_borrow_mut().ok().and_then(|mut p| p.take_if(|tx| tx.id == id)));
  if let Some(tx) = tx {
    tx.finish();
  }
}

/// Restores the clipboard now if a paste still holds it (the app quits).
pub fn finish() {
  if let Some(id) = PENDING.with_borrow(|p| p.as_ref().map(|tx| tx.id)) {
    settle(id);
  }
}

impl Transaction {
  fn finish(self) {
    let clipboard = SystemClipboard;
    let held = self.posted.elapsed();
    let unread = if self.read.is_none() { "; the app never read the text" } else { "" };
    let summary = summary(&self.snapshot);
    if !clipboard.holds(self.offer) {
      info!("paste: the clipboard changed since ⌘V: left as it is ({held:.0?}{unread})");
    } else if clipboard.restore(self.snapshot) {
      info!("paste: clipboard restored {held:.0?} after ⌘V ({summary}{unread})");
    } else {
      warn!("paste: the clipboard could not be restored ({summary})");
    }
  }
}

fn summary(snapshot: &Snapshot) -> String {
  let missing = match snapshot.missing {
    0 => String::new(),
    n => format!(", {n} types unreadable"),
  };

  format!("{} items, {} bytes{missing}", snapshot.items.len(), snapshot.bytes())
}
