// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Updates in place with nimbo's [update]: a signed feed beside each release on GitHub, checked at
//! launch and once a day while automatic checks are on, and when the person asks. What the person
//! sees is nimbo's [dialog]: one menu item, and the window it opens. Nothing opens by itself; where
//! nimbo says the app can't update (a build, a .deb's, a folder the person can't write, a dev
//! build), none of it shows.
//!
//! [update]: nimbo::update
//! [dialog]: nimbo::update::dialog

use std::io;
use std::time::Duration;

use nimbo::update::{Feed, Release};

/// `update.txt` beside the latest release's files, which CI's publish job writes and signs (nimbo's
/// `pack.py release --tag`); `latest` is never the dev pre-release.
pub const FEED: Feed = Feed {
  url: "https://github.com/vladkens/wavo-app/releases/latest/download/update.txt",
  key: "c56b43f703ba8af3ad226604a5f5030b51b07b03b00dc116116bc48a4a86bd1a",
};

const RELEASES: &str = "https://github.com/vladkens/wavo-app/releases";

/// The release's page, with its notes.
pub fn notes_url(version: &str) -> String {
  format!("{RELEASES}/tag/v{}", version.trim_start_matches('v'))
}

// MARK: Fake

/// `WAVO_FAKE_UPDATE=<version>` (`none`: nothing newer), to see the updater on this computer: it
/// shows in any build, its check offers that version with no network, its download only counts
/// and its install fails, nothing replaced.
pub fn fake() -> Option<String> {
  std::env::var("WAVO_FAKE_UPDATE").ok().filter(|v| !v.is_empty())
}

/// What the fake check finds.
pub fn fake_check(version: &str) -> io::Result<Option<Release>> {
  Ok((version != "none").then(|| Release::test(version)))
}

/// The fake download: `progress` counts to 100 in about 5 s.
pub fn fake_download(mut progress: impl FnMut(u64, u64)) {
  for done in (0..=100).step_by(4) {
    std::thread::sleep(Duration::from_millis(200));
    progress(done, 100);
  }
}

/// The fake install's failure, after Restarting… shows a while.
pub const FAKE_INSTALL: &str = "WAVO_FAKE_UPDATE: nothing is installed";

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_notes_are_the_releases_page() {
    let page = format!("{RELEASES}/tag/v1.2.0");
    assert_eq!((notes_url("1.2.0"), notes_url("v1.2.0")), (page.clone(), page));
  }

  #[test]
  fn the_fake_offers_its_version_or_none() {
    assert_eq!(fake_check("1.2.0").unwrap(), Some(Release::test("1.2.0")));
    assert_eq!(fake_check("none").unwrap(), None);
  }
}
