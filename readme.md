# Wavo app

Local dictation for macOS: hold a hotkey, speak, release, and the text lands where you were typing. Speech is recognized on your machine by [wavo](https://github.com/vladkens/wavo); the UI is drawn by [nimbo](https://github.com/vladkens/nimbo).

Work in progress: the app is moving here from its experiment, step by step ([the plan](docs/plans/20261023-port.md)).

## Install

Download `Wavo-<version>.zip` from [GitHub Releases](https://github.com/vladkens/wavo-app/releases), unzip it and move `Wavo.app` to `/Applications`; the rolling [dev build](https://github.com/vladkens/wavo-app/releases/tag/dev) follows `main`. macOS 14 or newer.

Wavo is signed ad hoc until it has a Developer ID, so macOS refuses the first launch: right-click `Wavo.app` → Open (on macOS 15 and later: System Settings › Privacy & Security › Open Anyway), or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/Wavo.app
```

An ad hoc signature changes with every build, so after an update macOS asks for Accessibility and the microphone again; if Wavo still says Accessibility is missing, remove it from that list in System Settings and allow it again.

To build from source (Rust): `make install` builds `Wavo.app`, installs it into `/Applications` and starts it.

Wavo needs a speech model: download one on the Models page of the settings, or with [wavo](https://github.com/vladkens/wavo), e.g. `wavo pull parakeet-v3`.

To move the history and settings to another Mac, export them on the old one with Advanced › Data › Export in the settings, or with the command line inside the app:

```sh
/Applications/Wavo.app/Contents/MacOS/wavo-app export wavo.tar
```

and import them on the new one with Wavo quit (without `--settings` only the history comes):

```sh
/Applications/Wavo.app/Contents/MacOS/wavo-app import wavo.tar --settings
```

## License

MIT
