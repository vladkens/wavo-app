// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The macOS shell: the menu bar app on nimbo ([app]) and what only AppKit does here: where the
//! person types ([focus], through the accessibility API), the paste that gives the clipboard back
//! ([paste]), the session's text output through the main thread ([output]) and the microphone's
//! permission ([microphone]).

mod app;
mod focus;
mod microphone;
mod output;
mod paste;

use app::proxy;
pub use app::run;
