// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The build script: `$OUT_DIR/about-icon.rgba`, `assets/icon.png` at 128 px (64 points at 2x) as
//! raw RGBA, the About page's ([nimbo::Ui::rgba_image] takes pixels, nimbo decodes no PNG).

use std::path::PathBuf;

const ICON: &str = "assets/icon.png";
/// The About icon's side in pixels.
const PX: usize = 128;

fn main() {
  println!("cargo:rerun-if-changed={ICON}");
  let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
  let (w, h, rgba) = decode(ICON);
  let about = out.join("about-icon.rgba");
  std::fs::write(&about, resized(w, h, &rgba, PX))
    .unwrap_or_else(|e| panic!("write {}: {e}", about.display()));
}

/// The PNG at `path` as `(width, height, RGBA)`, 8 bits a channel, not premultiplied.
fn decode(path: &str) -> (usize, usize, Vec<u8>) {
  let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
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
