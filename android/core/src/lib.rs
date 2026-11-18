// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

//! Wavo for Android: the speech engine ([wavo]), its models and the desktop app's text cleanup,
//! behind the JNI functions of the Kotlin class `io.github.tyetladd.wavo.Native`.
//!
//! The Kotlin side owns the microphone, the screens and the keyboard; this crate turns 16 kHz mono
//! samples into text. [engine] has no JNI in it, so it is tested on the machine that builds it.

// The desktop app's own files, not copies: a fix there is a fix here.
#[allow(dead_code)]
#[path = "../../../src/cleanup.rs"]
mod cleanup;
#[allow(dead_code)]
#[path = "../../../src/numbers.rs"]
mod numbers;
#[allow(dead_code)]
#[path = "../../../src/gradient.rs"]
mod gradient;

mod cores;
pub mod engine;
#[cfg(test)]
mod gradient_test;
mod jni;
mod store;
