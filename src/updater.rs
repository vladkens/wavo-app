// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Updates in place with [shipo]: a signed feed beside each release on GitHub, checked at launch
//! and once a day while automatic checks are on, and when the person asks. What the person sees
//! is Sparkle's: a row in the menu says what the [Updater] has, and it and Check for Updates…
//! open its window ([UpdateWindow]), which offers the release, shows the download and the restart,
//! or why it failed. Nothing opens by itself: an update found waits in the menu. Where the app
//! can't update (a build, a .deb's, a folder the person can't write, a [dev] build), none of it
//! shows.
//!
//! The [Updater] is the state without the network: the app runs shipo's check, download and
//! install on threads of its own and brings each result back to it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use nimbo::{Align, Button, Config, Png, Style, TextStyle, Ui, grow, px};
use shipo::{Error, Feed, Release};

/// `update.txt` beside the latest release's files, which CI's publish job writes and signs
/// (`make pack`'s `shipo release --tag`); `latest` is never the dev pre-release. The public keys
/// `shipo keygen` printed: the working one, then the spare.
pub const FEED: Feed = Feed::new(
  "https://github.com/vladkens/wavo-app/releases/latest/download/update.txt",
  &[
    "9ef558b726bcca12c391391bf02432ac13da1e5f9c986fb6077c9600d843b242",
    "14eb5d25ae6eaea5871a01e705cd5b74ef0791fe99ecb2a16556ec20e7aa14b0",
  ],
);

/// An automatic check once a day; one that got no answer is tried again at the next [TICK].
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the app asks whether a check is [Updater::due]: a timer would stop while the Mac
/// sleeps.
pub const TICK: Duration = Duration::from_secs(60 * 60);

/// A dev build's version, a pre-release (`v0.1.0-5-gabc`), which takes no update: shipo's
/// [check](shipo::check) refuses it, and its [supported](shipo::supported) doesn't tell.
pub fn dev(version: &str) -> bool {
  version.contains('-')
}

/// A check's answer as the [Updater] takes it: a missing feed (404) and one whose content or
/// signature is wrong are nothing newer; another error is its text ([why]).
pub fn found(checked: shipo::Result<Option<Release>>) -> Result<Option<Release>, String> {
  checked.or_else(|e| match e {
    Error::Http { status: Some(404), .. } | Error::Feed(_) | Error::Signature(_) => Ok(None),
    _ => Err(why(&e)),
  })
}

/// An error's text as the window shows it: a request's without its URL, which the log has.
pub fn why(e: &Error) -> String {
  match e {
    Error::Http { message, .. } => message.clone(),
    _ => e.to_string(),
  }
}

/// `v1.2.0`, as the app shows a version.
pub fn shown(version: &str) -> String {
  format!("v{}", version.trim_start_matches('v'))
}

/// The whole percent of `done` bytes of `total`: a download tells the app only when it changes.
pub fn percent(done: u64, total: u64) -> u8 {
  (done.saturating_mul(100) / total.max(1)).min(100) as u8
}

// MARK: State

/// What the updater shows, in its menu item and its window.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
  /// Nothing: no answer yet, nothing newer, or the newer one put off or skipped.
  Idle,
  /// A check the person asked for runs; an automatic one shows nothing.
  Checking,
  /// The check the person asked for found nothing newer.
  UpToDate,
  Available(Release),
  /// With the whole percent done.
  Downloading(Release, u8),
  /// Downloaded: the app puts it in place once no dictation runs, and the new version starts.
  Restarting(Release),
  /// Why the check, the download or the install failed: with the release when it was its
  /// download or install, which Retry downloads again; else Retry checks again.
  Failed(Option<Release>, String),
  /// Why the update that started this app failed ([shipo::finish]): this one is the old version.
  /// No automatic check runs until the person has seen it; Retry checks again.
  Relaunched(String),
}

impl State {
  /// The release being downloaded or put in place.
  fn busy(&self) -> bool {
    matches!(self, State::Downloading(..) | State::Restarting(_))
  }
}

/// The state, and when to check.
#[derive(Debug)]
pub struct Updater {
  pub state: State,
  /// A check runs: whether the person waits for it in the window.
  checking: Option<bool>,
  /// When a check last got an answer.
  answered: Option<SystemTime>,
  /// The app can't update itself: nothing shows and no check runs.
  off: bool,
}

impl Updater {
  /// `on`: the app updates itself, as decided at launch: shipo's [supported](shipo::supported)
  /// and not a [dev] build.
  pub fn new(on: bool) -> Self {
    Self { state: State::Idle, checking: None, answered: None, off: !on }
  }

  /// What the menu item and the window show, `None` when the app can't update.
  pub fn showing(&self) -> Option<&State> {
    (!self.off).then_some(&self.state)
  }

  /// An automatic check: none answered yet, or the last a day before `now` (or after it, the clock
  /// set back); none while a failed relaunch waits for the person to see it.
  pub fn due(&self, now: SystemTime) -> bool {
    let old = |t: SystemTime| now.duration_since(t).map_or(true, |d| d >= DAY);
    let relaunched = matches!(self.state, State::Relaunched(_));
    !self.off && !relaunched && self.checking.is_none() && self.answered.is_none_or(old)
  }

  /// Whether to start a check: not while one runs, which the person's joins, or while an update is
  /// on its way. The person's shows Checking.
  pub fn check(&mut self, manual: bool) -> bool {
    if self.off || self.state.busy() {
      return false;
    }

    if manual {
      self.state = State::Checking;
    }

    let start = self.checking.is_none();
    self.checking = Some(manual || self.checking == Some(true));
    start
  }

  /// A menu item opened the window: whether to start a check, when there is nothing to show but
  /// how one goes (a failure shows first, Retry checks).
  pub fn open(&mut self) -> bool {
    matches!(self.state, State::Idle | State::UpToDate) && self.check(true)
  }

  /// A check's answer at `now` ([found]): the newer release, none, or why the check failed, which
  /// is tried again at the next [TICK]. A newer release shows unless it is the version `skipped`
  /// and nobody asked; nothing newer takes back an offer the feed no longer makes; nothing newer
  /// and an error show only to the person waiting in the window. A failed download or install
  /// stays until the person sees it.
  pub fn checked(
    &mut self,
    found: Result<Option<Release>, String>,
    skipped: Option<&str>,
    now: SystemTime,
  ) {
    let Some(manual) = self.checking.take() else { return };
    if found.is_ok() {
      self.answered = Some(now);
    }

    let failed = matches!(self.state, State::Failed(Some(_), _));
    if self.state.busy() || failed && !manual {
      return;
    }

    let offered = matches!(self.state, State::Available(_));
    self.state = match found {
      Ok(Some(r)) if manual || skipped != Some(&r.version) => State::Available(r),
      Ok(_) if manual => State::UpToDate,
      Ok(_) if offered => State::Idle,
      Err(e) if manual => State::Failed(None, e),
      _ => return,
    };
  }

  /// Install, or Retry after a failed download or install: the release to download.
  pub fn install(&mut self) -> Option<Release> {
    let (State::Available(r) | State::Failed(Some(r), _)) = &self.state else { return None };
    let r = r.clone();
    self.state = State::Downloading(r.clone(), 0);
    Some(r)
  }

  /// Remind Me Later: the next daily check offers it again.
  pub fn later(&mut self) {
    if matches!(self.state, State::Available(_) | State::Failed(..) | State::Relaunched(_)) {
      self.state = State::Idle;
    }
  }

  /// Skip This Version: the version to save, which automatic checks pass over from then on.
  pub fn skip(&mut self) -> Option<String> {
    let State::Available(r) = &self.state else { return None };
    let version = r.version.clone();
    self.state = State::Idle;
    Some(version)
  }

  /// The download's whole percent ([percent]).
  pub fn progress(&mut self, done: u8) {
    if let State::Downloading(_, at) = &mut self.state {
      *at = done;
    }
  }

  pub fn downloaded(&mut self) {
    if let State::Downloading(r, _) = &self.state {
      self.state = State::Restarting(r.clone());
    }
  }

  /// The download or the install failed, `why`.
  pub fn failed(&mut self, why: String) {
    if let State::Downloading(r, _) | State::Restarting(r) = &self.state {
      self.state = State::Failed(Some(r.clone()), why);
    }
  }

  /// The update that started this app failed, `why` ([shipo::finish]).
  pub fn relaunched(&mut self, why: String) {
    self.state = State::Relaunched(why);
  }

  /// The window closed: what it said about a check or a relaunch is done with, and a check the
  /// person waited for goes on quietly; a release on offer or on its way, or its failure, stays in
  /// the menu.
  pub fn closed(&mut self) {
    if self.checking == Some(true) {
      self.checking = Some(false);
    }

    let said = matches!(self.state, State::Failed(None, _) | State::Relaunched(_));
    if said || matches!(self.state, State::Checking | State::UpToDate) {
      self.state = State::Idle;
    }
  }
}

// MARK: Window

/// The ids of the window's controls.
const INSTALL: &str = "update-install";
const LATER: &str = "update-later";
const SKIP: &str = "update-skip";
const NOTES: &str = "update-notes";
const RETRY: &str = "update-retry";
const CLOSE: &str = "update-close";

/// The app's icon beside the text: 128 px, 64 points at 2x.
const ICON: Png = Png::new(crate::ICON).side(128);

/// What the person chose in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
  Install,
  /// The download again, or the check when it was the check that failed.
  Retry,
  /// Remind Me Later.
  Later,
  /// Skip This Version.
  Skip,
  /// The release's notes, its `notes` page, which the app opens.
  Notes,
  /// OK, Cancel, Hide: the window closes.
  Close,
}

/// The update's window: a fixed size, the app's icon in its title bar off macOS.
pub fn config() -> Config {
  crate::with_icon(Config::new("Software Update", 500.0, 196.0).fixed())
}

/// The update's window over the app's [Updater]: what the person chooses goes to `chosen`.
pub struct UpdateWindow {
  updater: Arc<Mutex<Updater>>,
  chosen: Box<dyn Fn(Act)>,
}

impl UpdateWindow {
  pub fn new(updater: Arc<Mutex<Updater>>, chosen: impl Fn(Act) + 'static) -> Self {
    Self { updater, chosen: Box::new(chosen) }
  }
}

impl nimbo::App for UpdateWindow {
  fn view(&mut self, ui: &mut Ui) {
    let state = self.updater.lock().unwrap().state.clone();
    if let Some(act) = view(ui, &state) {
      (self.chosen)(act);
    }
  }
}

/// The window's content for `state`: the app's icon, what goes on and its buttons, along the
/// bottom as Sparkle's; what the person chose.
fn view(ui: &mut Ui, state: &State) -> Option<Act> {
  let t = ui.theme();
  let current = shown(crate::VERSION);
  let (title, text) = match state {
    State::Idle | State::Checking => ("Checking for updates…", String::new()),
    State::UpToDate => ("You're up to date!", format!("Wavo {current} is the newest version.")),
    State::Available(r) => (
      "A new version of Wavo is available!",
      format!("Wavo {} is out, you have {current}. Install it now?", shown(&r.version)),
    ),
    State::Downloading(..) => ("Downloading the update…", String::new()),
    State::Restarting(r) => {
      ("Installing the update…", format!("Wavo restarts as {} once it can.", shown(&r.version)))
    }
    State::Failed(Some(_), why) | State::Relaunched(why) => ("The update failed", sentence(why)),
    State::Failed(None, why) => ("Couldn't check for updates", sentence(why)),
  };

  let mut act = None;
  let page = Style::new().grow().pads([20.0, 20.0, 16.0, 20.0]).gap(12.0).bg(t.bg);
  ui.col(page, |ui| {
    ui.row(Style::new().w(grow()).gap(16.0), |ui| {
      ui.png_image(ICON, Style::new().w(px(64.0)).h(px(64.0)));
      ui.col(Style::new().w(grow()).gap(6.0), |ui| {
        ui.text(title, TextStyle::new(t.bold, t.text).ellipsis());
        if !text.is_empty() {
          ui.text(&text, t.body());
        }

        match state {
          State::Downloading(r, done) => {
            ui.el(Style::new().h(px(2.0)));
            ui.progress(Some(f32::from(*done) / 100.0));
            if r.size > 0 {
              let mb = |b: u64| format!("{:.1} MB", b as f64 / 1e6);
              let of = format!("{} of {}", mb(r.size * u64::from(*done) / 100), mb(r.size));
              ui.text(&of, t.muted());
            }
          }
          State::Idle | State::Checking | State::Restarting(_) => {
            ui.el(Style::new().h(px(2.0)));
            ui.progress(None);
          }
          State::Available(r) if r.notes.is_some() => {
            if ui.id(NOTES).link("Release notes", t.body().color(t.link)).clicked {
              act = Some(Act::Notes);
            }
          }
          State::Available(_) | State::UpToDate | State::Failed(..) | State::Relaunched(_) => {}
        }
      });
    });

    ui.spacer();
    // Under the text and the icon, the window's width: Sparkle's bar.
    let bar = Style::new().w(grow()).gap(8.0).align(Align::Start, Align::Center);
    ui.row(bar, |ui| {
      let mut button = |ui: &mut Ui, id, label, then, primary| {
        let b = if primary { Button::new(label).primary() } else { Button::new(label) };
        if b.show(ui.id(id)).clicked {
          act = Some(then);
        }
      };

      match state {
        State::Available(_) => {
          button(ui, SKIP, "Skip This Version", Act::Skip, false);
          ui.spacer();
          button(ui, LATER, "Remind Me Later", Act::Later, false);
          button(ui, INSTALL, "Install Update", Act::Install, true);
        }
        State::Failed(..) | State::Relaunched(_) => {
          ui.spacer();
          button(ui, CLOSE, "Close", Act::Close, false);
          button(ui, RETRY, "Retry", Act::Retry, true);
        }
        State::UpToDate => {
          ui.spacer();
          button(ui, CLOSE, "OK", Act::Close, true);
        }
        State::Idle | State::Checking => {
          ui.spacer();
          button(ui, CLOSE, "Cancel", Act::Close, false);
        }
        State::Downloading(..) | State::Restarting(_) => {
          ui.spacer();
          button(ui, CLOSE, "Hide", Act::Close, false);
        }
      }
    });
  });

  act
}

/// An error's text as a sentence: its first letter capital.
fn sentence(s: &str) -> String {
  let mut chars = s.chars();
  chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

// MARK: Fake

const RELEASES: &str = "https://github.com/vladkens/wavo-app/releases";

/// `WAVO_FAKE_UPDATE=<version>` (`none`: nothing newer), to see the updater on this computer: it
/// shows in any build, its check offers that version with no network, its download only counts
/// and its install fails, nothing replaced.
pub fn fake() -> Option<String> {
  std::env::var("WAVO_FAKE_UPDATE").ok().filter(|v| !v.is_empty())
}

/// What the fake check finds: the version with its notes' page and a size, as a feed gives them.
pub fn fake_check(version: &str) -> shipo::Result<Option<Release>> {
  if version == "none" {
    return Ok(None);
  }

  // shipo makes a release only from a feed, or with no file for an app's own tests.
  let mut r = Release::test(version);
  r.notes = Some(format!("{RELEASES}/tag/{}", shown(version)));
  r.size = 30_000_000;
  Ok(Some(r))
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
  use std::cell::RefCell;
  use std::path::PathBuf;
  use std::rc::Rc;

  use nimbo::{Appearance, Event, Harness, Id};

  use super::*;

  const NOW: SystemTime = SystemTime::UNIX_EPOCH;
  /// macOS' text for a request with no network.
  const OFFLINE: &str = "The Internet connection appears to be offline.";

  fn r(v: &str) -> Release {
    Release::test(v)
  }

  fn newer(v: &str) -> Result<Option<Release>, String> {
    Ok(Some(r(v)))
  }

  fn error() -> Result<Option<Release>, String> {
    Err("offline".into())
  }

  /// `u` after an automatic or a manual check answered `found`.
  fn checked(
    u: &mut Updater,
    manual: bool,
    found: Result<Option<Release>, String>,
    skipped: Option<&str>,
  ) {
    assert!(u.check(manual));
    u.checked(found, skipped, NOW);
  }

  #[test]
  fn a_dev_build_takes_no_update() {
    assert!(dev("v0.1.0-5-gabc") && dev("1.2.0-rc.1"));
    assert!(!dev("v0.1.0") && !dev("1.2.0"));
  }

  /// A missing feed and one whose content or signature is wrong are nothing newer; another error
  /// shows its text.
  #[test]
  fn a_missing_or_bad_feed_is_nothing_newer() {
    let url = FEED.url.to_owned();
    let http =
      |status, message: &str| Error::Http { url: url.clone(), status, message: message.into() };
    let quiet = [
      http(Some(404), "HTTP 404"),
      Error::Feed("no version, or a bad one".into()),
      Error::Signature("no key of the app verifies it".into()),
    ];
    for e in quiet {
      let what = e.to_string();
      assert_eq!(found(Err(e)), Ok(None), "{what}");
    }

    let errors = [
      (http(Some(503), "HTTP 503"), "HTTP 503".into()),
      (http(None, OFFLINE), OFFLINE.into()),
      (Error::Keys, "the app has no public key for its feed, or a malformed one".into()),
      (Error::Unsupported("no libcurl".into()), "can't update in place: no libcurl".into()),
    ];
    for (e, text) in errors {
      assert_eq!(found(Err(e)), Err(text));
    }

    assert_eq!(found(Ok(Some(r("1.2.0")))), newer("1.2.0"));
    assert_eq!(found(Ok(None)), Ok(None));
  }

  #[test]
  fn the_feed_has_valid_keys() {
    assert!(FEED.url.starts_with("https://") && !FEED.keys.is_empty());
    for key in FEED.keys {
      assert!(key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
    }

    // Read with no network: valid keys get past `Keys`, and the signature is what fails.
    let read = FEED.read(b"version 1\nsig 00\n", "0.1.0").unwrap_err();
    assert!(matches!(read, Error::Signature(_)), "{read}");
  }

  #[test]
  fn the_percent_is_whole() {
    let got = [(0, 100), (419, 1_000), (999, 1_000), (1_000, 1_000), (5, 0), (u64::MAX, 2)];
    assert_eq!(got.map(|(done, total)| percent(done, total)), [0, 41, 99, 100, 100, 100]);
  }

  #[test]
  fn an_automatic_check_is_due_daily_and_after_a_failure() {
    let mut u = Updater::new(true);
    assert!(u.due(NOW), "at launch");
    assert!(u.check(false));
    assert!(!u.due(NOW) && !u.check(false), "one at a time");
    u.checked(error(), None, NOW);
    assert!(u.due(NOW), "no answer: again at the next tick");

    checked(&mut u, false, Ok(None), None);
    assert!(!u.due(NOW + DAY - Duration::from_secs(1)) && u.due(NOW + DAY));
    assert!(u.due(NOW - Duration::from_secs(1)), "the clock set back");
  }

  /// An app that can't update shows nothing and checks nothing; one that can shows a failed check
  /// the person asked for.
  #[test]
  fn an_app_that_cant_update_shows_nothing() {
    let mut u = Updater::new(false);
    assert_eq!(u.showing(), None);
    assert!(!u.due(NOW) && !u.check(true) && !u.open());

    let mut u = Updater::new(true);
    assert_eq!(u.showing(), Some(&State::Idle));
    checked(&mut u, true, error(), None);
    assert_eq!(u.showing(), Some(&State::Failed(None, "offline".into())));
  }

  #[test]
  fn an_automatic_check_shows_only_what_is_new() {
    let mut u = Updater::new(true);
    checked(&mut u, false, Ok(None), None);
    checked(&mut u, false, error(), None);
    checked(&mut u, false, newer("1.2.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Idle, "nothing new, offline, skipped: quiet");

    checked(&mut u, false, newer("1.3.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Available(r("1.3.0")));
    u.later();
    assert_eq!(u.state, State::Idle);
    checked(&mut u, false, newer("1.3.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Available(r("1.3.0")), "put off until the next check");
    checked(&mut u, false, error(), None);
    assert_eq!(u.state, State::Available(r("1.3.0")), "offline: the offer stays");
    checked(&mut u, false, Ok(None), None);
    assert_eq!(u.state, State::Idle, "the feed took it back");
    checked(&mut u, false, newer("1.3.0"), None);
    assert_eq!(u.skip().as_deref(), Some("1.3.0"));
    assert_eq!((u.state.clone(), u.skip()), (State::Idle, None));
  }

  /// The menu item opens the window with a check when there is nothing else to show; the check
  /// says how it went until the window closes.
  #[test]
  fn opening_the_window_checks_and_says_how_it_went() {
    let mut u = Updater::new(true);
    assert!(u.open());
    assert_eq!(u.state, State::Checking);
    u.checked(Ok(None), None, NOW);
    assert_eq!(u.state, State::UpToDate);
    u.closed();
    assert_eq!(u.state, State::Idle);

    // An error shows, Retry checks again.
    assert!(u.open());
    u.checked(error(), None, NOW);
    assert_eq!(u.state, State::Failed(None, "offline".into()));
    assert!(!u.open(), "the failure shows first");
    assert_eq!(u.install(), None, "nothing to download: Retry checks again");
    assert!(u.check(true));
    u.checked(newer("1.2.0"), Some("1.2.0"), NOW);
    assert_eq!(u.state, State::Available(r("1.2.0")), "asked, a skipped one shows too");
    assert!(!u.open(), "the offer shows as it is");
    u.closed();
    assert_eq!(u.state, State::Available(r("1.2.0")), "the menu keeps it");
  }

  /// The window closed during the person's check: it goes on quietly.
  #[test]
  fn a_check_left_goes_on_quietly() {
    let mut u = Updater::new(true);
    assert!(u.open());
    u.closed();
    assert_eq!(u.state, State::Idle);
    assert!(!u.check(false), "it still runs");
    u.checked(Ok(None), None, NOW);
    assert_eq!(u.state, State::Idle, "nothing newer: quiet");

    assert!(u.open());
    u.closed();
    u.checked(newer("1.2.0"), None, NOW);
    assert_eq!(u.state, State::Available(r("1.2.0")), "a release still shows in the menu");
  }

  #[test]
  fn the_persons_check_joins_an_automatic_one() {
    let mut u = Updater::new(true);
    assert!(u.check(false));
    assert!(!u.open(), "no second thread");
    assert_eq!(u.state, State::Checking);
    u.checked(Ok(None), None, NOW);
    assert_eq!(u.state, State::UpToDate);
  }

  #[test]
  fn an_install_downloads_restarts_or_fails_with_retry() {
    let mut u = Updater::new(true);
    assert_eq!(u.install(), None);
    checked(&mut u, false, newer("1.2.0"), None);
    assert_eq!(u.install(), Some(r("1.2.0")));
    assert_eq!(u.state, State::Downloading(r("1.2.0"), 0));
    assert!(!u.check(true) && !u.check(false) && !u.open(), "no check while it downloads");
    u.progress(percent(41_990, 100_000));
    assert_eq!(u.state, State::Downloading(r("1.2.0"), 41));
    u.closed();
    assert_eq!(u.state, State::Downloading(r("1.2.0"), 41), "hidden, it goes on");

    let e = "1.2.0 is not the file";
    u.failed(e.into());
    let failed = State::Failed(Some(r("1.2.0")), e.into());
    assert_eq!(u.state, failed);
    checked(&mut u, false, newer("1.2.0"), None);
    assert_eq!(u.state, failed, "an automatic check leaves the failure be");
    assert_eq!(u.install(), Some(r("1.2.0")), "Retry");
    u.downloaded();
    assert_eq!(u.state, State::Restarting(r("1.2.0")));
    u.failed(e.into());
    u.later();
    assert_eq!(u.state, State::Idle, "a failure put off");

    // The update that started this app failed: Retry checks again.
    u.relaunched("the new app did not start".into());
    assert_eq!(u.state, State::Relaunched("the new app did not start".into()));
    assert_eq!(u.install(), None);
    u.later();

    // A check answered during the download leaves it be.
    checked(&mut u, false, newer("1.2.0"), None);
    assert!(u.check(false));
    u.install();
    u.checked(newer("1.3.0"), None, NOW);
    assert_eq!(u.state, State::Downloading(r("1.2.0"), 0));
  }

  /// The update that started this app failed: no automatic check runs while the failure shows,
  /// so it stays until the person has seen it; the next tick after offers the release.
  #[test]
  fn a_failed_relaunch_shows_until_seen() {
    let why = "installing the update: the new version didn't start";
    let mut u = Updater::new(true);
    u.relaunched(why.into());
    assert!(!u.due(NOW), "not the launch's check");
    assert!(!u.open(), "the failure shows first");
    assert_eq!(u.state, State::Relaunched(why.into()));
    u.closed();
    assert_eq!(u.state, State::Idle, "seen");
    assert!(u.due(NOW), "the next tick's");
    checked(&mut u, false, newer("1.2.0"), None);
    assert_eq!(u.state, State::Available(r("1.2.0")));
    assert!(!u.due(NOW), "answered");

    // Retry checks again.
    u.relaunched(why.into());
    assert_eq!(u.install(), None);
    assert!(u.check(true));
    u.checked(newer("1.2.0"), None, NOW);
    assert_eq!(u.state, State::Available(r("1.2.0")));
  }

  #[test]
  fn the_fake_offers_its_version_or_none() {
    let r = fake_check("1.2.0").unwrap().unwrap();
    assert_eq!(r.version, "1.2.0");
    assert_eq!(r.notes.as_deref(), Some(format!("{RELEASES}/tag/v1.2.0").as_str()));
    assert_eq!(fake_check("v1.2.0").unwrap().unwrap().notes, r.notes);
    assert_eq!(fake_check("none").unwrap(), None);
  }

  // MARK: Window

  /// The window over `state` in the light look, and what the person chose in it.
  fn window(state: State) -> (Harness<UpdateWindow>, Rc<RefCell<Vec<Act>>>) {
    let mut u = Updater::new(true);
    u.state = state;
    let acts = Rc::new(RefCell::new(Vec::new()));
    let chosen = acts.clone();
    let w = UpdateWindow::new(Arc::new(Mutex::new(u)), move |act| chosen.borrow_mut().push(act));
    let mut h = Harness::new(w, config().size, 2.0);
    h.event(Event::Appearance(Appearance::LIGHT));
    h.render();
    (h, acts)
  }

  /// The fake's release on offer.
  fn available() -> State {
    State::Available(fake_check("1.2.0").unwrap().unwrap())
  }

  #[test]
  fn an_offer_has_its_three_choices_and_the_notes() {
    for (id, act) in
      [(SKIP, Act::Skip), (LATER, Act::Later), (NOTES, Act::Notes), (INSTALL, Act::Install)]
    {
      let (mut h, acts) = window(available());
      assert!(h.click_id(Id::of(id)), "{id}");
      assert_eq!(*acts.borrow(), [act], "{id}");
    }

    // The window draws the shared state: Install's download, once the app starts it.
    let (mut h, _) = window(available());
    h.app_mut().updater.lock().unwrap().install();
    h.render();
    assert!(h.rect(Id::of(INSTALL)).is_none() && h.rect(Id::of(CLOSE)).is_some(), "Hide");
  }

  /// A release without notes is offered without a link to them.
  #[test]
  fn an_offer_without_notes_has_no_link() {
    let (h, _) = window(State::Available(r("1.2.0")));
    assert!(h.rect(Id::of(NOTES)).is_none());
    assert!(h.rect(Id::of(INSTALL)).is_some());
  }

  /// A failed download, check or relaunch offers Retry and Close, under its own title.
  #[test]
  fn a_failure_offers_retry_and_close() {
    let r = fake_check("1.2.0").unwrap();
    let failures = [
      (State::Failed(r, "the download stopped".into()), "The update failed"),
      (State::Failed(None, "offline".into()), "Couldn't check for updates"),
      (State::Relaunched("the new version didn't start".into()), "The update failed"),
    ];
    for (state, title) in failures {
      for (id, act) in [(RETRY, Act::Retry), (CLOSE, Act::Close)] {
        let (mut h, acts) = window(state.clone());
        assert!(h.a11y().contains(title), "{state:?}: {}", h.a11y());
        assert!(h.click_id(Id::of(id)), "{id}");
        assert_eq!(*acts.borrow(), [act], "{state:?} {id}");
      }
    }
  }

  /// Every state in both looks draws its title and one way out of the window at least; with
  /// `WAVO_SHOTS` set (`make shots`), written there.
  #[test]
  fn every_state_renders_in_both_looks() {
    let r = fake_check("1.2.0").unwrap().unwrap();
    let states = [
      ("available", State::Available(r.clone())),
      ("checking", State::Checking),
      ("up-to-date", State::UpToDate),
      ("downloading", State::Downloading(r.clone(), 42)),
      ("restarting", State::Restarting(r.clone())),
      ("failed", State::Failed(Some(r), "the download stopped: the network is down".into())),
      ("check-failed", State::Failed(None, OFFLINE.into())),
      (
        "relaunched",
        State::Relaunched("installing the update: the new version didn't start".into()),
      ),
    ];
    let dir = std::env::var_os("WAVO_SHOTS").map(PathBuf::from);
    for (look, appearance) in [("light", Appearance::LIGHT), ("dark", Appearance::DARK)] {
      for (name, state) in &states {
        let (mut h, _) = window(state.clone());
        h.event(Event::Appearance(appearance));
        let ground = h.render().pixel(4.0, 4.0);
        assert_eq!(ground[0] < 100, appearance.dark, "{name} {look}: {ground:?}");
        let out = [CLOSE, LATER].iter().any(|id| h.rect(Id::of(*id)).is_some());
        assert!(out, "{name}: no way out");
        if let Some(dir) = &dir {
          std::fs::create_dir_all(dir).unwrap();
          h.shot(&dir.join(format!("update-{name}-{look}.png"))).unwrap();
        }
      }
    }
  }
}
