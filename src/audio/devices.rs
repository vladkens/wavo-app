// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Input devices: listing, choice by ID or name, and the fallback to the system default when the
//! chosen device is missing or will not open. The fallback is a warning, never a change of the
//! choice: once the device is back, the next recording uses it again.

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait};
use tracing::debug;

/// A chosen microphone. The ID (CoreAudio's device UID, ALSA's PCM name) is matched first: it
/// survives a rename and tells two devices of the same name apart. The name is shown, and matches
/// when the ID does not (no ID known, or the backend changed it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
  pub id: Option<String>,
  pub name: String,
}

impl Choice {
  /// From the command line: one string that may be either an ID or a name.
  pub fn id_or_name(s: &str) -> Self {
    Self { id: Some(s.to_owned()), name: s.to_owned() }
  }
}

/// A device as the backend lists it.
#[derive(Debug, Clone)]
pub struct Found<D> {
  pub id: Option<String>,
  pub name: String,
  pub device: D,
}

impl<D> Found<D> {
  fn is_id(&self, wanted: &Choice) -> bool {
    wanted.id.is_some() && self.id == wanted.id
  }
}

/// The devices a recorder may open: cpal's in the app, fakes in tests.
pub trait Inputs {
  type Device: Clone + PartialEq;

  /// Input devices in system order.
  fn inputs(&self) -> Vec<Found<Self::Device>>;

  /// The system default input.
  fn default_input(&self) -> Option<Found<Self::Device>>;
}

/// The device that was opened, and why it is not the one asked for, if it is not.
#[derive(Debug)]
pub struct Opened<T> {
  pub name: String,
  pub value: T,
  pub warning: Option<String>,
}

/// Opens `wanted` (`None` is the system default): by ID, else by name. A device that is missing
/// or fails to open falls back to the system default with a warning. The default is looked at
/// first: when it is the device asked for, which is the usual case, the slower listing of every
/// device is skipped.
pub fn open<I: Inputs, T>(
  inputs: &I,
  wanted: Option<&Choice>,
  mut open: impl FnMut(&str, &I::Device) -> Result<T>,
) -> Result<Opened<T>> {
  let default = inputs.default_input();
  let (mut warning, mut tried) = (None, None);
  if let Some(wanted) = wanted {
    let found = match &default {
      Some(d) if d.is_id(wanted) || (wanted.id.is_none() && d.name == wanted.name) => {
        Some(d.clone())
      }
      _ => {
        let all = inputs.inputs();
        let by_id = all.iter().position(|f| f.is_id(wanted));
        let index = by_id.or_else(|| all.iter().position(|f| f.name == wanted.name));
        index.map(|i| all[i].clone())
      }
    };

    match found {
      Some(found) => match open(&found.name, &found.device) {
        Ok(value) => return Ok(Opened { name: found.name, value, warning: None }),
        Err(e) => {
          warning = Some(format!("could not open the microphone {:?}: {e:#}", wanted.name));
          tried = Some(found);
        }
      },
      None => warning = Some(format!("the microphone {:?} is not connected", wanted.name)),
    }
  }

  let default = default.ok_or_else(|| match &warning {
    Some(w) => anyhow!("{w}, and there is no default microphone"),
    None => anyhow!("no microphone found"),
  })?;
  if tried.is_some_and(|tried: Found<I::Device>| tried.device == default.device) {
    // The default is the device that just failed to open: no second attempt.
    return Err(anyhow!(warning.unwrap_or_default()));
  }

  let Found { name, device, .. } = default;

  let value = open(&name, &device).map_err(|e| match &warning {
    Some(w) => anyhow!("{w}, and the default microphone {name:?} failed too: {e:#}"),
    None => anyhow!("could not open the microphone {name:?}: {e:#}"),
  })?;
  let warning = warning.map(|w| format!("{w}; using {name:?}"));
  Ok(Opened { name, value, warning })
}

/// cpal's default host: CoreAudio on macOS, ALSA on Linux.
pub struct Cpal(cpal::Host);

impl Cpal {
  pub fn new() -> Self {
    Self(cpal::default_host())
  }
}

impl Inputs for Cpal {
  type Device = cpal::Device;

  fn inputs(&self) -> Vec<Found<cpal::Device>> {
    match self.0.input_devices() {
      Ok(devices) => devices.filter_map(found).collect(),
      Err(e) => {
        debug!("list input devices: {e}");
        Vec::new()
      }
    }
  }

  fn default_input(&self) -> Option<Found<cpal::Device>> {
    found(self.0.default_input_device()?)
  }
}

/// An input device as `wavo-app devices` shows it.
#[derive(Debug, Clone)]
pub struct Info {
  pub id: Option<String>,
  pub name: String,
  pub default: bool,
  /// Default input format, `None` if the device would not say (e.g. it was just unplugged).
  pub channels: Option<u16>,
  pub rate: Option<u32>,
  pub format: Option<String>,
}

/// Input devices in system order, the system default marked.
pub fn list() -> Vec<Info> {
  let cpal = Cpal::new();
  let default = cpal.default_input().map(|f| f.device);
  cpal
    .inputs()
    .into_iter()
    .map(|Found { id, name, device }| {
      let config = device.default_input_config().ok();
      Info {
        default: default.as_ref() == Some(&device),
        channels: config.as_ref().map(|c| c.channels()),
        rate: config.as_ref().map(|c| c.sample_rate()),
        format: config.map(|c| c.sample_format().to_string()),
        id,
        name,
      }
    })
    .collect()
}

fn found(device: cpal::Device) -> Option<Found<cpal::Device>> {
  let name = match device.description() {
    Ok(d) => d.name().to_owned(),
    Err(e) => {
      debug!("device {device}: {e}"); // disconnected while listing
      return None;
    }
  };

  let id = device.id().map(|id| id.to_string()).ok();
  Some(Found { id, name, device })
}

#[cfg(test)]
mod tests {
  use anyhow::bail;

  use super::*;

  /// Devices as `(id, name)`; `broken` IDs fail to open. `no_ids`: the backend tells no IDs.
  #[derive(Default)]
  struct Fake {
    devices: Vec<(&'static str, &'static str)>,
    default: Option<(&'static str, &'static str)>,
    broken: Vec<&'static str>,
    no_ids: bool,
  }

  type Device = (&'static str, &'static str);

  fn found((id, name): Device) -> Found<Device> {
    Found { id: Some(id.to_owned()), name: name.to_owned(), device: (id, name) }
  }

  impl Inputs for Fake {
    type Device = Device;

    fn inputs(&self) -> Vec<Found<Device>> {
      self.devices.iter().map(|&d| self.found(d)).collect()
    }

    fn default_input(&self) -> Option<Found<Device>> {
      self.default.map(|d| self.found(d))
    }
  }

  impl Fake {
    fn found(&self, device: Device) -> Found<Device> {
      let found = found(device);
      Found { id: found.id.filter(|_| !self.no_ids), ..found }
    }

    fn open(&self, wanted: Option<&Choice>) -> Result<Opened<Device>> {
      open(self, wanted, |name, &device| {
        assert_eq!(name, device.1);
        if self.broken.contains(&device.0) {
          bail!("device busy");
        }

        Ok(device)
      })
    }
  }

  const BUILT_IN: Device = ("BuiltInMicrophoneDevice", "MacBook Pro Microphone");
  const RODE: Device = ("AppleUSBAudioEngine:RØDE:PodMic:1", "RØDE PodMic USB");
  /// A second microphone of the same model: same name, another ID.
  const RODE_2: Device = ("AppleUSBAudioEngine:RØDE:PodMic:2", "RØDE PodMic USB");

  fn choice((id, name): Device) -> Choice {
    Choice { id: Some(id.into()), name: name.into() }
  }

  fn by_name((_, name): Device) -> Choice {
    Choice { id: None, name: name.into() }
  }

  fn laptop() -> Fake {
    Fake { devices: vec![BUILT_IN], default: Some(BUILT_IN), ..Fake::default() }
  }

  #[test]
  fn opens_the_chosen_device() {
    let fake = Fake { devices: vec![BUILT_IN, RODE], ..laptop() };
    let opened = fake.open(Some(&choice(RODE))).unwrap();
    assert_eq!((opened.name.as_str(), opened.value, opened.warning), (RODE.1, RODE, None));
    let opened = fake.open(Some(&by_name(RODE))).unwrap();
    assert_eq!(opened.value, RODE, "a choice without an ID matches by name");
  }

  #[test]
  fn the_id_wins_over_the_name() {
    let fake = Fake { devices: vec![BUILT_IN, RODE, RODE_2], default: Some(RODE), ..laptop() };
    assert_eq!(fake.open(Some(&choice(RODE_2))).unwrap().value, RODE_2);
    assert_eq!(fake.open(Some(&choice(RODE))).unwrap().value, RODE);

    // The ID is unknown (e.g. it changed): the name still finds the device.
    let renamed = Choice { id: Some("gone".into()), name: RODE.1.into() };
    let fake = Fake { devices: vec![BUILT_IN, RODE], ..laptop() };
    let opened = fake.open(Some(&renamed)).unwrap();
    assert_eq!((opened.value, opened.warning), (RODE, None));

    // The command line gives one string: an ID or a name.
    assert_eq!(fake.open(Some(&Choice::id_or_name(RODE.0))).unwrap().value, RODE);
    assert_eq!(fake.open(Some(&Choice::id_or_name(RODE.1))).unwrap().value, RODE);
  }

  #[test]
  fn no_choice_opens_the_default_without_warning() {
    let opened = laptop().open(None).unwrap();
    assert_eq!((opened.value, opened.warning), (BUILT_IN, None));
  }

  #[test]
  fn missing_device_falls_back_to_the_default_with_a_warning() {
    let wanted = Some(choice(RODE));
    let opened = laptop().open(wanted.as_ref()).unwrap();
    assert_eq!(opened.value, BUILT_IN);
    let warning = opened.warning.unwrap();
    assert!(warning.contains(RODE.1) && warning.contains("not connected"), "{warning}");
    assert!(warning.contains(BUILT_IN.1), "{warning}");
    assert_eq!(wanted, Some(choice(RODE)), "the choice stays");
  }

  #[test]
  fn device_that_fails_to_open_falls_back_too() {
    let fake = Fake { devices: vec![BUILT_IN, RODE], broken: vec![RODE.0], ..laptop() };
    let opened = fake.open(Some(&choice(RODE))).unwrap();
    assert_eq!(opened.value, BUILT_IN);
    let warning = opened.warning.unwrap();
    assert!(warning.contains("device busy") && warning.contains(BUILT_IN.1), "{warning}");

    // The default is the twin of the broken one: same name, another device.
    let twins =
      Fake { devices: vec![RODE, RODE_2], default: Some(RODE), broken: vec![RODE_2.0], ..laptop() };
    let opened = twins.open(Some(&choice(RODE_2))).unwrap();
    assert_eq!(opened.value, RODE);
    assert!(opened.warning.unwrap().contains("device busy"));

    // Without IDs from the backend the twins are still two devices.
    let twins = Fake {
      devices: vec![RODE_2, RODE],
      default: Some(RODE),
      broken: vec![RODE_2.0],
      no_ids: true,
    };
    let opened = twins.open(Some(&choice(RODE_2))).unwrap();
    assert_eq!(opened.value, RODE);
    assert!(opened.warning.unwrap().contains("device busy"));
  }

  #[test]
  fn failures_without_a_working_default_are_errors() {
    let none = Fake::default();
    assert!(none.open(None).unwrap_err().to_string().contains("no microphone"));
    let err = none.open(Some(&choice(RODE))).unwrap_err().to_string();
    assert!(err.contains("not connected") && err.contains("no default"), "{err}");

    let broken_default = Fake { broken: vec![BUILT_IN.0], ..laptop() };
    let err = broken_default.open(Some(&choice(RODE))).unwrap_err().to_string();
    assert!(err.contains("failed too"), "{err}");

    // The chosen device is the default and is broken: one attempt, its own error.
    let err = broken_default.open(Some(&choice(BUILT_IN))).unwrap_err().to_string();
    assert!(err.contains("device busy"), "{err}");
  }
}
