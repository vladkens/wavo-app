// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! What the menu bar app shows, platform-free: the HUD ([hud]'s state, drawn on nimbo by the
//! [overlay]), the menu ([menu]) and the menu bar icons ([icons]). The macOS shell uses them;
//! elsewhere only the tests build them.

pub mod hud;
pub mod icons;
pub mod menu;
pub mod overlay;
