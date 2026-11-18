// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The HUD pill's colours, shared with the Android app: the drifting clouds under a dictation and
//! the glass rim over them. No nimbo in here, so the Android core builds it as it is.

use std::f32::consts::TAU;
use std::sync::LazyLock;
use std::time::Duration;

/// A pill's size in points. nimbo's own `Size` is this and no more, so the core needs no nimbo.
#[derive(Clone, Copy)]
pub struct Size {
  pub w: f32,
  pub h: f32,
}

pub const WIDTH: f32 = 176.0;
pub const HEIGHT: f32 = 36.0;
// The live text's line and its padding at the far edge: [corner] rounds the pill less as it opens
// for them, so they are here with it, and the HUD imports them back.
pub const LINE: f32 = 18.0;
pub const TEXT_EDGE: f32 = 10.0;
const OPEN_RADIUS: f32 = 15.0;

// The icon's colours drift by as soft clouds: three wide blobs side by side, each a colour, mixed
// by nearness, so the pill shows three at a time with no edge between them. A dictation takes a
// few seconds, so it all moves within the first two: the colours go round in this long, each
// cloud this far along them from the one before, together a good half of them.
const HUES_PERIOD: f64 = 9.0;
const HUES_APART: f32 = 0.3;
// A cloud's reach (a Gaussian's sigma); how far apart they sit, and how far from there they
// wander: across and up and down, in these periods (seconds), unalike, so they never line up the
// same way twice.
const CLOUD: f32 = 0.3 * WIDTH;
const SPREAD: f32 = WIDTH / 3.0;
const WANDER: (f32, f32) = (0.2 * WIDTH, 0.5 * HEIGHT);
const CLOUDS: [(f64, f64); 3] = [(6.1, 4.3), (7.3, 5.3), (8.9, 6.7)];
// Between the clouds the colour deepens, this much at most: the clouds' depth.
pub const DEEPEN: f32 = 0.25;
// The icon's colours (assets/icon.png), calmed, in OKLCH (lightness, chroma, hue): a soft orchid
// for its pink, its violet, indigo and blue, and back by a periwinkle and an iris, no two
// neighbours far apart. All about as light, dark enough for white text (WCAG AA's 4.5:1 with
// room), and as colourful: chroma 0.15 to 0.18, the orchid's less, as a warm hue glares sooner;
// the icon's own sit at sRGB's edge, where its pinks glare. Not its cyan: dark enough for white
// text, it is a muddy teal.
const BRAND: [[f32; 3]; 6] = [
  [0.55, 0.13, 328.0],
  [0.54, 0.17, 302.0],
  [0.53, 0.18, 282.0],
  [0.55, 0.155, 260.0],
  [0.54, 0.165, 272.0],
  [0.54, 0.17, 293.0],
];
// The edge, a glass pill lit from above: white over the fill a point inside the pill, at its top
// and its bottom, so the rim is the fill's own colour, lighter, and no dark line, seen on any
// wallpaper; and a glow down from the top, its strength. An even rim: (0.35, 0.35) and 0.0.
const RIM: (f32, f32) = (0.5, 0.08);
const GLOW: f32 = 0.16;

pub fn lerp(a: f32, b: f32, k: f32) -> f32 {
  a + (b - a) * k
}

/// The gradient of a pill `size` at `t` in its flow: straight RGBA rows, a pixel per
/// pixel at `scale`, drawn 1:1. A pixel's colour is the clouds' (see [CLOUDS]) mixed by nearness,
/// a Gaussian's weight each: a place among [SHADES], so every pixel is one of the icon's colours,
/// never a grey mix of two, and the change from cloud to cloud is as soft as the clouds are wide.
/// Dithered, so a slow change shows no bands. Its edge is the pill's, smoothed as a quad's.
pub fn field(size: Size, t: Duration, scale: f32) -> (usize, usize, Vec<u8>) {
  // Of their periods, which they wrap at unseen: f32 keeps its precision over days on.
  let of = |period: f64| (t.as_secs_f64() % period / period) as f32;
  let hues = of(HUES_PERIOD);
  // From the pill's middle, so the colours stay put as it narrows.
  let clouds = CLOUDS.iter().enumerate().map(|(n, &(across, up))| {
    let phase = n as f32 / CLOUDS.len() as f32;
    let home = size.w / 2.0 + (n as f32 - 1.0) * SPREAD;
    let x = home + WANDER.0 * (TAU * (of(across) + phase)).sin();
    let y = size.h / 2.0 + WANDER.1 * (TAU * (of(up) + phase)).sin();
    (x, y, hues + n as f32 * HUES_APART)
  });
  let clouds: Vec<_> = clouds.collect();

  pixels(size, scale, |(i, j), (x, y)| {
    // The clouds' cores, their weights squared, are narrower: the depth.
    let (mut sum, mut hue, mut core) = (0.0, 0.0, 0.0);
    for &(cx, cy, h) in &clouds {
      let w = (-((x - cx).powi(2) + (y - cy).powi(2)) / (2.0 * CLOUD * CLOUD)).exp();
      (sum, hue, core) = (sum + w, hue + w * h, core + w * w);
    }

    let shade = SHADES[((hue / sum).rem_euclid(1.0) * SHADES.len() as f32) as usize % SHADES.len()];
    let deep = 1.0 - DEEPEN * (1.0 - core.min(1.0));
    // Interleaved gradient noise (Jimenez), half a step either way, still from frame to frame.
    let noise = (52.982_918 * (0.067_110_56 * i as f32 + 0.005_837_15 * j as f32).fract()).fract();
    let [red, green, blue] =
      shade.map(|v| (v * deep + noise - 0.5).round().clamp(0.0, 255.0) as u8);
    [red, green, blue, 255]
  })
}

/// The pill's edge, white over its fill (see [RIM] and [GLOW]), as [field]'s pixels.
pub fn edge(size: Size, scale: f32) -> (usize, usize, Vec<u8>) {
  pixels(size, scale, |_, (x, y)| {
    // A point wide, its inner side smoothed too.
    let rim = ((1.0 - inside(size, x, y)) * scale + 0.5).clamp(0.0, 1.0);
    let rim = rim * lerp(RIM.1, RIM.0, 1.0 - y / size.h);
    let glow = GLOW * (1.0 - y / (0.4 * HEIGHT)).max(0.0).powi(2);
    [255, 255, 255, (rim.max(glow) * 255.0).round() as u8]
  })
}

/// The straight RGBA rows of an image of a pill `size` in points, a pixel per pixel at `scale`:
/// `pixel` of each one's column and row and its middle in points, its alpha cut to the pill as a
/// quad's edge is, so the two meet with no seam.
fn pixels(
  size: Size,
  scale: f32,
  mut pixel: impl FnMut((usize, usize), (f32, f32)) -> [u8; 4],
) -> (usize, usize, Vec<u8>) {
  let (cols, rows) = ((size.w * scale).ceil() as usize, (size.h * scale).round() as usize);
  let at = |i: usize| (i as f32 + 0.5) / scale;
  let mut px = Vec::with_capacity(cols * rows * 4);
  for j in 0..rows {
    for i in 0..cols {
      let (x, y) = (at(i), at(j));
      let [red, green, blue, a] = pixel((i, j), (x, y));
      let cover = (0.5 + inside(size, x, y) * scale).clamp(0.0, 1.0);
      px.extend_from_slice(&[red, green, blue, (a as f32 * cover).round() as u8]);
    }
  }

  (cols, rows, px)
}

/// How far a point is inside a pill of `size`, its corners [corner], in points: below 0 outside.
fn inside(size: Size, x: f32, y: f32) -> f32 {
  let r = corner(size.h);
  let qx = (x - size.w / 2.0).abs() - (size.w / 2.0 - r);
  let qy = (y - size.h / 2.0).abs() - (size.h / 2.0 - r);
  r - qx.max(0.0).hypot(qy.max(0.0)) - qx.max(qy).min(0.0)
}

/// The corners' radius of a pill `height` tall: round ends, less round as it opens for a line.
pub fn corner(height: f32) -> f32 {
  lerp(HEIGHT / 2.0, OPEN_RADIUS, ((height - HEIGHT) / (LINE + TEXT_EDGE)).clamp(0.0, 1.0))
}

/// [BRAND] round, from 0 to 1, mixed in OKLCH, 0..255 unrounded for the dither: a lookup per
/// pixel.
pub static SHADES: LazyLock<Vec<[f32; 3]>> =
  LazyLock::new(|| (0..4096).map(|k| srgb(brand(k as f32 / 4096.0))).collect());

/// [BRAND] round at `t`, 0..1 (the last back to the first), mixed in OKLCH: a Catmull-Rom spline
/// through their lightness, chroma and hue, the hue the short way round, so no grey between two
/// colours and no crease at a stop.
pub fn brand(t: f32) -> [f32; 3] {
  let n = BRAND.len();
  let at = t.rem_euclid(1.0) * n as f32;
  let (i, k) = (at as usize % n, at.fract());
  // The stops around the segment.
  let mut p = [0, 1, 2, 3].map(|m| BRAND[(i + n + m - 1) % n]);
  for m in 1..4 {
    p[m][2] = p[m - 1][2] + (p[m][2] - p[m - 1][2] + 540.0) % 360.0 - 180.0;
  }

  let spline = |v: [f32; 4]| {
    let (a, b, c) = (v[2] - v[0], 2.0 * v[0] - 5.0 * v[1] + 4.0 * v[2] - v[3], v[3] - v[0]);
    v[1] + 0.5 * k * (a + k * (b + k * (c + 3.0 * (v[1] - v[2]))))
  };
  let [l, c, h] = [0, 1, 2].map(|n| spline(p.map(|c| c[n])));
  [l, c.max(0.0), h]
}

/// An OKLCH colour in sRGB, 0..255, its chroma cut to the most that fits: found by halving, so
/// colours side by side cut alike, without steps.
fn srgb([l, c, h]: [f32; 3]) -> [f32; 3] {
  let (sin, cos) = h.to_radians().sin_cos();
  let linear = |c: f32| {
    let (a, b) = (c * cos, c * sin);
    let l_ = (l + 0.3963378 * a + 0.2158038 * b).powi(3);
    let m_ = (l - 0.1055613 * a - 0.0638542 * b).powi(3);
    let s_ = (l - 0.0894842 * a - 1.2914855 * b).powi(3);
    [
      4.0767417 * l_ - 3.3077116 * m_ + 0.2309699 * s_,
      -1.268438 * l_ + 2.6097574 * m_ - 0.3413194 * s_,
      -0.0041961 * l_ - 0.7034186 * m_ + 1.7076147 * s_,
    ]
  };
  let fits = |c| linear(c).iter().all(|v| (-1e-4..=1.0 + 1e-4).contains(v));
  let (mut lo, mut hi) = if fits(c) { (c, c) } else { (0.0, c) };
  while hi - lo > 1e-5 {
    let mid = (lo + hi) / 2.0;
    if fits(mid) { lo = mid } else { hi = mid }
  }

  linear(lo).map(|v| {
    let v = v.clamp(0.0, 1.0);
    255.0 * if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  const PILL: Size = Size { w: WIDTH, h: HEIGHT };

  #[test]
  fn field_is_the_pills_pixels() {
    let (w, h, px) = field(PILL, Duration::ZERO, 1.0);
    assert_eq!((w, h), (176, 36));
    assert_eq!(px.len(), 176 * 36 * 4);
  }

  #[test]
  fn field_is_still_at_one_time_and_moves_at_another() {
    let a = field(PILL, Duration::ZERO, 1.0).2;
    let b = field(PILL, Duration::ZERO, 1.0).2;
    assert_eq!(a, b, "the same time must give the same pixels");
    let c = field(PILL, Duration::from_secs(1), 1.0).2;
    assert_ne!(a, c, "the clouds must have moved");
  }

  #[test]
  fn field_is_opaque_inside_the_pill() {
    let (_, _, px) = field(PILL, Duration::ZERO, 1.0);
    for i in 20..156 {
      let at = (18 * 176 + i) * 4;
      assert_eq!(px[at + 3], 255, "pixel {i} of the middle row is not opaque");
    }
  }

  #[test]
  fn brand_stops_are_in_gamut() {
    for stop in BRAND {
      for v in srgb(stop) {
        assert!((0.0..=255.0).contains(&v), "{v} is outside sRGB");
      }
    }
  }
}
