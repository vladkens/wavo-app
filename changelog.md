## v0.1.0 – 2026-10-29

### Features

- Added dictation on macOS 14+, Windows 10 and 11, and Linux on X11 and Wayland: hold a hotkey (Option-Space on macOS, Ctrl+Alt+Space elsewhere) and speak, or tap it to start and stop, Esc to cancel; the microphone stays open while you finish a word after the release, and the text is pasted where you were typing with the clipboard put back (#2, #6, #8, #24, #29, #31)
- Added speech recognition on your computer by wavo, on the GPU or a Mac's Neural Engine, in about 100 languages: nothing leaves the machine; long recordings are transcribed in pieces, and a dictation never waits for the Neural Engine to get ready (#5, #10, #20, #34)
- Added the menu bar and tray app: an icon for each state, Copy Last Dictation, recent dictations, the model and the microphone to use, and short warnings that open where a missing permission or model is fixed (#8, #12, #22, #24, #32)
- Added the HUD: a small pill with the level while you speak and the transcription after, at the bottom or the top of the screen, which never takes the focus (#8, #29, #34)
- Added Automatic, the default microphone: the system's input unless it is a Bluetooth headset's (worse sound, the first words lost) or a Mac's built-in one under a closed lid, shown as `Automatic (MacBook Pro Microphone)` in the menu (#35)
- Added the settings window: General (permissions, microphone, model, language, shortcut), Models (download with progress, resume and delete wavo's models, by language, Parakeet Ultra recommended), History, Advanced and About; it opens by itself only at a launch with a permission or a model missing (#12, #18, #19, #22)
- Added the history: every recording with speech, kept forever or for a while, the whole of it in one fast list; search, play back, copy, delete, or transcribe again with another model and restore the previous text; a recording left without text is transcribed again once a model can (#4, #9, #21, #23, #30)
- Added export and import of the history with its recordings between computers, and the history of Handy brought in through `scripts/handy.py` (#11, #22)
- Added updates in place: a release looks for a new version at launch and once a day and offers it in the menu, where Install and Relaunch puts it in place; Advanced › Automatic updates turns the check off (#37)
- Added a signed and notarized macOS app, which opens without Gatekeeper's warning (#36)
- Added the command line: `wavo-app toggle` for a desktop's own shortcut where Wavo's hotkey can't be (GNOME before 48 on Wayland), `quit`, and `uninstall`, which removes the app and all it left: the history, settings, caches, downloaded models and permissions (#17, #22, #24, #30)
- Added the downloads: Wavo.app for Apple Silicon Macs, Wavo.exe for Windows on x64 and Arm, and AppImages for Linux on x86_64 and aarch64 (#25, #26)

**Full Changelog**: https://github.com/vladkens/wavo-app/commits/v0.1.0
