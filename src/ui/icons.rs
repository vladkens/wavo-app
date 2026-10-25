// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar icon, one per state, from the SVGs in `assets/status-icon/`: the app icon's
//! wave-"w", big and centred in an 18 × 18 view box, a state's mark (a dot, three dots, a dot with
//! "!" cut out) as big as its top-right corner holds, laid over it with the "w" cut away around it.
//! A template: the OS draws it at every scale.

use nimbo::tray::TrayIcon;

/// As `wavo-app ctl status` calls it: `idle`, `recording`…
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Icon {
  Idle,
  Recording,
  Transcribing,
  Warning,
}

impl Icon {
  fn svg(self) -> &'static str {
    match self {
      Icon::Idle => include_str!("../../assets/status-icon/idle.svg"),
      Icon::Recording => include_str!("../../assets/status-icon/recording.svg"),
      Icon::Transcribing => include_str!("../../assets/status-icon/transcribing.svg"),
      Icon::Warning => include_str!("../../assets/status-icon/warning.svg"),
    }
  }

  /// The tooltip, also what accessibility says of it.
  pub fn description(self) -> &'static str {
    match self {
      Icon::Idle => "Wavo",
      Icon::Recording => "Wavo — recording",
      Icon::Transcribing => "Wavo — transcribing",
      Icon::Warning => "Wavo — needs attention",
    }
  }

  pub fn tray(self) -> TrayIcon {
    TrayIcon::from_svg(self.svg()).expect("the status icons are tested")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_icon_is_an_18_point_even_odd_path() {
    for icon in [Icon::Idle, Icon::Recording, Icon::Transcribing, Icon::Warning] {
      let TrayIcon::Path { size, d, even_odd } = icon.tray() else { unreachable!() };
      assert!(size == 18.0 && even_odd && !d.is_empty(), "{icon:?}");
      assert!(icon.description().starts_with("Wavo"), "{icon:?}");
    }
  }
}
