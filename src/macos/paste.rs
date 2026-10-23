// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Paste on the main thread: the text goes on the general pasteboard as a lazy promise, ⌘V goes to
//! the frontmost app, and the previous contents come back once the app has read the text.
//!
//! - `declareTypes:owner:` declares plain text plus `org.nspasteboard.TransientType` and
//!   `ConcealedType` (clipboard managers skip such contents) with [`Owner`] as the provider. The
//!   text is handed out only when someone reads it, in `pasteboard:provideDataForType:`: that call
//!   says when it was read. The pasteboard server asks once and serves later reads from its cache.
//! - Before that a [`Snapshot`] of every item and type on the pasteboard is taken. It is put back
//!   [`QUIET`] after the read of the text, but not before [`MIN_HOLD`] after ⌘V (an early reader
//!   that is not the target must not cut the target short), or [`CAP`] after ⌘V without a read; and
//!   only while the pasteboard's `changeCount` is still the paste's: what the user copied meanwhile
//!   stays.
//! - A paste while the previous one waits for its restore takes over that one's snapshot: the
//!   user's clipboard comes back, not the previous text.
//! - "Keep the text on the clipboard": the text is written at once as plain text, without the
//!   markers (clipboard managers may keep it, it outlives the app), and nothing is restored. When
//!   the app may not read the clipboard without asking ([`Access`]: macOS 15.4+, System Settings →
//!   Privacy & Security → Paste from Other Apps), or nothing of it could be read, the text is
//!   written at once too, with the markers: there is no snapshot to restore.
//! - ⌘V: nimbo's [posted keys](nimbo::keys::post), the key that types "v" with Command in the
//!   current layout, with the Command flag only (a held Option does not make it ⌥⌘V).
//! - Owners are released a few pastes later, not when their promise goes: AppKit may still tell one
//!   that it lost the pasteboard after that.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use nimbo::{Clipboard, Key, Modifiers, Shortcut, SystemClipboard};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
  NSPasteboard, NSPasteboardAccessBehavior, NSPasteboardItem, NSPasteboardTypeString,
  NSPasteboardWriting,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSData, NSObject, NSObjectProtocol, NSString};
use tracing::{debug, info, warn};

use super::proxy;
use crate::session::output::NotPasted;

/// The restore waits this long after the read: some apps read more than once per paste.
const QUIET: Duration = Duration::from_millis(200);
/// The text stays at least this long after ⌘V.
const MIN_HOLD: Duration = Duration::from_millis(500);
/// The longest the text stays on the pasteboard when nobody reads it (the keys were lost, the app
/// does not paste there).
const CAP: Duration = Duration::from_secs(8);
/// Released owners kept alive (see the module docs).
const RETIRED: usize = 4;
const TRANSIENT: &str = "org.nspasteboard.TransientType";
const CONCEALED: &str = "org.nspasteboard.ConcealedType";

/// The paste may read the general pasteboard to restore it: always before macOS 15.4, then unless
/// Paste from Other Apps (System Settings → Privacy & Security, `NSPasteboard.accessBehavior`,
/// which reading asks nothing) has macOS ask before every read or deny it. Never asked, the first
/// read makes macOS ask once and list Wavo there, where the person allows it for good.
pub fn readable() -> bool {
  may_read(&NSPasteboard::generalPasteboard())
}

fn may_read(pasteboard: &NSPasteboard) -> bool {
  use NSPasteboardAccessBehavior as Behavior;

  !pasteboard.respondsToSelector(sel!(accessBehavior))
    || !matches!(pasteboard.accessBehavior(), Behavior::Ask | Behavior::AlwaysDeny)
}

/// Every item of a pasteboard with the data of each of its types.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
  items: Vec<Vec<(Retained<NSString>, Retained<NSData>)>>,
  /// Types whose data could not be read.
  missing: usize,
}

impl Snapshot {
  pub fn take(pasteboard: &NSPasteboard) -> Self {
    let mut snapshot = Snapshot::default();
    let Some(items) = pasteboard.pasteboardItems() else { return snapshot };
    for item in items.iter() {
      let mut types = Vec::new();
      for kind in item.types().iter() {
        match item.dataForType(&kind) {
          Some(data) => types.push((kind, data)),
          None => snapshot.missing += 1,
        }
      }

      if !types.is_empty() {
        snapshot.items.push(types);
      }
    }

    snapshot
  }

  /// Replaces the pasteboard's contents with the snapshot's.
  pub fn restore(&self, pasteboard: &NSPasteboard) -> bool {
    pasteboard.clearContents();
    if self.items.is_empty() {
      return true;
    }

    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = self
      .items
      .iter()
      .map(|types| {
        let item = NSPasteboardItem::new();
        for (kind, data) in types {
          item.setData_forType(data, kind);
        }

        ProtocolObject::from_retained(item)
      })
      .collect();
    pasteboard.writeObjects(&NSArray::from_retained_slice(&items))
  }

  pub fn bytes(&self) -> usize {
    self.items.iter().flatten().map(|(_, data)| data.len()).sum()
  }

  /// The pasteboard had contents, but none could be read (macOS did not let Wavo): restoring
  /// this would empty it.
  fn unreadable(&self) -> bool {
    self.items.is_empty() && self.missing > 0
  }

  /// Items, each a list of (type, data).
  #[cfg(test)]
  pub fn contents(&self) -> Vec<Vec<(String, Vec<u8>)>> {
    let item = |types: &Vec<(Retained<NSString>, Retained<NSData>)>| {
      types.iter().map(|(kind, data)| (kind.to_string(), data.to_vec())).collect()
    };
    self.items.iter().map(item).collect()
  }

  fn summary(&self) -> String {
    let missing = match self.missing {
      0 => String::new(),
      n => format!(", {n} types unreadable"),
    };

    format!("{} items, {} bytes{missing}", self.items.len(), self.bytes())
  }
}

/// A paste whose restore is pending.
struct Transaction {
  id: u64,
  change_count: isize,
  snapshot: Snapshot,
  /// Provides the text while the promise is on the pasteboard.
  owner: Retained<Owner>,
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
  static RETIRED_OWNERS: RefCell<VecDeque<Retained<Owner>>> = const { RefCell::new(VecDeque::new()) };
}

struct OwnerIvars {
  id: u64,
  /// Gone once the owner is retired.
  text: RefCell<Option<Retained<NSString>>>,
}

define_class!(
  // SAFETY: NSObject has no subclassing requirements, and `Owner` does not implement `Drop`.
  #[unsafe(super = NSObject)]
  #[thread_kind = MainThreadOnly]
  #[name = "WavoPasteOwner"]
  #[ivars = OwnerIvars]
  struct Owner;

  // SAFETY: no requirements.
  unsafe impl NSObjectProtocol for Owner {}

  impl Owner {
    /// NSPasteboardOwner: someone reads a promised type. Only a read of the text counts; the
    /// markers are handed out empty.
    #[unsafe(method(pasteboard:provideDataForType:))]
    fn provide(&self, pasteboard: &NSPasteboard, kind: &NSString) {
      let ivars = self.ivars();
      // SAFETY: a static of the framework.
      let text = kind.isEqualToString(unsafe { NSPasteboardTypeString });
      let data = ivars.text.borrow().as_ref().filter(|_| text).cloned();
      pasteboard.setString_forType(&data.unwrap_or_else(NSString::new), kind);
      if text {
        read(ivars.id);
      }
    }

    /// Someone else wrote to the pasteboard (or the restore did): the promise is gone.
    #[unsafe(method(pasteboardChangedOwner:))]
    fn changed_owner(&self, _pasteboard: &NSPasteboard) {
      let id = self.ivars().id;
      // Not from inside the owner's own call.
      proxy().after(Duration::ZERO, move |_| settle(id));
    }
  }
);

impl Owner {
  fn new(mtm: MainThreadMarker, id: u64, text: &str) -> Retained<Self> {
    let text = RefCell::new(Some(NSString::from_str(text)));
    let this = Self::alloc(mtm).set_ivars(OwnerIvars { id, text });
    // SAFETY: NSObject's `init`.
    unsafe { msg_send![super(this), init] }
  }

  /// Its promise is gone: the text goes now, the object a few pastes later.
  fn retire(this: Retained<Self>) {
    this.ivars().text.borrow_mut().take();
    RETIRED_OWNERS.with_borrow_mut(|retired| {
      retired.push_back(this);
      while retired.len() > RETIRED {
        retired.pop_front();
      }
    });
  }
}

/// Pastes `text` into the frontmost app: pasteboard, then ⌘V. `commit` is asked before the first
/// change (when it says no, nothing has changed), `last_look` right before it (where the text
/// goes, once more). The checks of where the text goes are the caller's.
pub fn paste(
  mtm: MainThreadMarker,
  text: &str,
  keep: bool,
  commit: impl FnOnce() -> bool,
  last_look: impl FnOnce() -> Result<(), NotPasted>,
) -> Result<(), NotPasted> {
  let started = Instant::now();
  let pasteboard = NSPasteboard::generalPasteboard();
  let readable = may_read(&pasteboard);
  if !readable && !keep {
    warn!("paste: Wavo may not read the clipboard without asking: not restored");
  }

  // What comes back afterwards: the user's clipboard, also when the last paste still holds it.
  // Taking it may take long (lazy data of another app, or macOS asking the user): what the user
  // copies meanwhile wins; twice, and nothing is pasted.
  let (mut snapshot, mut count) = (None, None);
  if !keep && readable {
    for _ in 0..2 {
      let now = pasteboard.changeCount();
      let previous = PENDING.with_borrow(|p| {
        p.as_ref().filter(|tx| tx.change_count == now).map(|tx| tx.snapshot.clone())
      });
      let taken = previous.unwrap_or_else(|| Snapshot::take(&pasteboard));
      if pasteboard.changeCount() == now {
        (snapshot, count) = (Some(taken), Some(now));
        break;
      }
    }

    if count.is_none() {
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
  if let Some(count) = count
    && pasteboard.changeCount() != count
  {
    return Err(NotPasted::Failed("the clipboard changed just now: nothing was pasted".into()));
  }

  // The previous paste is over (its snapshot, if still due, is taken over); its owner is retired
  // once the new contents replaced its promise.
  let previous = PENDING.take();
  let id = NEXT_ID.replace(NEXT_ID.get() + 1);
  // SAFETY: a static of the framework.
  let string = unsafe { NSPasteboardTypeString };
  let markers = [NSString::from_str(TRANSIENT), NSString::from_str(CONCEALED)];
  let types =
    NSArray::from_retained_slice(&[string.retain(), markers[0].clone(), markers[1].clone()]);
  let promise = match snapshot {
    Some(snapshot) => {
      let owner = Owner::new(mtm, id, text);
      let owner_object: &AnyObject = &owner;
      // SAFETY: the owner implements `pasteboard:provideDataForType:` for every declared type and
      // stays alive while its promise is on the pasteboard (and some time after).
      let change_count = unsafe { pasteboard.declareTypes_owner(&types, Some(owner_object)) };
      Some((snapshot, owner, change_count))
    }
    None if keep => {
      SystemClipboard.write(text);
      None
    }
    // No snapshot to restore (reading is not allowed): the text stays, marked.
    None => {
      // SAFETY: no owner: the data of every declared type is set right below.
      unsafe { pasteboard.declareTypes_owner(&types, None) };
      for marker in &markers {
        pasteboard.setData_forType(Some(&NSData::new()), marker);
      }

      pasteboard.setString_forType(&NSString::from_str(text), string);
      None
    }
  };

  if let Some(previous) = previous {
    Owner::retire(previous.owner);
  }

  let sent = nimbo::keys::post(Shortcut { key: Key::Char('v'), mods: Modifiers::META });
  let posted = Instant::now();
  match promise {
    Some((snapshot, owner, change_count)) => {
      debug!("paste: snapshot of the clipboard: {}", snapshot.summary());
      PENDING.set(Some(Transaction { id, change_count, snapshot, owner, posted, read: None }));
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

/// Ends paste `id`, unless that happened already: the snapshot comes back if the pasteboard is
/// still the paste's.
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
    let pasteboard = NSPasteboard::generalPasteboard();
    let held = self.posted.elapsed();
    let unread = if self.read.is_none() { "; the app never read the text" } else { "" };
    if pasteboard.changeCount() != self.change_count {
      info!("paste: the clipboard changed since ⌘V: left as it is ({held:.0?}{unread})");
    } else if self.snapshot.restore(&pasteboard) {
      info!("paste: clipboard restored {held:.0?} after ⌘V ({}{unread})", self.snapshot.summary());
    } else {
      warn!("paste: the clipboard could not be restored ({})", self.snapshot.summary());
    }

    Owner::retire(self.owner);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// On a pasteboard of its own: the user's clipboard is never touched.
  #[test]
  fn a_snapshot_restores_every_item_and_type() {
    let pasteboard = NSPasteboard::pasteboardWithUniqueName();
    let item = |types: &[(&str, &[u8])]| {
      let item = NSPasteboardItem::new();
      for (kind, data) in types {
        item.setData_forType(&NSData::with_bytes(data), &NSString::from_str(kind));
      }

      ProtocolObject::<dyn NSPasteboardWriting>::from_retained(item)
    };
    let png = [0x89, b'P', b'N', b'G', 0, 1, 2, 255];
    let items = [
      item(&[("public.utf8-plain-text", "текст ✓".as_bytes()), ("public.html", b"<b>x</b>")]),
      item(&[("public.png", &png), ("com.example.private", &[])]),
    ];
    pasteboard.clearContents();
    assert!(pasteboard.writeObjects(&NSArray::from_retained_slice(&items)));
    let snapshot = Snapshot::take(&pasteboard);
    let contents = snapshot.contents();
    assert_eq!(contents.len(), 2, "{contents:?}");
    assert_eq!(snapshot.bytes(), "текст ✓".len() + 8 + png.len());
    assert!(may_read(&pasteboard), "a pasteboard of its own may be read");

    // SAFETY: a static of the framework.
    let string = unsafe { NSPasteboardTypeString };
    pasteboard.clearContents();
    pasteboard.setString_forType(&NSString::from_str("other"), string);
    assert_ne!(Snapshot::take(&pasteboard).contents(), contents);
    assert!(snapshot.restore(&pasteboard));
    assert_eq!(Snapshot::take(&pasteboard).contents(), contents, "byte for byte");

    // An empty pasteboard comes back empty.
    pasteboard.clearContents();
    let empty = Snapshot::take(&pasteboard);
    pasteboard.setString_forType(&NSString::from_str("other"), string);
    assert!(empty.restore(&pasteboard));
    assert!(Snapshot::take(&pasteboard).contents().is_empty());
    // SAFETY: `releaseGlobally` takes no arguments; the pasteboard is not used afterwards.
    let _: () = unsafe { msg_send![&pasteboard, releaseGlobally] };
  }
}
