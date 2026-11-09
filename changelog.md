## v0.2.0 – 2026-11-09

### Features

- Added dictation on Mac accounts that can't allow Accessibility: the text goes to the clipboard, ready to paste with ⌘V (#55)
- Added "Until Wavo quits" to Keep the history: nothing is written to disk, and the dictations are gone when Wavo quits (#63)
- Added Delete All in Advanced to delete the saved history and its recordings (#63)
- Added each model's accuracy and speed in the chosen language to the Models page, with a recommended model (#60)
- Added "Clean up the text", on by default: a space after each paste, a lowercase start when you go on with a sentence, and "um", "uh" and the like left out in English and Russian (#61)
- Added a gradient look for the HUD, with a switch back to the plain one (#59)
- Added a warning in the menu when a USB microphone gets slow to start, so the start of a phrase isn't lost (#56)
- Added Copy Recording Path and Show in Finder to a dictation's menu in History (#53)

### Improvements

- Improved the Models page: models made for the chosen language come first, models that run on the Neural Engine say ANE, several models can be deleted at once, and models that make too many errors in a language are folded away and never recommended (#53, #60, #64)
- Improved the menu: it says when transcription runs on a GPU emulated on the CPU and is slow (#53)

### Fixes

- Fixed Wavo staying on the GPU with about 400 MB of memory after the model was reloaded, until a restart, while the Neural Engine was on (#62)

**Full Changelog**: https://github.com/vladkens/wavo-app/compare/v0.1.0...v0.2.0

---

## v0.1.0 – 2026-10-31

First release of Wavo, a dictation app for macOS, Windows and Linux.

### Features

- Added dictation into any app: hold the hotkey and speak, or tap it to start and stop.
- Added offline speech recognition in about 100 languages, on the GPU or a Mac's Neural Engine.
- Added fast transcription: a minute of speech in under a second on an M2 Mac.
- Added the menu bar and tray app, with recent dictations and a small indicator while you speak.
- Added automatic microphone choice that prefers a USB microphone over a Bluetooth headset.
- Added a searchable history with playback, re-transcription with another model, and import from Handy.
- Added updates in place from the menu.
- Added installers: a signed DMG for Apple Silicon, Windows setups for x64 and ARM64, and an AppImage and a .deb for Linux.

**Full Changelog**: https://github.com/vladkens/wavo-app/commits/v0.1.0
