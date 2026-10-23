// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The dictation state machine, as a pure reducer: [`Machine::handle`] takes an [`Input`] (hotkey
//! edges with their timestamps, commands, and what came of earlier effects) and returns the
//! [`Effect`] to carry out, if any. It does no I/O and reads no clock, so every transition is
//! tested directly.
//!
//! `Idle → Recording → Transcribing → Pasting → Idle`; a cancel, a failure or a result with
//! nothing to paste goes straight back to `Idle`. Each dictation has a [`Take`] number, and
//! results for any take but the current one (a cancelled dictation's late result) are ignored.

use std::time::{Duration, Instant};

use crate::settings::Activation;

/// In [`Activation::HoldOrToggle`], a press held at least this long is push-to-talk; a shorter
/// one is a tap that latches the recording on.
pub const HOLD_THRESHOLD: Duration = Duration::from_millis(300);
/// A press this soon after the previous one, with no release in between, is autorepeat even when
/// the platform does not flag it (macOS waits at most ~1.8 s before it repeats a key). Later, a
/// press counts again: a release that never arrived costs at most this long.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(2);

/// Number of a dictation, from 1.
pub type Take = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
  Idle,
  Recording { take: Take, hold: Hold },
  Transcribing { take: Take },
  Pasting { take: Take },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
  /// The hotkey that started the recording is down since `since`: its release decides.
  Held { since: Instant },
  /// Records until the next press or a stop command: a tap, toggle mode or a command started it.
  Latched,
}

/// From `wavo-app ctl` or the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
  Toggle,
  Start,
  Stop,
  Cancel,
}

#[cfg_attr(not(test), expect(dead_code, reason = "the hotkey (step 7) sends the key inputs"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
  /// The hotkey went down at `at`; `repeat` is keyboard autorepeat.
  KeyDown {
    at: Instant,
    repeat: bool,
  },
  KeyUp {
    at: Instant,
  },
  /// Another key went down while the hotkey, a modifier key alone, was held: the press was the
  /// user's chord (⌥E), not the hotkey. A recording it started is dropped without a trace; what
  /// it stopped stays stopped.
  KeyAborted,
  /// Escape: cancels a recording and nothing else.
  Escape,
  Command(Command),
  /// The microphone did not open: the recording never started.
  RecordingFailed {
    take: Take,
  },
  /// The microphone stopped by itself: what it recorded is transcribed.
  MicStopped {
    take: Take,
  },
  /// The dictation ended with nothing to paste: no speech, a failure, an empty text.
  Finished {
    take: Take,
  },
  /// Text to paste is ready.
  Transcribed {
    take: Take,
  },
  /// The text was pasted, or the paste was refused.
  Delivered {
    take: Take,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
  /// Open the microphone, note where the user is typing, prewarm the engine.
  StartRecording { take: Take },
  /// Close the microphone and transcribe the recording.
  StopRecording { take: Take },
  /// Close the microphone; the audio is kept as a cancelled entry.
  CancelRecording { take: Take },
  /// Close the microphone and drop the recording without a word: the hotkey's press was not one.
  AbortRecording { take: Take },
  /// Drop the engine's work on the recording: a piece it is running finishes unread, and the warm
  /// engine thread stays for the next one.
  CancelTranscription { take: Take },
  /// Paste the text.
  Deliver { take: Take },
}

#[derive(Debug)]
pub struct Machine {
  activation: Activation,
  state: State,
  last_take: Take,
  /// The last press or repeat of the hotkey, if no release came after it.
  key_down: Option<Instant>,
  /// The take the hotkey's press in progress started, until its release.
  pressed: Option<Take>,
}

impl Machine {
  pub fn new(activation: Activation) -> Self {
    Self { activation, state: State::Idle, last_take: 0, key_down: None, pressed: None }
  }

  pub fn state(&self) -> State {
    self.state
  }

  /// Takes effect from the next hotkey event; the recording in progress keeps its hold.
  pub fn set_activation(&mut self, activation: Activation) {
    self.activation = activation;
  }

  pub fn handle(&mut self, input: Input) -> Option<Effect> {
    use State::{Idle, Pasting, Recording, Transcribing};
    match (self.state, self.track_key(input)) {
      // Autorepeat never means anything.
      (_, Input::KeyDown { repeat: true, .. }) => None,
      (Idle, Input::KeyDown { at, .. }) => {
        let effect = self.start(match self.activation {
          Activation::Toggle => Hold::Latched,
          Activation::HoldOrToggle | Activation::PushToTalk => Hold::Held { since: at },
        });
        self.pressed = Some(self.last_take);
        effect
      }
      // A second press before the release: autorepeat without the flag, or a release that never
      // arrived. Either way the release to come decides.
      (Recording { hold: Hold::Held { .. }, .. }, Input::KeyDown { .. }) => None,
      (Recording { take, hold: Hold::Latched }, Input::KeyDown { .. }) => self.stop(take),
      (Recording { take, hold: Hold::Held { since } }, Input::KeyUp { at }) => {
        let tap = at.saturating_duration_since(since) < HOLD_THRESHOLD;
        if self.activation == Activation::HoldOrToggle && tap {
          self.state = Recording { take, hold: Hold::Latched };
          None
        } else {
          self.stop(take)
        }
      }
      // Transcribing and pasting ignore the hotkey; releases outside a held recording mean nothing.
      (_, Input::KeyDown { .. } | Input::KeyUp { .. }) => None,

      (Recording { take, .. }, Input::KeyAborted) if self.pressed == Some(take) => {
        self.pressed = None;
        self.state = Idle;
        Some(Effect::AbortRecording { take })
      }
      (_, Input::KeyAborted) => {
        self.pressed = None;
        None
      }

      (Recording { take, .. }, Input::Escape | Input::Command(Command::Cancel)) => {
        self.state = Idle;
        Some(Effect::CancelRecording { take })
      }
      (Transcribing { take }, Input::Command(Command::Cancel)) => {
        self.state = Idle;
        Some(Effect::CancelTranscription { take })
      }
      (Idle, Input::Command(Command::Toggle | Command::Start)) => self.start(Hold::Latched),
      (Recording { take, .. }, Input::Command(Command::Toggle | Command::Stop)) => self.stop(take),
      (_, Input::Escape | Input::Command(_)) => None,

      (Recording { take, .. }, Input::RecordingFailed { take: t }) if t == take => {
        self.state = Idle;
        None
      }
      (Recording { take, .. }, Input::MicStopped { take: t }) if t == take => self.stop(take),
      (Transcribing { take }, Input::Finished { take: t }) if t == take => {
        self.state = Idle;
        None
      }
      (Transcribing { take }, Input::Transcribed { take: t }) if t == take => {
        self.state = Pasting { take };
        Some(Effect::Deliver { take })
      }
      (Pasting { take }, Input::Delivered { take: t }) if t == take => {
        self.state = Idle;
        None
      }
      // Late news of a take that is no longer current.
      (
        _,
        Input::RecordingFailed { .. }
        | Input::MicStopped { .. }
        | Input::Finished { .. }
        | Input::Transcribed { .. }
        | Input::Delivered { .. },
      ) => None,
    }
  }

  /// Flags an unflagged autorepeat (see [`REPEAT_WINDOW`]).
  fn track_key(&mut self, input: Input) -> Input {
    match input {
      Input::KeyDown { at, repeat } => {
        let held =
          self.key_down.is_some_and(|last| at.saturating_duration_since(last) < REPEAT_WINDOW);
        self.key_down = Some(at);
        Input::KeyDown { at, repeat: repeat || held }
      }
      Input::KeyUp { .. } => {
        self.key_down = None;
        self.pressed = None;
        input
      }
      Input::KeyAborted => {
        self.key_down = None;
        input
      }
      _ => input,
    }
  }

  fn start(&mut self, hold: Hold) -> Option<Effect> {
    self.last_take += 1;
    let take = self.last_take;
    self.state = State::Recording { take, hold };
    Some(Effect::StartRecording { take })
  }

  fn stop(&mut self, take: Take) -> Option<Effect> {
    self.state = State::Transcribing { take };
    Some(Effect::StopRecording { take })
  }
}

#[cfg(test)]
mod tests {
  use super::Command::{Cancel, Start, Stop, Toggle};
  use super::Effect::{
    AbortRecording, CancelRecording, CancelTranscription, Deliver, StartRecording, StopRecording,
  };
  use super::Input::{
    Delivered, Escape, Finished, KeyAborted, KeyDown, KeyUp, MicStopped, RecordingFailed,
    Transcribed,
  };
  use super::{
    Activation, Command, Duration, Effect, HOLD_THRESHOLD, Hold, Instant, Machine, REPEAT_WINDOW,
    State, Take,
  };

  /// Drives a machine with inputs at times in ms from a fixed start.
  struct Drive {
    m: Machine,
    t0: Instant,
  }

  impl Drive {
    fn new(activation: Activation) -> Self {
      Self { m: Machine::new(activation), t0: Instant::now() }
    }

    fn at(&self, ms: u64) -> Instant {
      self.t0 + Duration::from_millis(ms)
    }

    fn down(&mut self, ms: u64) -> Option<Effect> {
      let at = self.at(ms);
      self.m.handle(KeyDown { at, repeat: false })
    }

    fn repeat(&mut self, ms: u64) -> Option<Effect> {
      let at = self.at(ms);
      self.m.handle(KeyDown { at, repeat: true })
    }

    fn up(&mut self, ms: u64) -> Option<Effect> {
      let at = self.at(ms);
      self.m.handle(KeyUp { at })
    }

    fn cmd(&mut self, c: Command) -> Option<Effect> {
      self.m.handle(super::Input::Command(c))
    }

    fn state(&self) -> State {
      self.m.state()
    }

    fn recording(&self) -> bool {
      matches!(self.state(), State::Recording { .. })
    }

    fn latched(&self) -> bool {
      matches!(self.state(), State::Recording { hold: Hold::Latched, .. })
    }

    /// Transcribing → text → pasted → idle.
    fn finish(&mut self, take: Take) {
      assert_eq!(self.m.handle(Transcribed { take }), Some(Deliver { take }));
      assert_eq!(self.state(), State::Pasting { take });
      assert_eq!(self.m.handle(Delivered { take }), None);
      assert_eq!(self.state(), State::Idle);
    }
  }

  #[test]
  fn a_press_that_was_a_chord_drops_the_recording_it_started() {
    for activation in [Activation::HoldOrToggle, Activation::PushToTalk, Activation::Toggle] {
      let mut d = Drive::new(activation);
      assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
      assert_eq!(d.m.handle(KeyAborted), Some(AbortRecording { take: 1 }), "{activation:?}");
      assert_eq!(d.state(), State::Idle);
      assert_eq!(d.up(80), None, "its release means nothing");
      // The next press starts afresh.
      assert_eq!(d.down(5_000), Some(StartRecording { take: 2 }));
    }
    // A press that stopped a latched recording: what it stopped stays stopped.
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    d.up(100);
    assert_eq!(d.down(3_000), Some(StopRecording { take: 1 }));
    assert_eq!(d.m.handle(KeyAborted), None);
    assert_eq!(d.state(), State::Transcribing { take: 1 });
    // After the release, the press is over: a later abort is nobody's.
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    d.up(100);
    assert_eq!(d.m.handle(KeyAborted), None);
    assert!(d.latched());
  }

  #[test]
  fn hold_or_toggle_hold_is_push_to_talk() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
    assert_eq!(d.state(), State::Recording { take: 1, hold: Hold::Held { since: d.at(0) } });
    assert_eq!(d.up(2_500), Some(StopRecording { take: 1 }));
    assert_eq!(d.state(), State::Transcribing { take: 1 });
    d.finish(1);
  }

  #[test]
  fn hold_or_toggle_threshold_boundary() {
    // Exactly the threshold is a hold.
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(1_000);
    assert_eq!(d.up(1_000 + HOLD_THRESHOLD.as_millis() as u64), Some(StopRecording { take: 1 }));
    // A millisecond less is a tap.
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(1_000);
    assert_eq!(d.up(1_000 + HOLD_THRESHOLD.as_millis() as u64 - 1), None);
    assert!(d.latched());
  }

  #[test]
  fn hold_or_toggle_tap_latches_and_the_next_press_stops() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
    assert_eq!(d.up(120), None, "a tap latches");
    assert!(d.latched());
    assert_eq!(d.down(4_000), Some(StopRecording { take: 1 }), "the second tap stops");
    assert_eq!(d.up(4_090), None);
    assert_eq!(d.state(), State::Transcribing { take: 1 });
    d.finish(1);
    // A long second press stops on its down as well; its release means nothing.
    d.down(10_000);
    d.up(10_100);
    assert_eq!(d.down(12_000), Some(StopRecording { take: 2 }));
    assert_eq!(d.up(14_000), None);
  }

  #[test]
  fn push_to_talk_stops_on_release_however_short() {
    let mut d = Drive::new(Activation::PushToTalk);
    assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
    assert_eq!(d.up(40), Some(StopRecording { take: 1 }));
    d.finish(1);
    assert_eq!(d.down(1_000), Some(StartRecording { take: 2 }));
    assert_eq!(d.up(9_000), Some(StopRecording { take: 2 }));
  }

  #[test]
  fn toggle_starts_and_stops_on_presses_only() {
    let mut d = Drive::new(Activation::Toggle);
    assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
    assert!(d.latched());
    assert_eq!(d.up(2_000), None, "a long hold changes nothing");
    assert_eq!(d.down(5_000), Some(StopRecording { take: 1 }));
    assert_eq!(d.up(5_050), None);
    d.finish(1);
  }

  #[test]
  fn autorepeat_is_ignored_everywhere() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    // Held: repeats, flagged or not, do not stop it; the release does.
    d.down(0);
    for ms in (500..2_000).step_by(33) {
      assert_eq!(d.repeat(ms), None);
    }

    assert_eq!(d.down(2_010), None, "a second down while held");
    assert!(d.recording());
    assert_eq!(d.up(2_100), Some(StopRecording { take: 1 }), "measured from the first down");
    // Transcribing: a press and its repeats are ignored, and so are they once idle again.
    assert_eq!(d.down(2_200), None);
    assert_eq!(d.repeat(2_700), None);
    d.finish(1);
    assert_eq!(d.repeat(2_733), None, "the key is still held from before");
    assert_eq!(d.state(), State::Idle);
    assert_eq!(d.up(3_000), None);
    // Latched: a repeat is not a second press.
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    assert_eq!(d.repeat(600), None);
    assert!(d.latched());
  }

  #[test]
  fn unflagged_autorepeat_is_told_by_its_timing() {
    // Toggle latches on the first press: a repeat must not stop it.
    let mut d = Drive::new(Activation::Toggle);
    assert_eq!(d.down(0), Some(StartRecording { take: 1 }));
    for ms in (500..4_000).step_by(33) {
      assert_eq!(d.down(ms), None, "unflagged repeat at {ms} ms");
    }

    assert!(d.latched());
    d.up(4_000);
    assert_eq!(d.down(4_200), Some(StopRecording { take: 1 }), "a press after a release counts");
    // The press that stopped a take that ended at once (no speech) is still held: its repeats in
    // Idle start nothing.
    d.m.handle(Finished { take: 1 });
    assert_eq!(d.state(), State::Idle);
    assert_eq!(d.down(4_700), None);
    assert_eq!(d.down(4_733), None);
    assert_eq!(d.state(), State::Idle);
  }

  #[test]
  fn a_lost_release_costs_at_most_the_repeat_window() {
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    d.down(1_000); // the release of the first press never came
    assert!(d.latched(), "taken for a repeat");
    let later = REPEAT_WINDOW.as_millis() as u64 + 1_000;
    assert_eq!(d.down(later), Some(StopRecording { take: 1 }), "a press after the window counts");
    // Held in hold-or-toggle: a press without a release waits for the next release.
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    assert_eq!(d.down(5_000), None);
    assert_eq!(d.up(5_100), Some(StopRecording { take: 1 }), "measured from the first press");
  }

  #[test]
  fn re_entry_during_transcribing_and_pasting_is_ignored() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    d.up(1_000);
    let busy = State::Transcribing { take: 1 };
    assert_eq!(d.state(), busy);
    for input in [d.down(1_100), d.up(1_150), d.cmd(Toggle), d.cmd(Start), d.cmd(Stop)] {
      assert_eq!(input, None);
    }

    assert_eq!(d.m.handle(Escape), None, "Escape only cancels a recording");
    assert_eq!(d.state(), busy);
    assert_eq!(d.m.handle(Transcribed { take: 1 }), Some(Deliver { take: 1 }));
    for input in [d.down(1_200), d.cmd(Toggle), d.cmd(Cancel), d.m.handle(Escape)] {
      assert_eq!(input, None);
    }

    assert_eq!(d.state(), State::Pasting { take: 1 });
  }

  #[test]
  fn escape_cancels_a_recording_in_every_mode() {
    for activation in [Activation::HoldOrToggle, Activation::PushToTalk, Activation::Toggle] {
      let mut d = Drive::new(activation);
      d.down(0);
      assert_eq!(d.m.handle(Escape), Some(CancelRecording { take: 1 }), "{activation:?}");
      assert_eq!(d.state(), State::Idle);
      assert_eq!(d.up(500), None, "the release after a cancel");
      assert_eq!(d.m.handle(Escape), None, "nothing to cancel");
    }
  }

  #[test]
  fn cancel_command_stops_recording_or_transcription() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    assert_eq!(d.cmd(Cancel), None);
    d.cmd(Start);
    assert_eq!(d.cmd(Cancel), Some(CancelRecording { take: 1 }));
    d.cmd(Start);
    d.cmd(Stop);
    assert_eq!(d.cmd(Cancel), Some(CancelTranscription { take: 2 }));
    assert_eq!(d.state(), State::Idle);
  }

  #[test]
  fn late_results_of_a_cancelled_take_are_ignored() {
    let mut d = Drive::new(Activation::Toggle);
    d.down(0);
    d.up(100);
    d.down(1_000);
    d.up(1_100);
    assert_eq!(d.cmd(Cancel), Some(CancelTranscription { take: 1 }));
    assert_eq!(d.down(2_000), Some(StartRecording { take: 2 }));
    d.up(2_100);
    for late in [Transcribed { take: 1 }, Finished { take: 1 }, MicStopped { take: 1 }] {
      assert_eq!(d.m.handle(late), None);
    }

    assert_eq!(d.m.handle(RecordingFailed { take: 1 }), None);
    assert!(d.recording());
    d.down(3_000);
    assert_eq!(d.m.handle(Delivered { take: 2 }), None, "not pasting");
    assert_eq!(d.state(), State::Transcribing { take: 2 });
    d.finish(2);
  }

  #[test]
  fn commands_start_latched_recordings() {
    let mut d = Drive::new(Activation::PushToTalk);
    assert_eq!(d.cmd(Stop), None);
    assert_eq!(d.cmd(Toggle), Some(StartRecording { take: 1 }));
    assert!(d.latched(), "even in push-to-talk: there is no key to release");
    assert_eq!(d.cmd(Start), None, "already recording");
    assert_eq!(d.up(100), None);
    assert_eq!(d.cmd(Toggle), Some(StopRecording { take: 1 }));
    d.finish(1);
    // A hotkey press stops a recording a command started, and the other way round.
    d.cmd(Start);
    assert_eq!(d.down(5_000), Some(StopRecording { take: 2 }));
    d.up(5_100);
    d.finish(2);
    d.down(6_000);
    assert_eq!(d.cmd(Stop), Some(StopRecording { take: 3 }));
    assert_eq!(d.up(7_000), None, "the release after a stop command");
  }

  #[test]
  fn device_outcomes() {
    let mut d = Drive::new(Activation::HoldOrToggle);
    d.down(0);
    assert_eq!(d.m.handle(RecordingFailed { take: 1 }), None);
    assert_eq!(d.state(), State::Idle);
    assert_eq!(d.up(2_000), None);
    d.cmd(Start);
    assert_eq!(d.m.handle(MicStopped { take: 2 }), Some(StopRecording { take: 2 }));
    // Nothing to paste: no speech, a failure, an empty text.
    assert_eq!(d.m.handle(Finished { take: 2 }), None);
    assert_eq!(d.state(), State::Idle);
  }
}
