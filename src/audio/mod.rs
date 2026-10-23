// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Audio: [`devices`] and [`capture`] (cpal, or a WAV file standing in for the microphone) →
//! `rtrb` → the [`recorder`]'s processing thread: downmix, [`resample`] to 16 kHz, signal level,
//! [`vad`] (earshot) and the [`wav`] sink.

pub mod capture;
pub mod devices;
pub mod recorder;
pub mod resample;
pub mod vad;
pub mod wav;

/// The engine's sample rate: every model takes 16 kHz mono.
pub const SAMPLE_RATE: u32 = 16_000;
