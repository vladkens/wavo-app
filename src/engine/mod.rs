// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! Speech engine, in the app's process: the [`supervisor`] (deadlines, restarts, the queue) and
//! the engine thread it starts, which owns the [`model`]. Only [`model::Wavo`] loads wavo's models,
//! and only on an engine thread.

pub mod model;
pub mod supervisor;

#[cfg(test)]
mod tests;

pub use supervisor::{Engine, EngineConfig};
