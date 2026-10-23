// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Where dictated text goes: [`TextOutput`], which the platform shells implement; the headless
//! app's [`Stdout`] and the tests' [`FakeOutput`].

use std::io::{self, Write};

/// Delivers dictated text to the app the person is typing in.
pub trait TextOutput: Send + 'static {
  /// Where the person is typing: the frontmost app and its focused window.
  type Target: Send + 'static;

  /// Taken when a recording starts.
  fn target(&mut self) -> Self::Target;

  /// Pastes `text` into `target`, unless the focus has moved since or pasting there is unsafe.
  /// The clipboard is restored afterwards unless `keep_in_clipboard`: then the text stays on it.
  fn paste(
    &mut self,
    text: &str,
    target: &Self::Target,
    keep_in_clipboard: bool,
  ) -> Result<(), NotPasted>;

  /// Puts `text` on the clipboard.
  fn copy(&mut self, text: &str) -> anyhow::Result<()>;
}

/// Why a text was not pasted. It is in the history either way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotPasted {
  #[error("the focus moved to another window during the dictation")]
  FocusChanged,
  #[error("the focused field is a password field")]
  SecureField,
  #[error("{0}")]
  Failed(String),
}

/// Output of the headless app: each text is a line on stdout; nothing is pasted anywhere.
pub struct Stdout;

impl Stdout {
  fn line(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{text}")?;
    out.flush()
  }
}

impl TextOutput for Stdout {
  type Target = ();

  fn target(&mut self) {}

  fn paste(&mut self, text: &str, _: &(), _: bool) -> Result<(), NotPasted> {
    Self::line(text).map_err(|e| NotPasted::Failed(format!("stdout: {e}")))
  }

  fn copy(&mut self, text: &str) -> anyhow::Result<()> {
    Ok(Self::line(text)?)
  }
}

/// Records what it is asked to deliver; tests set `refuse` to act like a failed safety check.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeOutput(pub std::sync::Arc<std::sync::Mutex<FakeLog>>);

#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeLog {
  /// Targets taken so far; a target is its number.
  pub targets: u32,
  /// Taking a target waits for a message on it (a slow focus check).
  pub target_gate: Option<crossbeam_channel::Receiver<()>>,
  /// `(text, target, keep_in_clipboard)` of each paste.
  pub pasted: Vec<(String, u32, bool)>,
  pub copied: Vec<String>,
  pub refuse: Option<NotPasted>,
}

#[cfg(test)]
impl FakeOutput {
  pub fn log(&self) -> std::sync::MutexGuard<'_, FakeLog> {
    self.0.lock().unwrap()
  }
}

#[cfg(test)]
impl TextOutput for FakeOutput {
  type Target = u32;

  fn target(&mut self) -> u32 {
    let gate = self.log().target_gate.clone();
    if let Some(gate) = gate {
      let _ = gate.recv();
    }

    let mut log = self.log();
    log.targets += 1;
    log.targets
  }

  fn paste(&mut self, text: &str, target: &u32, keep: bool) -> Result<(), NotPasted> {
    let mut log = self.log();
    if let Some(refused) = log.refuse.clone() {
      return Err(refused);
    }

    log.pasted.push((text.to_owned(), *target, keep));
    Ok(())
  }

  fn copy(&mut self, text: &str) -> anyhow::Result<()> {
    self.log().copied.push(text.to_owned());
    Ok(())
  }
}
