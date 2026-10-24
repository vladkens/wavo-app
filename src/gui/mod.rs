// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The menu bar app on nimbo ([app]): where the person types ([focus]), the paste that gives the
//! clipboard back ([paste]) and the session's text output through the main thread ([output]).

mod app;
mod focus;
mod output;
mod paste;

use app::proxy;
pub use app::run;
