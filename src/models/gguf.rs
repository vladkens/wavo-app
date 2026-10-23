// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Just enough of the GGUF layout to tell a speech model from any other file without loading it.

use std::fs::File;
use std::io::{self, BufReader, ErrorKind, Read, Seek};
use std::path::Path;

/// `general.architecture` values wavo transcribes with (`wavo::Model::load`).
const SPEECH_ARCHS: &[&str] = &["gigaam", "moonshine", "parakeet", "whisper"];

/// Header key/value pairs to look through; `general.architecture` comes first in practice.
const MAX_KEYS: u64 = 64;

const TYPE_STRING: u32 = 8;
const TYPE_ARRAY: u32 = 9;

/// The model's architecture if `path` is a speech model wavo can load, else `None`.
pub fn speech_arch(path: &Path) -> io::Result<Option<String>> {
  let arch = read_arch(&mut BufReader::new(File::open(path)?))?;
  Ok(arch.filter(|a| SPEECH_ARCHS.contains(&a.as_str())))
}

/// `general.architecture` of a GGUF file, else `None`. Reads nothing past it: later keys hold
/// Whisper's ~50k tokens.
fn read_arch(r: &mut (impl Read + Seek)) -> io::Result<Option<String>> {
  let mut magic = [0u8; 4];
  if let Err(e) = r.read_exact(&mut magic) {
    return if e.kind() == ErrorKind::UnexpectedEof { Ok(None) } else { Err(e) };
  }

  // Version 1 used 32-bit counts and lengths; nothing current writes it.
  if &magic != b"GGUF" || read_u32(r)? < 2 {
    return Ok(None);
  }

  let _tensors = read_u64(r)?;
  let keys = read_u64(r)?;
  for _ in 0..keys.min(MAX_KEYS) {
    let key = read_string(r)?;
    let kind = read_u32(r)?;
    if key == "general.architecture" && kind == TYPE_STRING {
      return Ok(Some(read_string(r)?).filter(|a| !a.is_empty()));
    }

    skip_value(r, kind)?;
  }

  Ok(None)
}

fn skip_value(r: &mut (impl Read + Seek), kind: u32) -> io::Result<()> {
  match kind {
    TYPE_STRING => {
      let len = read_u64(r)?;
      r.seek_relative(len as i64)
    }
    TYPE_ARRAY => {
      let item = read_u32(r)?;
      let count = read_u64(r)?;
      match scalar_size(item) {
        Some(size) => r.seek_relative((count * size) as i64),
        None => (0..count).try_for_each(|_| skip_value(r, item)),
      }
    }
    _ => match scalar_size(kind) {
      Some(size) => r.seek_relative(size as i64),
      None => Err(io::Error::new(ErrorKind::InvalidData, format!("unknown GGUF type {kind}"))),
    },
  }
}

fn scalar_size(kind: u32) -> Option<u64> {
  match kind {
    0 | 1 | 7 => Some(1), // u8, i8, bool
    2 | 3 => Some(2),     // u16, i16
    4..=6 => Some(4),     // u32, i32, f32
    10..=12 => Some(8),   // u64, i64, f64
    _ => None,
  }
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
  let mut b = [0u8; 4];
  r.read_exact(&mut b)?;
  Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
  let mut b = [0u8; 8];
  r.read_exact(&mut b)?;
  Ok(u64::from_le_bytes(b))
}

fn read_string(r: &mut impl Read) -> io::Result<String> {
  let len = read_u64(r)?;
  if len > 1 << 16 {
    return Err(io::Error::new(ErrorKind::InvalidData, format!("GGUF string of {len} bytes")));
  }

  let mut buf = vec![0; len as usize];
  r.read_exact(&mut buf)?;
  String::from_utf8(buf).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
}

/// A minimal GGUF v3 header: the given key/value pairs, no tensors.
#[cfg(test)]
fn test_header(kvs: &[(&str, u32, &[u8])]) -> Vec<u8> {
  let mut out = b"GGUF".to_vec();
  out.extend_from_slice(&3u32.to_le_bytes());
  out.extend_from_slice(&0u64.to_le_bytes());
  out.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
  for (key, kind, value) in kvs {
    out.extend_from_slice(&(key.len() as u64).to_le_bytes());
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(value);
  }

  out
}

/// A GGUF string value.
#[cfg(test)]
fn gguf_string(s: &str) -> Vec<u8> {
  let mut out = (s.len() as u64).to_le_bytes().to_vec();
  out.extend_from_slice(s.as_bytes());
  out
}

/// The header of a speech model of architecture `arch`.
#[cfg(test)]
pub fn speech_header(arch: &str) -> Vec<u8> {
  test_header(&[("general.architecture", TYPE_STRING, &gguf_string(arch))])
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  fn arch_of(bytes: Vec<u8>) -> Option<String> {
    read_arch(&mut Cursor::new(bytes)).unwrap()
  }

  fn string_array(items: &[&str]) -> Vec<u8> {
    let mut out = TYPE_STRING.to_le_bytes().to_vec();
    out.extend_from_slice(&(items.len() as u64).to_le_bytes());
    for item in items {
      out.extend(gguf_string(item));
    }

    out
  }

  #[test]
  fn finds_the_architecture_among_other_keys() {
    let mut floats = 6u32.to_le_bytes().to_vec();
    floats.extend_from_slice(&3u64.to_le_bytes());
    floats.extend_from_slice(&[0; 12]);
    let header = test_header(&[
      ("general.alignment", 4, &32u32.to_le_bytes()),
      ("tokenizer.tokens", TYPE_ARRAY, &string_array(&["привет", "мир"])),
      ("mel.filters", TYPE_ARRAY, &floats),
      ("stt.capability.lang_detect", 7, &[1]),
      ("general.name", TYPE_STRING, &gguf_string("Whisper")),
      ("general.architecture", TYPE_STRING, &gguf_string("whisper")),
    ]);
    assert_eq!(arch_of(header).as_deref(), Some("whisper"));
    assert_eq!(arch_of(speech_header("gigaam")).as_deref(), Some("gigaam"));
  }

  #[test]
  fn ggml_bin_and_foreign_files_are_no_models() {
    assert_eq!(arch_of(b"lmgg\x9a\xca\0\0".to_vec()), None, "legacy whisper.cpp .bin");
    assert_eq!(arch_of(b"PK\x03\x04 a zip".to_vec()), None);
    assert_eq!(arch_of(b"GG".to_vec()), None);
    assert_eq!(arch_of(test_header(&[("general.name", TYPE_STRING, &gguf_string("x"))])), None);
  }

  #[test]
  fn truncated_header_is_an_error() {
    let mut header = speech_header("whisper");
    header.truncate(header.len() - 3);
    assert!(read_arch(&mut Cursor::new(header)).is_err());

    let mut header = test_header(&[
      ("general.architecture", TYPE_STRING, &gguf_string("whisper")),
      ("tokenizer.tokens", TYPE_ARRAY, &string_array(&["a", "b"])),
    ]);
    header.truncate(header.len() - 3);
    assert_eq!(arch_of(header).as_deref(), Some("whisper"), "nothing past the architecture");
  }
}
