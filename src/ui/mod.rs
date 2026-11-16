// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The settings window's building blocks, over nimbo's form: a row is a label with its mark and
//! a control, and a change comes back as a value, which the window applies to the settings. A
//! component holds no state of its own and names nothing itself: the control's [Id] is the
//! caller's, so the tests and the person's clicks find it where they did before.

pub mod actions;
pub mod parts;
pub mod rows;

/// A page's padding in the window, the History list's too.
pub const PAGE_PAD: [f32; 4] = [12.0, 20.0, 20.0, 20.0];

/// The height of the History panel under its list.
pub const HISTORY_FOOT_H: f32 = 150.0;

/// The height of the line under that panel, with the whole history.
pub const HISTORY_TOTAL_H: f32 = 30.0;

/// A progress bar's width, its caption's line beside it.
pub const PROGRESS_W: f32 = 120.0;

/// The app icon on the About page: 64 points of its 128 px image.
pub const ABOUT_ICON: f32 = 64.0;

/// How far a keyboard language sits in from the Language row's name.
pub const KEYBOARD_ROW_INDENT: f32 = 16.0;
