// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The build script: `assets/icon.png` as the app's icons in `$OUT_DIR`, raw RGBA for nimbo, which
//! decodes no PNG:
//! - `about-icon.rgba`: 128 px (64 points at 2x), the About page's ([nimbo::Ui::rgba_image]);
//! - `window-icon.rgba`: 256 px, the windows' off macOS ([nimbo::Config::icon]);
//! - for Windows, `wavo.ico` linked into the exe as its icon, the one Explorer shows.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

const ICON: &str = "assets/icon.png";
/// The sizes in the exe's icon: Explorer's views and the taskbar at 100–200% scale.
const ICO: [usize; 6] = [16, 24, 32, 48, 64, 256];

fn main() {
  println!("cargo:rerun-if-changed={ICON}");
  let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
  let (w, h, rgba) = decode(ICON);
  write(&out.join("about-icon.rgba"), &resized(w, h, &rgba, 128));
  write(&out.join("window-icon.rgba"), &resized(w, h, &rgba, 256));

  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    exe_icon(&out, w, h, &rgba);
  }
}

/// `wavo.ico` and a resource script with it as the first icon, which Windows takes for the exe's,
/// compiled and linked into the binary (rc.exe, or llvm-rc when cross-compiling).
fn exe_icon(out: &Path, w: usize, h: usize, rgba: &[u8]) {
  let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
  for n in ICO {
    let image = ico::IconImage::from_rgba_data(n as u32, n as u32, resized(w, h, rgba, n));
    dir.add_entry(ico::IconDirEntry::encode(&image).expect("an icon image"));
  }

  let path = out.join("wavo.ico");
  dir.write(File::create(&path).expect("wavo.ico")).expect("write wavo.ico");

  // UTF-8, as rc.exe reads the ANSI code page otherwise: a path with an accented user name.
  let path = path.display().to_string().replace('\\', "/");
  let rc = out.join("wavo.rc");
  write(&rc, format!("#pragma code_page(65001)\n1 ICON \"{path}\"\n").as_bytes());
  embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("wavo.rc");
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
