// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Dictation session: the state [`machine`] (activation modes, the hold threshold, cancel) and
//! the [`controller`] that runs it: recorder, engine, history and text [`output`] on one thread.

pub mod controller;
pub mod machine;
pub mod output;

#[cfg(test)]
mod tests;

pub use controller::{Config, Session};
