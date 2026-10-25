# Wavo app

Local dictation for macOS: hold a hotkey, speak, release, and the text lands where you were typing. Speech is recognized on your machine by [wavo](https://github.com/vladkens/wavo); the UI is drawn by [nimbo](https://github.com/vladkens/nimbo).

Work in progress: the app is moving here from its experiment, step by step ([the plan](docs/plans/20261023-port.md)).

## 📥 Installation

Download `Wavo-<version>.zip` from the [latest release](https://github.com/vladkens/wavo-app/releases/latest), unzip it and move `Wavo.app` into `/Applications` (macOS 14 or later). Or build it with Rust: `make install` builds `Wavo.app`, installs it into `/Applications` and starts it.

<details>
<summary>The first launch</summary>

Wavo is signed ad hoc until it has a Developer ID, so macOS refuses the first launch: right-click `Wavo.app` → Open (on macOS 15 and later: System Settings › Privacy & Security › Open Anyway), or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/Wavo.app
```

An ad hoc signature changes with every build, so after an update macOS asks for Accessibility and the microphone again.

</details>

<details>
<summary>Moving to another Mac</summary>

Export the history and settings on the old Mac with Advanced › Data › Export, or:

```sh
/Applications/Wavo.app/Contents/MacOS/wavo-app export wavo.tar
```

and import them on the new one with Wavo quit (without `--settings` only the history comes):

```sh
/Applications/Wavo.app/Contents/MacOS/wavo-app import wavo.tar --settings
```

</details>

## 📝 License

MIT
