// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The microphone's permission (AVFoundation): whether the app may record, and the system's
//! prompt that asks the person.

use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
  Authorized,
  Denied,
  Restricted,
  NotDetermined,
}

pub fn access() -> Access {
  // SAFETY: a static of the framework; audio is a valid media type here.
  let status =
    unsafe { AVCaptureDevice::authorizationStatusForMediaType(AVMediaTypeAudio.unwrap()) };
  match status {
    AVAuthorizationStatus::Authorized => Access::Authorized,
    AVAuthorizationStatus::Denied => Access::Denied,
    AVAuthorizationStatus::Restricted => Access::Restricted,
    _ => Access::NotDetermined,
  }
}

/// Shows the system's prompt; the answer arrives later (the next [access] sees it).
pub fn request() {
  info!("asking for microphone access");
  let done = block2::RcBlock::new(|granted: objc2::runtime::Bool| {
    info!("microphone access {}", if granted.as_bool() { "granted" } else { "denied" });
  });
  // SAFETY: a static of the framework; the block is copied by the callee.
  unsafe {
    AVCaptureDevice::requestAccessForMediaType_completionHandler(AVMediaTypeAudio.unwrap(), &done)
  };
}
