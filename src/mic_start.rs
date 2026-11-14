// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! A USB microphone grown slow to start against its own normal: macOS's usbaudiod lengthens a USB
//! device's start 50 ms at a time until it is plugged in again, and the start of every phrase is
//! lost. Wavo can't fix the driver; the menu says to reconnect it.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tracing::{info, warn};

use crate::audio::{Device, Recorder, Transport};
use crate::session::Take;

/// A USB microphone's first starts since the launch give its normal, its last ones its typical
/// start now: the start drifts over hours, and one slow start (a device waking up) is no drift.
const WINDOW: usize = 20;
/// How much slower than the device's own normal its typical start must get before Wavo warns.
const SLOWER_BY: Duration = Duration::from_millis(200);
/// Starts in a row back near the normal that clear the warning: a replug drops the delay by
/// hundreds of ms at once.
const RECENT: usize = 5;
/// The menu's warning; the log has the device and the times.
pub const SLOW: &str = "Microphone is slow to start: reconnect it";

/// Each USB microphone's starts, in memory since the app launched, and the warning, raised once per
/// climb. Others vary by themselves (a Bluetooth headset held open by another app or cold, a
/// suspended PipeWire node), and a replug wouldn't help them.
#[derive(Debug, Default)]
pub struct SlowStart {
  /// By the device's ID, else its name.
  devices: HashMap<String, Starts>,
  /// The key of the last recording's device: only its warning shows.
  device: Option<String>,
}

/// A recording's start, as its recorder tells at the stop.
pub struct Start<'a> {
  device: &'a str,
  id: Option<&'a str>,
  transport: Transport,
  /// The first samples came this long after the open began.
  heard: Option<Duration>,
  /// How long ago the open began.
  open: Duration,
  /// The chosen device failed and this one took over: the start includes that attempt.
  gave_way: bool,
  /// Cancelled or aborted (a chord): it may stop before the sound could come.
  cancelled: bool,
}

impl<'a> Start<'a> {
  pub fn of(recorder: &'a Recorder, cancelled: bool) -> Self {
    Self {
      device: recorder.device(),
      id: recorder.id(),
      transport: recorder.transport(),
      heard: recorder.started_in(),
      open: recorder.opened().elapsed(),
      gave_way: recorder.warning().is_some(),
      cancelled,
    }
  }
}

impl SlowStart {
  /// Logs the recording's start once, before its stop, and records it.
  pub fn note(&mut self, take: Take, start: Start) {
    match start.heard {
      Some(after) => info!("dictation {take}: sound after {} ms", after.as_millis()),
      None => info!("dictation {take}: no sound in {} ms, up to the stop", start.open.as_millis()),
    }

    self.record(take, start);
  }

  /// Counts a kept USB recording's start; `true` when it raises the warning.
  fn record(&mut self, take: Take, start: Start) -> bool {
    if start.cancelled {
      return false;
    }

    let key = start.id.unwrap_or(start.device).to_owned();
    self.device = Some(key.clone());
    if start.transport != Transport::Usb || start.gave_way {
      return false;
    }

    // Stopped before any sound: a start at least that slow, which counts only past the limit.
    let starts = self.devices.entry(key).or_default();
    let limit = |b| start.open < b + SLOWER_BY;
    if start.heard.is_none() && starts.baseline.is_none_or(limit) {
      return false;
    }

    let raised = starts.push(start.heard.unwrap_or(start.open));
    if raised {
      let (now, normal) = (median(&starts.last).as_millis(), starts.baseline.unwrap().as_millis());
      warn!("dictation {take}: {} starts in {now} ms, normal {normal} ms: replug", start.device);
    }

    raised
  }

  /// The inputs changed: a warned microphone `listed` no more was unplugged, and starts fast once
  /// plugged in again. A replug by hand takes longer than nimbo's 250 ms burst, so the unplug comes
  /// as a change of its own; one of another device (an output too) keeps the warning, and a missed
  /// unplug clears on [RECENT] starts back. Its window starts again, the normal kept: still slow,
  /// it warns after a whole window.
  pub fn inputs_changed(&mut self, listed: &[Device]) {
    let listed = |key: &str| listed.iter().any(|d| d.id.as_deref().unwrap_or(&d.name) == key);
    for (key, starts) in self.devices.iter_mut().filter(|(key, s)| s.slow && !listed(key)) {
      info!("{key}: unplugged: the slow start warning cleared");
      starts.last.clear();
      starts.slow = false;
    }
  }

  /// Another microphone was chosen: the warning, of the one that recorded last, goes until a
  /// recording says which records now; the one chosen still slow, it is back after it.
  pub fn chosen(&mut self) {
    self.device = None;
  }

  pub fn warning(&self) -> Option<String> {
    let key = self.device.as_ref()?;
    self.devices.get(key).filter(|s| s.slow).map(|_| SLOW.to_owned())
  }
}

/// A USB microphone's starts.
#[derive(Debug, Default)]
struct Starts {
  /// Its normal: the median of its first [WINDOW] starts, or of a whole faster window since.
  baseline: Option<Duration>,
  /// Its last [WINDOW] starts, since the calibration began or the warning cleared.
  last: VecDeque<Duration>,
  /// The window's median is past the normal by [SLOWER_BY]: until [RECENT] starts are back, or it
  /// is unplugged.
  slow: bool,
}

impl Starts {
  /// A start; `true` when it raises the warning.
  fn push(&mut self, after: Duration) -> bool {
    if self.last.len() == WINDOW {
      self.last.pop_front();
    }

    self.last.push_back(after);
    let Some(baseline) = self.baseline else {
      if self.last.len() == WINDOW {
        self.baseline = Some(median(&self.last));
      }

      return false;
    };

    // Back near the normal (a replug): the climb is over, and the window restarts from them.
    let recent = self.last.range(self.last.len().saturating_sub(RECENT)..);
    if self.slow && recent.len() == RECENT && recent.clone().all(|&d| d < baseline + SLOWER_BY / 2)
    {
      self.last = recent.copied().collect();
      self.slow = false;
      return false;
    }

    // A whole window faster: the new normal. Fewer may be another app holding the device open.
    let (full, now) = (self.last.len() == WINDOW, median(&self.last));
    if full && self.last.iter().all(|&d| d < baseline.saturating_sub(SLOWER_BY / 2)) {
      (self.baseline, self.slow) = (Some(now), false);
      return false;
    }

    let raised = full && now > baseline + SLOWER_BY && !self.slow;
    self.slow |= raised;
    raised
  }
}

/// The middle one, the later of two.
fn median(starts: &VecDeque<Duration>) -> Duration {
  let mut sorted: Vec<_> = starts.iter().copied().collect();
  sorted.sort();
  sorted[sorted.len() / 2]
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::Status;
  use crate::window::Pick;
  use crate::{audio, menu};

  const RODE: &str = "RØDE PodMic USB";

  /// A kept recording on `device` that heard sound `ms` after the open began.
  fn start(device: &str, transport: Transport, ms: u64) -> Start<'_> {
    let after = Duration::from_millis(ms);
    let (heard, open, gave_way, cancelled) = (Some(after), after, false, false);
    Start { device, id: None, transport, heard, open, gave_way, cancelled }
  }

  /// `ms` starts on the USB `device`: how many raised the warning.
  fn starts(s: &mut SlowStart, device: &str, ms: impl IntoIterator<Item = u64>) -> usize {
    ms.into_iter().filter(|&ms| s.record(1, start(device, Transport::Usb, ms))).count()
  }

  /// `n` starts of `ms`.
  fn level(ms: u64, n: usize) -> impl Iterator<Item = u64> {
    std::iter::repeat_n(ms, n)
  }

  fn baseline(s: &SlowStart) -> Option<u128> {
    s.devices[RODE].baseline.map(|b| b.as_millis())
  }

  /// The author's RØDE over 25 h of dictations, each level its count: calibrated at 359 ms, the
  /// warning comes once, in the 560 ms level, and stays through 610; after the replug to 83 ms it
  /// clears on the fifth start, and 83 is the normal once a whole window is.
  #[test]
  fn a_usb_microphone_slower_to_start_than_its_normal_warns_once() {
    let elgato = "Elgato Wave:3";
    let mut s = SlowStart::default();
    assert_eq!((starts(&mut s, RODE, level(359, 90)), baseline(&s)), (0, Some(359)));
    let climb = level(409, 149).chain(level(459, 22)).chain(level(507, 173));
    assert_eq!(starts(&mut s, RODE, climb), 0);
    assert_eq!((starts(&mut s, RODE, level(560, 320)), s.warning().is_some()), (1, true));
    assert_eq!((starts(&mut s, RODE, level(610, 71)), s.warning().is_some()), (0, true));
    let warning = "Microphone is slow to start: reconnect it";
    assert_eq!(s.warning().as_deref(), Some(warning));

    // In the menu, whole.
    let status = Status { warnings: vec![warning.into()], ..Status::default() };
    let system = menu::System { accessibility: true, ..menu::System::default() };
    let shown = menu::warnings(&system, "f5", &status);
    let mics = (&[][..], &audio::Microphone::Automatic, false);
    let items = menu::menu(None, &status, &shown, &[], mics, (&[], &Pick::default()), None).items;
    assert_eq!(items[1].label(), format!("⚠︎ {warning}"));

    // Another device records, one of the same name too: no warning; each keeps its own.
    assert_eq!(starts(&mut s, elgato, level(300, 5)), 0);
    assert_eq!(s.warning(), None);
    s.record(1, Start { id: Some("usb-2"), ..start(RODE, Transport::Usb, 610) });
    assert_eq!(s.warning(), None);
    s.record(1, start(RODE, Transport::Usb, 610));
    assert_eq!(s.warning().as_deref(), Some(warning));

    // Replugged: cleared on the fifth start, the new normal after a whole window.
    starts(&mut s, RODE, level(83, 4));
    assert!(s.warning().is_some());
    starts(&mut s, RODE, level(83, 1));
    assert_eq!((s.warning(), baseline(&s)), (None, Some(359)));
    assert_eq!((starts(&mut s, RODE, level(83, 14)), baseline(&s)), (0, Some(359)));
    assert_eq!((starts(&mut s, RODE, level(83, 1)), baseline(&s)), (0, Some(83)));
  }

  /// A climb that comes back near the normal clears the warning and keeps the normal; a second
  /// climb warns again. Fast starts for a while (another app holding the device open) are no new
  /// normal, a whole window of them (a replug) is.
  #[test]
  fn a_cleared_warning_comes_back_with_the_next_climb() {
    let mut s = SlowStart::default();
    assert_eq!(starts(&mut s, RODE, level(359, 20).chain(level(610, 11))), 1);
    assert_eq!(
      (starts(&mut s, RODE, level(409, 5)), s.warning(), baseline(&s)),
      (0, None, Some(359))
    );
    assert_eq!(starts(&mut s, RODE, level(610, 20)), 1);
    assert!(s.warning().is_some());

    let mut s = SlowStart::default();
    let held = level(359, 20).chain(level(10, 11)).chain(level(359, 20));
    assert_eq!((starts(&mut s, RODE, held), baseline(&s)), (0, Some(359)));
    assert_eq!((starts(&mut s, RODE, level(83, 19)), baseline(&s)), (0, Some(359)));
    assert_eq!((starts(&mut s, RODE, level(83, 1)), baseline(&s)), (0, Some(83)));
  }

  /// The warned microphone unplugged: the warning goes at once, the normal kept; another change,
  /// it listed still, keeps it. Still slow, it comes back after a whole window; fast, a whole
  /// window is the new normal.
  #[test]
  fn an_unplug_clears_the_warning() {
    let rode = Device { id: None, name: RODE.into(), default: true, transport: Transport::Usb };
    let mut s = SlowStart::default();
    assert_eq!(starts(&mut s, RODE, level(359, 20).chain(level(610, 11))), 1);
    s.inputs_changed(std::slice::from_ref(&rode));
    assert!(s.warning().is_some(), "listed still");

    s.inputs_changed(&[]);
    assert_eq!((s.warning(), baseline(&s)), (None, Some(359)));
    assert_eq!(starts(&mut s, RODE, level(610, 19)), 0);
    assert_eq!((starts(&mut s, RODE, level(610, 1)), s.warning().is_some()), (1, true));

    s.inputs_changed(&[]);
    assert_eq!(
      (starts(&mut s, RODE, level(83, 20)), s.warning(), baseline(&s)),
      (0, None, Some(83))
    );
  }

  /// Another microphone chosen: the warning goes at once; a recording by the slow one again brings
  /// it back.
  #[test]
  fn choosing_another_mic_clears_the_warning() {
    let mut s = SlowStart::default();
    assert_eq!(starts(&mut s, RODE, level(359, 20).chain(level(610, 11))), 1);
    s.chosen();
    assert_eq!(s.warning(), None);
    starts(&mut s, RODE, level(610, 1));
    assert!(s.warning().is_some(), "the slow one records again");
  }

  /// Only a kept recording's start counts: not a cancelled or aborted one, nor one after the chosen
  /// device failed. One stopped before any sound counts only once its time is past the normal's
  /// limit, never in the calibration. Each device calibrates on its own 20 starts.
  #[test]
  fn only_a_kept_start_counts() {
    let mut s = SlowStart::default();
    let count = |s: &SlowStart| s.devices.get(RODE).map_or(0, |d| d.last.len());
    s.record(1, Start { cancelled: true, ..start(RODE, Transport::Usb, 20) });
    s.record(1, Start { gave_way: true, ..start(RODE, Transport::Usb, 900) });
    assert_eq!(count(&s), 0);

    let silent = |ms| Start { heard: None, ..start(RODE, Transport::Usb, ms) };
    s.record(1, silent(1000));
    assert_eq!(count(&s), 0, "in the calibration");

    // Calibration on its own device's starts, whatever records in between.
    starts(&mut s, RODE, level(359, 10));
    starts(&mut s, "Elgato Wave:3", level(80, 10));
    assert_eq!((count(&s), baseline(&s)), (10, None));
    starts(&mut s, RODE, level(359, 10));
    assert_eq!((count(&s), baseline(&s)), (20, Some(359)));

    // Stopped before any sound: 100 ms is no start, 700 ms one at least that slow.
    s.record(1, silent(100));
    assert_eq!(s.devices[RODE].last.back(), Some(&Duration::from_millis(359)));
    s.record(1, silent(700));
    assert_eq!(s.devices[RODE].last.back(), Some(&Duration::from_millis(700)));
    assert_eq!((0..10).filter(|_| s.record(1, silent(700))).count(), 1);
  }

  /// One slow start never warns; a microphone not on USB never does.
  #[test]
  fn an_outlier_or_a_microphone_not_on_usb_never_warns() {
    let mut s = SlowStart::default();
    let outlier = level(359, 30).chain([708]).chain(level(359, 30));
    assert_eq!((starts(&mut s, RODE, outlier), s.warning()), (0, None));

    let wh = "WH-1000XM5";
    for transport in
      [Transport::BuiltIn, Transport::Bluetooth, Transport::Virtual, Transport::Unknown]
    {
      for ms in level(20, 30).chain(level(1000, 30)) {
        assert!(!s.record(1, start(wh, transport, ms)));
      }

      assert_eq!(s.warning(), None, "{transport:?}");
    }

    // The same on USB warns.
    assert_eq!(starts(&mut s, wh, level(20, 30).chain(level(1000, 30))), 1);
  }
}
