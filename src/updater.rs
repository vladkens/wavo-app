// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Updates in place with nimbo's [update]: a signed feed beside each release on GitHub, checked at
//! launch and once a day while Advanced's Automatic updates is on, and when the person asks.
//! Nothing opens by itself, as in Ghostty: what was found shows in the menu and on About, and only
//! the person installs it. nimbo's check offers a development build nothing.
//!
//! [Updater] is the state without the network: the app runs the check, the download and the
//! install on threads and brings their results back to it.
//!
//! [update]: nimbo::update

use std::io::{self, ErrorKind};
use std::time::{Duration, SystemTime};

use nimbo::tray::{Item, Menu};
use nimbo::update::{Feed, Release};

/// `update.txt` beside the latest release's files, which CI's publish job writes and signs (nimbo's
/// `pack.py feed`); `latest` is never the dev pre-release.
pub const FEED: Feed = Feed {
  url: "https://github.com/vladkens/wavo-app/releases/latest/download/update.txt",
  key: UPDATE_KEY,
};

/// The Ed25519 public key that signs the feed, 64 hex digits.
// PLACEHOLDER until the person runs `python3 <nimbo>/scripts/pack.py feed-key --repo
// vladkens/wavo-app`, which prints it: until then every check fails ("a bad key").
const UPDATE_KEY: &str = "PLACEHOLDER";

const RELEASES: &str = "https://github.com/vladkens/wavo-app/releases";
/// An automatic check once a day; one that got no answer is tried again at the next [TICK].
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the app asks whether a check is [Updater::due]: a timer would stop while the Mac
/// sleeps.
pub const TICK: Duration = Duration::from_secs(60 * 60);
/// How long "No Updates Available" stays after a check the person asked for.
pub const UP_TO_DATE: Duration = Duration::from_secs(5);

/// The whole percent of `done` bytes of `total`, which the menu shows: the download tells the app
/// only when it changes.
pub fn percent(done: u64, total: u64) -> u8 {
  (done.saturating_mul(100) / total.max(1)).min(100) as u8
}

/// `v1.2.0`, as the app shows its own version.
pub fn shown(version: &str) -> String {
  format!("v{}", version.trim_start_matches('v'))
}

/// The release's page, with its notes.
pub fn notes_url(version: &str) -> String {
  format!("{RELEASES}/tag/{}", shown(version))
}

// MARK: State

/// A release the feed offers: nimbo's, or a version alone in tests.
pub trait Offer: Clone {
  fn version(&self) -> &str;
}

impl Offer for Release {
  fn version(&self) -> &str {
    &self.version
  }
}

impl Offer for String {
  fn version(&self) -> &str {
    self
  }
}

/// What the updater shows, in the menu and on About.
#[derive(Clone, Debug, PartialEq)]
pub enum State<R> {
  /// Nothing: no answer yet, nothing newer, or the newer one put off or skipped.
  Idle,
  /// A check the person asked for runs; an automatic one shows nothing.
  Checking,
  /// The check the person asked for found nothing: said until then.
  UpToDate(SystemTime),
  Available(R),
  /// With the whole percent done.
  Downloading(R, u8),
  /// Downloaded: put in place once no dictation runs, then the new app starts and this one quits.
  Restarting(R),
  /// Why the check, the download or the install failed; Retry checks or downloads again.
  Failed(Option<R>, String),
}

impl<R: Offer> State<R> {
  /// The same with each release as its version: what the menu and the window show.
  pub fn versions(&self) -> State<String> {
    let v = |r: &R| r.version().to_owned();
    match self {
      State::Idle => State::Idle,
      State::Checking => State::Checking,
      State::UpToDate(until) => State::UpToDate(*until),
      State::Available(r) => State::Available(v(r)),
      State::Downloading(r, done) => State::Downloading(v(r), *done),
      State::Restarting(r) => State::Restarting(v(r)),
      State::Failed(r, why) => State::Failed(r.as_ref().map(v), why.clone()),
    }
  }

  /// The release being downloaded or put in place.
  fn busy(&self) -> bool {
    matches!(self, State::Downloading(..) | State::Restarting(_))
  }
}

/// The state, and when to check.
#[derive(Debug)]
pub struct Updater<R> {
  pub state: State<R>,
  /// A check runs: whether the person asked for it.
  checking: Option<bool>,
  /// When a check last got an answer.
  answered: Option<SystemTime>,
  /// This install can't update itself (a package manager's): automatic checks offer nothing.
  stuck: bool,
}

impl<R> Default for Updater<R> {
  fn default() -> Self {
    Self { state: State::Idle, checking: None, answered: None, stuck: false }
  }
}

impl<R: Offer> Updater<R> {
  /// An automatic check: none answered yet, or the last a day before `now` (or after it, the clock
  /// set back).
  pub fn due(&self, now: SystemTime) -> bool {
    let old = |t: SystemTime| now.duration_since(t).map_or(true, |d| d >= DAY);
    self.checking.is_none() && self.answered.is_none_or(old)
  }

  /// Whether to start a check: not while one runs, which the person's joins, or while an update is
  /// on its way. The person's shows Checking.
  pub fn check(&mut self, manual: bool) -> bool {
    if self.state.busy() {
      return false;
    }

    if manual {
      self.state = State::Checking;
    }

    let start = self.checking.is_none();
    self.checking = Some(manual || self.checking == Some(true));
    start
  }

  /// A check's answer at `now`. A missing feed (404) and one whose signature or content is wrong
  /// count as no update. A newer release shows unless nobody asked and it is the version `skipped`
  /// or this install can't take it; nothing newer shows only to the person who asked, as does
  /// another error.
  pub fn checked(&mut self, found: io::Result<Option<R>>, skipped: Option<&str>, now: SystemTime) {
    let Some(manual) = self.checking.take() else { return };
    let found = match found {
      Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::InvalidData) => Ok(None),
      found => found,
    };

    if found.is_ok() {
      self.answered = Some(now);
    }

    if self.state.busy() {
      return;
    }

    self.state = match found {
      Ok(Some(r)) if manual || !self.stuck && skipped != Some(r.version()) => State::Available(r),
      Ok(_) if manual => State::UpToDate(now + UP_TO_DATE),
      Err(e) if manual => State::Failed(None, e.to_string()),
      _ => return,
    };
  }

  /// Install and Relaunch, or Retry after a failed download: the release to download.
  pub fn install(&mut self) -> Option<R> {
    let (State::Available(r) | State::Failed(Some(r), _)) = &self.state else { return None };
    let r = r.clone();
    self.state = State::Downloading(r.clone(), 0);
    Some(r)
  }

  /// Later: the next daily check offers it again.
  pub fn later(&mut self) {
    if matches!(self.state, State::Available(_) | State::Failed(..)) {
      self.state = State::Idle;
    }
  }

  /// Skip This Version: the version to save, which automatic checks pass over from then on.
  pub fn skip(&mut self) -> Option<String> {
    let State::Available(r) = &self.state else { return None };
    let version = r.version().to_owned();
    self.state = State::Idle;
    Some(version)
  }

  /// The download's whole percent, from [percent].
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

  /// The download or the install failed. One this install can't take is quiet (`Unsupported`: a
  /// .deb's in /usr, a build; `PermissionDenied`: a folder the person can't write), and automatic
  /// checks offer nothing more.
  pub fn failed(&mut self, e: &io::Error) {
    let (State::Downloading(r, _) | State::Restarting(r)) = &self.state else { return };
    self.state = match e.kind() {
      ErrorKind::Unsupported | ErrorKind::PermissionDenied => {
        self.stuck = true;
        State::Idle
      }
      _ => State::Failed(Some(r.clone()), e.to_string()),
    };
  }

  /// The update that started this app failed (nimbo's `update::finish`): this one is the old
  /// version again, and Retry checks again.
  pub fn relaunched(&mut self, e: &io::Error) {
    self.state = State::Failed(None, e.to_string());
  }

  /// "No Updates Available" goes at its time.
  pub fn expire(&mut self, now: SystemTime) {
    if matches!(self.state, State::UpToDate(until) if now >= until) {
      self.state = State::Idle;
    }
  }
}

// MARK: Menu

/// What the person does with an update, from the menu or About.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
  Check,
  Install,
  /// The download again, or the check when it was the check that failed.
  Retry,
  Later,
  Skip,
  Notes,
}

impl Act {
  const ALL: [Act; 6] = [Act::Check, Act::Install, Act::Retry, Act::Later, Act::Skip, Act::Notes];

  /// Its menu item's id.
  pub fn id(self) -> &'static str {
    match self {
      Act::Check => "update:check",
      Act::Install => "update:install",
      Act::Retry => "update:retry",
      Act::Later => "update:later",
      Act::Skip => "update:skip",
      Act::Notes => "update:notes",
    }
  }

  pub fn of(id: &str) -> Option<Act> {
    Act::ALL.into_iter().find(|a| a.id() == id)
  }
}

fn item(act: Act, label: &str) -> Item {
  Item::new(act.id(), label)
}

/// The row under the menu's warnings while there is an update or it goes on: Update Available
/// with what to do with it, its download, the restart, a failure with Retry.
pub fn menu_row(state: &State<String>) -> Option<Item> {
  let row = match state {
    State::Available(v) => {
      let label = format!("Update Available: {}", shown(v));
      Item::new("update", &label).submenu(Menu::new(vec![
        item(Act::Install, "Install and Relaunch"),
        item(Act::Later, "Later"),
        item(Act::Skip, "Skip This Version"),
        Item::separator(),
        item(Act::Notes, "Release Notes"),
      ]))
    }
    State::Downloading(_, done) => Item::new("update", &format!("Downloading: {done}%")),
    State::Restarting(_) => Item::new("update", "Restarting…"),
    State::Failed(_, why) => Item::new("update", &format!("Update Failed: {why}"))
      .submenu(Menu::new(vec![item(Act::Retry, "Retry"), item(Act::Later, "Later")])),
    State::Idle | State::Checking | State::UpToDate(_) => return None,
  };

  Some(row.enabled(matches!(state, State::Available(_) | State::Failed(..))))
}

/// The row above Quit: Check for Updates…, or how the check the person asked for went.
pub fn check_row(state: &State<String>) -> Item {
  match state {
    State::Checking => item(Act::Check, "Checking for Updates…").enabled(false),
    State::UpToDate(_) => item(Act::Check, "No Updates Available").enabled(false),
    s => item(Act::Check, "Check for Updates…").enabled(!s.busy()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  type U = Updater<String>;

  const NOW: SystemTime = SystemTime::UNIX_EPOCH;

  fn found(v: &str) -> io::Result<Option<String>> {
    Ok(Some(v.to_owned()))
  }

  fn error(kind: ErrorKind) -> io::Result<Option<String>> {
    Err(io::Error::new(kind, "offline"))
  }

  /// `u` after an automatic or a manual check answered `found`.
  fn checked(u: &mut U, manual: bool, found: io::Result<Option<String>>, skipped: Option<&str>) {
    assert!(u.check(manual));
    u.checked(found, skipped, NOW);
  }

  #[test]
  fn a_version_shows_with_its_v() {
    assert_eq!((shown("1.2.0"), shown("v1.2.0")), ("v1.2.0".into(), "v1.2.0".into()));
    assert_eq!(notes_url("1.2.0"), format!("{RELEASES}/tag/v1.2.0"));
  }

  #[test]
  fn the_percent_is_whole() {
    let got = [(0, 100), (419, 1_000), (999, 1_000), (1_000, 1_000), (5, 0), (u64::MAX, 2)];
    assert_eq!(got.map(|(done, total)| percent(done, total)), [0, 41, 99, 100, 100, 100]);
  }

  #[test]
  fn an_automatic_check_is_due_daily_and_after_a_failure() {
    let mut u = U::default();
    assert!(u.due(NOW), "at launch");
    assert!(u.check(false));
    assert!(!u.due(NOW) && !u.check(false), "one at a time");
    u.checked(error(ErrorKind::TimedOut), None, NOW);
    assert!(u.due(NOW), "no answer: again at the next tick");

    checked(&mut u, false, Ok(None), None);
    assert!(!u.due(NOW + DAY - Duration::from_secs(1)) && u.due(NOW + DAY));
    assert!(u.due(NOW - Duration::from_secs(1)), "the clock set back");
  }

  #[test]
  fn an_automatic_check_shows_only_what_is_new() {
    let mut u = U::default();
    checked(&mut u, false, Ok(None), None);
    checked(&mut u, false, error(ErrorKind::ConnectionRefused), None);
    checked(&mut u, false, found("1.2.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Idle, "nothing new, offline, skipped: quiet");

    checked(&mut u, false, found("1.3.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Available("1.3.0".into()));
    u.later();
    assert_eq!(u.state, State::Idle);
    checked(&mut u, false, found("1.3.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Available("1.3.0".into()), "put off until the next check");
    assert_eq!(u.skip().as_deref(), Some("1.3.0"));
    assert_eq!((u.state.clone(), u.skip()), (State::Idle, None));
  }

  #[test]
  fn the_persons_check_says_how_it_went() {
    let mut u = U::default();
    assert!(u.check(true));
    assert_eq!(u.state, State::Checking);
    u.checked(Ok(None), None, NOW);
    assert_eq!(u.state, State::UpToDate(NOW + UP_TO_DATE));
    u.expire(NOW + UP_TO_DATE - Duration::from_millis(1));
    assert_eq!(u.state, State::UpToDate(NOW + UP_TO_DATE));
    u.expire(NOW + UP_TO_DATE);
    assert_eq!(u.state, State::Idle);

    // A missing feed and a wrong signature are no update; another error shows, Retry checks again.
    for kind in [ErrorKind::NotFound, ErrorKind::InvalidData] {
      checked(&mut u, true, error(kind), None);
      assert!(matches!(u.state, State::UpToDate(_)), "{kind:?}");
    }

    checked(&mut u, true, error(ErrorKind::TimedOut), None);
    assert_eq!(u.state, State::Failed(None, "offline".into()));
    assert_eq!(u.install(), None, "nothing to download: the app checks again");
    checked(&mut u, true, found("1.2.0"), Some("1.2.0"));
    assert_eq!(u.state, State::Available("1.2.0".into()), "asked, a skipped one shows too");
  }

  #[test]
  fn the_persons_check_joins_an_automatic_one() {
    let mut u = U::default();
    assert!(u.check(false));
    assert!(!u.check(true), "no second thread");
    assert_eq!(u.state, State::Checking);
    u.checked(Ok(None), None, NOW);
    assert!(matches!(u.state, State::UpToDate(_)));
  }

  #[test]
  fn an_install_downloads_restarts_or_fails_with_retry() {
    let mut u = U::default();
    assert_eq!(u.install(), None);
    checked(&mut u, false, found("1.2.0"), None);
    assert_eq!(u.install().as_deref(), Some("1.2.0"));
    assert_eq!(u.state, State::Downloading("1.2.0".into(), 0));
    assert!(!u.check(true) && !u.check(false), "no check while it downloads");
    u.progress(percent(41_990, 100_000));
    assert_eq!(u.state, State::Downloading("1.2.0".into(), 41));

    let e = io::Error::other("1.2.0 is not the file");
    u.failed(&e);
    assert_eq!(u.state, State::Failed(Some("1.2.0".into()), "1.2.0 is not the file".into()));
    assert_eq!(u.install().as_deref(), Some("1.2.0"), "Retry");
    u.downloaded();
    assert_eq!(u.state, State::Restarting("1.2.0".into()));
    u.failed(&e);
    u.later();
    assert_eq!(u.state, State::Idle, "a failure put off");

    // An install that can't update (a .deb's): quiet, and automatic checks offer nothing more.
    let mut u = U::default();
    checked(&mut u, false, found("1.2.0"), None);
    u.install();
    u.failed(&io::Error::from(ErrorKind::Unsupported));
    assert_eq!(u.state, State::Idle);
    checked(&mut u, false, found("1.3.0"), None);
    assert_eq!(u.state, State::Idle);
    checked(&mut u, true, found("1.3.0"), None);
    assert_eq!(u.state, State::Available("1.3.0".into()), "asked, it shows");

    // The update that started this app failed: Retry checks again.
    let mut u = U::default();
    u.relaunched(&io::Error::other("the new app did not start"));
    assert_eq!(u.state, State::Failed(None, "the new app did not start".into()));
    assert_eq!(u.install(), None);

    // A check answered during the download leaves it be.
    let mut u = U::default();
    checked(&mut u, false, found("1.2.0"), None);
    assert!(u.check(false));
    u.install();
    u.checked(found("1.3.0"), None, NOW);
    assert_eq!(u.state, State::Downloading("1.2.0".into(), 0));
  }

  #[test]
  fn the_menu_rows_follow_the_state() {
    let label = |s: &State<String>| menu_row(s).map(|i| (i.label, i.enabled));
    assert_eq!(label(&State::Idle), None);
    assert_eq!(
      label(&State::Available("1.2.0".into())),
      Some(("Update Available: v1.2.0".into(), true))
    );
    assert_eq!(
      label(&State::Downloading("1.2.0".into(), 42)),
      Some(("Downloading: 42%".into(), false))
    );
    assert_eq!(label(&State::Restarting("1.2.0".into())), Some(("Restarting…".into(), false)));
    let failed = State::Failed(None, "offline".into());
    assert_eq!(label(&failed), Some(("Update Failed: offline".into(), true)));

    let sub = menu_row(&State::Available("1.2.0".into())).unwrap().submenu.unwrap().items;
    let ids: Vec<_> = sub.iter().map(|i| Act::of(&i.id)).collect();
    let acts = [Act::Install, Act::Later, Act::Skip].map(Some);
    assert_eq!(ids, [acts[0], acts[1], acts[2], None, Some(Act::Notes)]);

    let check = |s: &State<String>| (check_row(s).label, check_row(s).enabled);
    assert_eq!(check(&State::Idle), ("Check for Updates…".into(), true));
    assert_eq!(check(&State::Checking), ("Checking for Updates…".into(), false));
    assert_eq!(check(&State::UpToDate(NOW)), ("No Updates Available".into(), false));
    assert_eq!(check(&State::Downloading("1.2.0".into(), 0)), ("Check for Updates…".into(), false));
  }
}
