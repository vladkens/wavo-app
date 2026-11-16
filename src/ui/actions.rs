// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A button as one call: its id, its label or icon, its tip and whether it is disabled, answering
//! whether it was clicked. The idle, disabled and hovered look is nimbo's; the caller never
//! repeats the `disabled(...).id(...).button(...).clicked` chain. A tip left pending by
//! [Ui::help] before the call lands on the button, as nimbo places it.

use std::hash::Hash;

use nimbo::{ControlSize, Icon, Ui};

/// The buttons over [Ui].
pub trait Actions {
  /// A button `id` labelled `label`, disabled while `disabled`: clicked this frame.
  fn button_action(&mut self, id: impl Hash, label: &str, disabled: bool) -> bool;

  /// An icon button `id` with `tip` on hover, disabled while `disabled`: clicked this frame.
  fn icon_action(&mut self, id: impl Hash, icon: Icon, tip: &str, disabled: bool) -> bool;

  /// The same as [Actions::icon_action], as large as a list row's buttons: Play and Copy in
  /// History, ↗ in About.
  fn list_icon_action(&mut self, id: impl Hash, icon: Icon, tip: &str, disabled: bool) -> bool;
}

impl Actions for Ui<'_> {
  fn button_action(&mut self, id: impl Hash, label: &str, disabled: bool) -> bool {
    // Only ever turned off, never cleared: a container's [Ui::disabled] still reaches it.
    if disabled {
      self.disabled(true);
    }

    self.id(id).button(label).clicked
  }

  fn icon_action(&mut self, id: impl Hash, icon: Icon, tip: &str, disabled: bool) -> bool {
    if disabled {
      self.disabled(true);
    }

    self.help(tip).id(id).icon_button(icon).clicked
  }

  fn list_icon_action(&mut self, id: impl Hash, icon: Icon, tip: &str, disabled: bool) -> bool {
    if disabled {
      self.disabled(true);
    }

    let ui = self.help(tip).control_size(ControlSize::Large);
    ui.id(id).icon_button(icon).clicked
  }
}
