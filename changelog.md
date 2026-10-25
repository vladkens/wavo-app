## v0.1.0 – 2026-10-25

### Features

- Added dictation on macOS 14+: hold a hotkey and speak, or tap it to start and stop, Esc to cancel; the text is pasted where you are typing and the clipboard put back, or left on it when it can't be pasted (a password field, the focus gone elsewhere); speech is recognized on this Mac by wavo, nothing leaves it (#2, #5, #6, #8, #14)
- Added the menu bar app: an icon for each state, Copy Last Dictation, Recent dictations, the model to use, Unload Model, launch at login, and short warnings that open where a missing permission or model is fixed (#8, #12)
- Added the HUD: a small panel with the recording's level, the transcription and what went wrong, at the bottom or the top of the screen (#8)
- Added onboarding: at the first launch Wavo asks for the microphone and Accessibility and opens the settings on the permissions and a model to download (#12)
- Added the settings window: General (permissions, shortcut, model, language), Models (download with progress, cancel, resume, delete and choose wavo's models, by language), History, Advanced (unload, retention, folders, data) and About (#12)
- Added the history: an entry for every recording with speech, kept forever or for a while; one without text (no model, an error, nothing recognized, a crash) is transcribed again once another model can, and can be transcribed again by hand with one undo; search, play, copy and delete (#4, #9, #12)
- Added long recordings transcribed in pieces, and a note when the model understands one language only (#10)
- Added export and import of the history and settings between computers, and Handy's history imported from Advanced › Data or `wavo-app import handy` (#11)
- Added the command line: `wavo-app transcribe`, `devices`, `record`, `export`, `import`, the headless `daemon` and `ctl` (#5, #6, #11)

**Full Changelog**: https://github.com/vladkens/wavo-app/commits/v0.1.0
