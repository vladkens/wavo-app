// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Where the person types, through the accessibility API: the frontmost app and its focused
//! window when a recording starts; at paste time, whether both are still the same (NSWorkspace's
//! frontmost app and, asked live, the app with the keyboard focus, `AXFocusedApplication`) and
//! whether the focused element is a password field. Main thread (NSWorkspace); every AX request
//! waits at most [`AX_TIMEOUT`].

use std::ptr::NonNull;

use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFRetained, CFString, CFType};
use tracing::{debug, info};

use crate::session::output::NotPasted;

/// How long an AX request to the app may take: a hung app does not block the main thread longer.
const AX_TIMEOUT: f32 = 0.25;

/// An accessibility element, compared with `CFEqual`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element(CFRetained<AXUIElement>);

// SAFETY: CF objects are retained and released atomically; the element is only used on the main
// thread, it only travels with the target through the session thread.
unsafe impl Send for Element {}

/// The app the person typed in when the recording started.
#[derive(Debug, Clone)]
pub struct Focus {
  pub pid: i32,
  pub app: String,
  window: Window,
}

#[derive(Debug, Clone)]
enum Window {
  /// The focused window (none: the app has no window in focus).
  Known(Option<Element>),
  /// The app did not say (no AX support, busy): only the pid is compared.
  Unknown(AXError),
}

impl Focus {
  /// The frontmost app and its focused window, now.
  pub fn current() -> Option<Focus> {
    let (pid, app) = frontmost()?;
    let window = match attribute(&application(pid), "AXFocusedWindow") {
      Ok(window) => Window::Known(window.and_then(element)),
      Err(e) => {
        debug!("{app} ({pid}): no AXFocusedWindow (AXError {})", e.0);
        Window::Unknown(e)
      }
    };

    Some(Focus { pid, app, window })
  }

  /// Pasting is still safe: the same app and window are in focus, and the focused element is not
  /// a password field.
  pub fn check(&self) -> Result<(), NotPasted> {
    let (pid, app) =
      frontmost().ok_or_else(|| NotPasted::Failed("there is no frontmost app".into()))?;
    if pid != self.pid {
      info!("not pasted: the focus moved from {} ({}) to {app} ({pid})", self.app, self.pid);
      return Err(NotPasted::FocusChanged);
    }

    self.still_in_focus()?;
    let ax = application(pid);
    let known = match &self.window {
      Window::Known(then) => {
        let now = attribute(&ax, "AXFocusedWindow").map(|w| w.and_then(element)).map_err(|e| {
          NotPasted::Failed(format!("{app} did not tell its focused window (AXError {})", e.0))
        })?;
        if now != *then {
          info!("not pasted: {app} ({pid}) has another window in focus");
          return Err(NotPasted::FocusChanged);
        }

        true
      }
      Window::Unknown(e) => {
        debug!("{app} ({pid}): the window at the start is unknown (AXError {}); pid only", e.0);
        false
      }
    };

    match secure_field(&ax) {
      Ok(true) => {
        info!("not pasted: a password field has the focus in {app} ({pid})");
        Err(NotPasted::SecureField)
      }
      Ok(false) => Ok(()),
      // An app that answers AX at all must answer this too; one that never did cannot be checked
      // — unless Secure Input is on: then it may well be a password field.
      Err(e) if known || nimbo::permissions::secure_input() => {
        Err(NotPasted::Failed(format!("{app} did not tell its focused element (AXError {})", e.0)))
      }
      Err(_) => Ok(()),
    }
  }

  /// The app with the keyboard focus, asked live (NSWorkspace's frontmost app is updated by the
  /// run loop), is still the one of the start; unknown counts as yes.
  pub fn still_in_focus(&self) -> Result<(), NotPasted> {
    match focused_app() {
      Some(pid) if pid != self.pid => {
        info!("not pasted: pid {pid} has the keyboard focus, not {} ({})", self.app, self.pid);
        Err(NotPasted::FocusChanged)
      }
      _ => Ok(()),
    }
  }
}

/// The frontmost app: pid and name.
fn frontmost() -> Option<(i32, String)> {
  let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
  let name = app.localizedName().map_or_else(String::new, |n| n.to_string());
  Some((app.processIdentifier(), name))
}

/// The pid of the app with the keyboard focus (`AXFocusedApplication`).
fn focused_app() -> Option<i32> {
  // SAFETY: no arguments.
  let system = unsafe { AXUIElement::new_system_wide() };
  // SAFETY: plain setter on a valid element.
  unsafe { system.set_messaging_timeout(AX_TIMEOUT) };
  let app = attribute(&system, "AXFocusedApplication").ok().flatten().and_then(element)?;
  let mut pid = 0;
  // SAFETY: `pid` receives the element's pid.
  (unsafe { app.0.pid(NonNull::from(&mut pid)) } == AXError::Success).then_some(pid)
}

fn application(pid: i32) -> CFRetained<AXUIElement> {
  // SAFETY: any pid is accepted; a dead one answers nothing.
  let app = unsafe { AXUIElement::new_application(pid) };
  // SAFETY: plain setter on a valid element.
  unsafe { app.set_messaging_timeout(AX_TIMEOUT) };
  app
}

/// An attribute's value; `None` when the element has none (`kAXErrorNoValue`).
fn attribute(element: &AXUIElement, name: &str) -> Result<Option<CFRetained<CFType>>, AXError> {
  let mut value: *const CFType = std::ptr::null();
  // SAFETY: `value` receives a +1 reference or stays null.
  let err =
    unsafe { element.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value)) };
  match err {
    AXError::Success => Ok(NonNull::new(value.cast_mut()).map(|v| {
      // SAFETY: "Copy" returns an owned reference.
      unsafe { CFRetained::from_raw(v) }
    })),
    AXError::NoValue => Ok(None),
    e => Err(e),
  }
}

fn element(value: CFRetained<CFType>) -> Option<Element> {
  value.downcast::<AXUIElement>().ok().map(Element)
}

/// The focused element of the app is a password field (`AXSecureTextField`).
fn secure_field(app: &AXUIElement) -> Result<bool, AXError> {
  let Some(focused) = attribute(app, "AXFocusedUIElement")?.and_then(element) else {
    return Ok(false);
  };

  // The timeout is the element's own, not the app's.
  // SAFETY: plain setter on a valid element.
  unsafe { focused.0.set_messaging_timeout(AX_TIMEOUT) };
  let subrole = match attribute(&focused.0, "AXSubrole") {
    Ok(subrole) => subrole,
    Err(AXError::AttributeUnsupported) => None,
    Err(e) => return Err(e),
  };

  let subrole = subrole.and_then(|s| s.downcast::<CFString>().ok());
  Ok(subrole.is_some_and(|s| s.to_string() == "AXSecureTextField"))
}
