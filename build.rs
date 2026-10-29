// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The build script: `assets/icon.png` as the app's icons in `$OUT_DIR`, raw RGBA for nimbo, which
//! decodes no PNG:
//! - `about-icon.rgba`: 128 px (64 points at 2x), the About page's ([nimbo::Ui::rgba_image]);
//! - `window-icon.rgba`: 256 px, the windows' off macOS ([nimbo::Config::icon]), [cropped] to the
//!   tile: Apple's margin around it would show it small.
//!
//! And the version the app shows, `WAVO_VERSION` ([version]).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

const ICON: &str = "assets/icon.png";

fn main() {
  println!("cargo:rerun-if-changed={ICON}");
  println!("cargo:rustc-env=WAVO_VERSION={}", version());
  let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
  let (w, h, rgba) = decode(ICON);
  write(&out.join("about-icon.rgba"), &resized(w, h, &rgba, 128));
  let (side, tile) = cropped(w, h, &rgba);
  write(&out.join("window-icon.rgba"), &resized(side, side, &tile, 256));
}

/// The version the build shows, as CI's dev builds have it (vladkens/actions dev-version): Cargo's
/// when CI stamped it already (a pre-release part); else `git describe` against the last vX.Y.Z
/// tag, `0.9.0-12-g3672a1b` after one, `0.1.0-g3672a1b` before any, the plain version on its tag;
/// `-dirty` with uncommitted changes; Cargo's without git (a crate build).
fn version() -> String {
  let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
  if pkg.contains('-') {
    return pkg;
  }

  let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else { return pkg };
  rerun_on_git();
  let described = git(&["describe", "--tags", "--match", "v[0-9]*.[0-9]*.[0-9]*"]);
  let version = match described {
    Some(d) => d.trim_start_matches('v').to_owned(),
    None => format!("{pkg}-g{sha}"),
  };

  let dirty =
    git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
  if dirty { format!("{version}-dirty") } else { version }
}

/// `git` in the package's folder: its trimmed output, `None` when it fails or there is no git.
fn git(args: &[&str]) -> Option<String> {
  let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
  Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The version again when HEAD moves, a tag comes or the tree changes: HEAD and the index (in a
/// worktree's own git dir), the ref HEAD names and the tags (in the shared one), and the sources.
fn rerun_on_git() {
  let dir = |arg| git(&["rev-parse", arg]).map(PathBuf::from);
  let (Some(own), Some(shared)) = (dir("--absolute-git-dir"), dir("--git-common-dir")) else {
    return;
  };

  let shared = std::env::current_dir().map(|cwd| cwd.join(&shared)).unwrap_or(shared);
  let head = git(&["rev-parse", "--symbolic-full-name", "HEAD"]).filter(|r| r.starts_with("refs/"));
  let mut paths = vec![own.join("HEAD"), own.join("index"), shared.join("packed-refs")];
  paths.push(shared.join("refs/tags"));
  paths.extend(head.map(|r| shared.join(r)));
  paths.extend(["src", "Cargo.toml", "Cargo.lock"].map(PathBuf::from));
  // A path that isn't there would run this every build.
  for path in paths.iter().filter(|p| p.exists()) {
    println!("cargo:rerun-if-changed={}", path.display());
  }
}

fn write(path: &Path, data: &[u8]) {
  fs::write(path, data).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// The PNG at `path` as `(width, height, RGBA)`, 8 bits a channel, not premultiplied.
fn decode(path: &str) -> (usize, usize, Vec<u8>) {
  let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
  let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
  decoder.set_transformations(png::Transformations::STRIP_16 | png::Transformations::ALPHA);
  let mut reader = decoder.read_info().expect("a PNG");
  let mut rgba = vec![0; reader.output_buffer_size().expect("its size")];
  let info = reader.next_frame(&mut rgba).expect("its pixels");
  assert_eq!(info.color_type, png::ColorType::Rgba, "{path}: not RGB(A)");
  rgba.truncate(info.buffer_size());
  (info.width as usize, info.height as usize, rgba)
}

/// `n` × `n` pixels, each the average of the source pixels it covers, weighted by their alpha:
/// averaging straight colours would bleed the transparent black.
/// `rgba` cut to the square around its visible pixels, centred, a side and its pixels: macOS icons
/// keep a transparent margin around their tile (about a tenth on each side), Windows' taskbar and
/// Linux docks show the image as it is.
fn cropped(w: usize, h: usize, rgba: &[u8]) -> (usize, Vec<u8>) {
  let visible = |x: usize, y: usize| rgba[(y * w + x) * 4 + 3] > 8;
  let xs: Vec<usize> = (0..w).filter(|&x| (0..h).any(|y| visible(x, y))).collect();
  let ys: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| visible(x, y))).collect();
  let (Some(&x0), Some(&x1), Some(&y0), Some(&y1)) = (xs.first(), xs.last(), ys.first(), ys.last())
  else {
    return (w.max(h), resized(w, h, rgba, w.max(h)));
  };

  let side = (x1 - x0).max(y1 - y0) + 1;
  // The square's corner, the tile centred in it; outside the image is transparent.
  let left = (x0 + x1 + 1) as isize / 2 - side as isize / 2;
  let top = (y0 + y1 + 1) as isize / 2 - side as isize / 2;
  let mut tile = vec![0; side * side * 4];
  for y in 0..side {
    for x in 0..side {
      let (sx, sy) = (left + x as isize, top + y as isize);
      if sx < 0 || sy < 0 || sx >= w as isize || sy >= h as isize {
        continue;
      }

      let from = (sy as usize * w + sx as usize) * 4;
      tile[(y * side + x) * 4..][..4].copy_from_slice(&rgba[from..from + 4]);
    }
  }

  (side, tile)
}

fn resized(w: usize, h: usize, rgba: &[u8], n: usize) -> Vec<u8> {
  let mut sums = vec![[0f64; 5]; n * n];
  for (i, p) in rgba.chunks(4).enumerate() {
    let sum = &mut sums[i / w * n / h * n + i % w * n / w];
    let a = f64::from(p[3]);
    (0..3).for_each(|c| sum[c] += f64::from(p[c]) * a);
    sum[3] += a;
    sum[4] += 1.0;
  }

  (sums.iter())
    .flat_map(|s| {
      let color = |c: f64| if s[3] > 0.0 { (c / s[3]).round() as u8 } else { 0 };
      [color(s[0]), color(s[1]), color(s[2]), (s[3] / s[4]).round() as u8]
    })
    .collect()
}
