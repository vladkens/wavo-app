// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Engine tests with real models: `make test-models`, which makes the fixtures first. Every test
//! warms up (page cache, first run) before it measures anything. The fake engine's tests are in
//! [`super::supervisor`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::model::Wavo;
use super::supervisor::{Engine, EngineConfig, EngineError, Phase, Transcription};
use crate::audio::vad::Clip;
use crate::testing::{
  Fault, Faults, Fixture, fixture, model_en, model_parakeet, model_ru, paragraph_recall, wer,
};

const MAX_WER: f64 = 0.15;
/// Share of each paragraph's words that a long recording must keep.
const MIN_PARAGRAPH_RECALL: f64 = 0.7;
/// Warm GigaAM Q8 on Metal, submission to text through the supervisor: about 2 × the medians
/// measured on an M2.
const WARM_SHORT_LIMIT: Duration = Duration::from_millis(100);
const WARM_TYPICAL_LIMIT: Duration = Duration::from_millis(200);
/// Warm thread start → loaded of GigaAM v3 e2e RNNT Q8_0 on Metal: 2 × the median measured on an
/// M2.
const WARM_LOAD_LIMIT: Duration = Duration::from_millis(330);

fn run(engine: &Engine, f: &Fixture, lang: Option<&str>) -> Result<Transcription, EngineError> {
  engine.transcribe(f.input().pcm, lang.map(str::to_owned)).wait()
}

fn assert_wer(f: &Fixture, out: &Transcription) {
  let w = wer(&f.text, &out.text);
  let (percent, max) = (w * 100.0, MAX_WER * 100.0);
  assert!(w <= MAX_WER, "{}: WER {percent:.1}% > {max:.0}%: {:?}", f.name, out.text);
}

fn model_name(path: &Path) -> String {
  path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// Transcribes each fixture after a warm-up run and checks its WER; prints WER and timings.
fn check_wer(model: PathBuf, cases: &[(&str, Option<&str>)]) {
  let name = model_name(&model);
  let engine = Engine::start(EngineConfig::new(model)).unwrap();
  let (warm, lang) = (fixture(cases[0].0), cases[0].1);
  let cold = run(&engine, &warm, lang).unwrap();
  let load = engine.status().last_load.unwrap();
  eprintln!("{name}: cold first run {:.2?} (thread start → loaded {load:.2?})", cold.elapsed);

  let mut failed = Vec::new();
  for &(fixture_name, lang) in cases {
    let f = fixture(fixture_name);
    let out = run(&engine, &f, lang).unwrap();
    let w = wer(&f.text, &out.text);
    eprintln!(
      "{name} {fixture_name}: WER {:.1}%, {:.2?} for {:.1} s of audio on {}\n  {}",
      w * 100.0,
      out.elapsed,
      f.seconds(),
      out.device,
      out.text
    );
    if w > MAX_WER {
      failed.push(fixture_name);
    }
  }

  assert!(failed.is_empty(), "{name}: WER above {:.0}% on {failed:?}", MAX_WER * 100.0);
}

#[test]
#[ignore = "needs models: make test-models"]
fn gigaam_ru_wer() {
  check_wer(model_ru(), &[("ru-short", None), ("ru-typical", None)]);
}

#[test]
#[ignore = "needs models: make test-models"]
fn whisper_ru_en_wer() {
  check_wer(
    model_en(),
    &[
      ("ru-short", Some("ru")),
      ("ru-typical", None),
      ("en-short", Some("en")),
      ("en-paragraph", None),
      ("en-short-44k", None),
    ],
  );
}

/// A real model whose run panics: the job fails, the model is dropped, and a fresh one on a new
/// thread transcribes the next job as well as the first.
#[test]
#[ignore = "needs models: make test-models"]
fn a_panic_in_a_run_fails_it_and_a_fresh_model_takes_the_next_job() {
  let faults = Faults::new();
  let engine =
    Engine::start(EngineConfig { loader: faults.wrap(Wavo), ..EngineConfig::new(model_ru()) })
      .unwrap();
  let short = fixture("ru-short");
  assert_wer(&short, &run(&engine, &short, None).unwrap());
  let first = engine.status().thread;

  faults.run(Fault::Panic);
  let err = run(&engine, &short, None).unwrap_err();
  eprintln!("a panic during a run: {err}");
  assert!(matches!(&err, EngineError::Crashed(m) if m.contains("panicked")), "{err}");
  assert_eq!(engine.status().phase, Phase::Stopped);

  let out = run(&engine, &short, None).unwrap();
  assert_wer(&short, &out);
  let status = engine.status();
  assert_ne!(status.thread, first, "a new thread serves the next job");
  eprintln!(
    "after the panic: {:.2?} with the load ({:.2?})",
    out.elapsed,
    status.last_load.unwrap()
  );
}

#[test]
#[ignore = "needs models: make test-models"]
fn warm_start_to_loaded_is_fast() {
  let short = fixture("ru-short");
  // Warms the OS shader cache and the page cache with a full cycle.
  run(&Engine::start(EngineConfig::new(model_ru())).unwrap(), &short, None).unwrap();

  let mut times: Vec<Duration> = (0..5)
    .map(|_| {
      let engine = Engine::start(EngineConfig::new(model_ru())).unwrap();
      run(&engine, &short, None).unwrap();
      engine.status().last_load.unwrap()
    })
    .collect();
  times.sort();
  let median = times[times.len() / 2];
  eprintln!("warm thread start → loaded, GigaAM Q8: median {median:.2?} of {times:.2?}");
  assert!(median <= WARM_LOAD_LIMIT, "median {median:?} > {WARM_LOAD_LIMIT:?}");
}

/// The long fixture (two paragraphs and a pause) through `model`: the pause is shortened, the input
/// is cut into pieces of up to `window`, WER and every paragraph within limits.
fn check_long(model: PathBuf, window: Duration) {
  let name = model_name(&model);
  let f = fixture("ru-long");
  let recorded = Clip::analyze(f.pcm.clone());
  let input = f.input();
  eprintln!(
    "ru-long: {:.2} s recorded, {:.2} s to the engine",
    recorded.seconds(),
    input.seconds()
  );
  assert!(input.seconds() < recorded.seconds() - 1.5, "the 2.5 s pause is shortened");

  let engine = Engine::start(EngineConfig::new(model)).unwrap();
  run(&engine, &fixture("ru-short"), None).unwrap(); // warm-up
  let seconds = input.seconds();
  let out = engine.transcribe(input.pcm, None).wait().unwrap();
  let w = wer(&f.text, &out.text);
  let recall = paragraph_recall(&f.text, &out.text);
  eprintln!(
    "{name} ru-long: WER {:.1}%, paragraphs {recall:.2?}, {} pieces, {:.2?}\n  {}",
    w * 100.0,
    out.pieces,
    out.elapsed,
    out.text
  );
  let least = (seconds / window.as_secs_f64()).ceil() as usize;
  assert!(out.pieces >= least, "{name}: {} pieces of {seconds:.1} s", out.pieces);
  assert!(w <= MAX_WER, "{name}: WER {:.1}%", w * 100.0);
  assert!(recall.len() == 2 && recall.iter().all(|&r| r >= MIN_PARAGRAPH_RECALL), "{recall:?}");
}

#[test]
#[ignore = "needs models: make test-models"]
fn gigaam_long_recording() {
  check_long(model_ru(), Duration::from_secs(25));
}

#[test]
#[ignore = "needs models: make test-models"]
fn whisper_long_recording() {
  check_long(model_en(), Duration::from_secs(30));
}

/// Parakeet v3 cannot be told a language: it is not, and detects it; long input is cut at wavo's
/// window for it.
#[test]
#[ignore = "needs models: make test-models"]
fn parakeet_detects_the_language_it_is_given() {
  check_wer(model_parakeet(), &[("ru-short", Some("ru")), ("en-short", Some("en"))]);
  check_long(model_parakeet(), Duration::from_secs(60));
}

#[test]
#[ignore = "needs models: make test-models"]
fn gigaam_warm_latency() {
  let engine = Engine::start(EngineConfig::new(model_ru())).unwrap();
  run(&engine, &fixture("ru-short"), None).unwrap();
  for (name, limit) in [("ru-short", WARM_SHORT_LIMIT), ("ru-typical", WARM_TYPICAL_LIMIT)] {
    let f = fixture(name);
    let input = f.input();
    let mut times: Vec<Duration> = (0..7)
      .map(|_| {
        let out = engine.transcribe(input.pcm.clone(), None).wait().unwrap();
        assert_wer(&f, &out);
        out.elapsed
      })
      .collect();
    times.sort();
    let median = times[times.len() / 2];
    let seconds = input.seconds();
    eprintln!(
      "warm GigaAM Q8 {name} ({seconds:.1} s to the engine): median {median:.2?} of {times:.2?}"
    );
    assert!(median <= limit, "{name}: median {median:?} > {limit:?}");
  }
}
