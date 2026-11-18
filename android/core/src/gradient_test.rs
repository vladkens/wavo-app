// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

//! The shared gradient as the core builds it: the same pixels the desktop's HUD draws, and the
//! rim as a layer of its own rather than something the Kotlin side paints again.

use std::time::Duration;

use crate::gradient::{self, Size};

const PILL: Size = Size { w: 176.0, h: 36.0 };

#[test]
fn field_is_the_pills_pixels() {
  let (w, h, px) = gradient::field(PILL, Duration::ZERO, 1.0);
  assert_eq!((w, h), (176, 36));
  assert_eq!(px.len(), 176 * 36 * 4);
  assert_eq!(px, gradient::field(PILL, Duration::ZERO, 1.0).2);
}

#[test]
fn edge_is_white_with_a_varying_alpha() {
  let (w, h, px) = gradient::edge(PILL, 1.0);
  assert_eq!((w, h), (176, 36));
  assert_eq!(px.len(), 176 * 36 * 4);
  for p in px.chunks(4) {
    assert_eq!([p[0], p[1], p[2]], [255, 255, 255], "the rim is white");
  }
  assert!(px.chunks(4).any(|p| p[3] == 0), "the rim must not cover the fill");
  // The rim is white over the fill at RIM.0 = 0.5 at most, so it is glass, never opaque.
  let most = px.chunks(4).map(|p| p[3]).max().unwrap();
  assert!(most > 0, "there must be a rim");
  assert!(most <= 128, "the rim is translucent, not a white outline: {most}");
}

#[test]
fn scale_multiplies_the_pixels() {
  let (w, h, px) = gradient::field(PILL, Duration::ZERO, 2.0);
  assert_eq!((w, h), (352, 72));
  assert_eq!(px.len(), 352 * 72 * 4);
}
