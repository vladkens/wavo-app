// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Where the person types: nimbo's [focus] when a recording starts, and at paste time whether
//! pasting there is still safe: the same app and window in focus, and no password field.

use nimbo::focus::{self, Focused};
use tracing::{debug, info};

use crate::session::output::NotPasted;

/// The app in focus now, for the paste to check against.
pub fn current() -> Option<Focused> {
  let now = focus::current()?;
  if let Err(e) = &now.window {
    debug!("{} ({}): no focused window ({e})", now.name, now.pid);
  }

  Some(now)
}

/// Pasting is still safe: `then` (the focus when the recording started) has the focus, and the
/// focused element is not a password field.
pub fn check(then: &Focused) -> Result<(), NotPasted> {
  let now =
    focus::current().ok_or_else(|| NotPasted::Failed("there is no frontmost app".into()))?;
  if then.moved(&now) {
    let to = if now.pid == then.pid { "another window" } else { &now.name };
    info!("not pasted: the focus moved from {} ({}) to {to} ({})", then.name, then.pid, now.pid);
    return Err(NotPasted::FocusChanged);
  }

  // The app told its window at the start: it must tell it now too.
  let known = then.window.is_ok();
  if known && let Err(e) = &now.window {
    return Err(NotPasted::Failed(format!("{} did not tell its focused window ({e})", now.name)));
  }

  match &now.secure {
    Ok(true) => {
      info!("not pasted: a password field has the focus in {} ({})", now.name, now.pid);
      Err(NotPasted::SecureField)
    }
    Ok(false) => Ok(()),
    // An app that answers at all must answer this too; one that never did cannot be checked —
    // unless Secure Input is on: then it may well be a password field.
    Err(e) if known || nimbo::permissions::secure_input() => {
      Err(NotPasted::Failed(format!("{} did not tell its focused element ({e})", now.name)))
    }
    Err(_) => Ok(()),
  }
}
