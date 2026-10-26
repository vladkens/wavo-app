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

Export the history with its recordings on the old Mac with Advanced › Data › Export…, and bring it in on the new one with Advanced › Data › Import…: the dictations it does not have yet are added.

</details>

<details>
<summary>Moving from Handy</summary>

The history of [Handy](https://github.com/cjpais/Handy), with its recordings, comes over through an archive; Handy itself is only read. Quit Handy, then run:

```sh
curl -fsSL https://raw.githubusercontent.com/vladkens/wavo-app/main/scripts/handy.py | python3 -
```

It writes `~/Downloads/handy-export.zip`: bring it in with Advanced › Data › Import…. Dictations Wavo already has are left, so importing twice adds nothing. `HANDY_DIR=<folder>` before `python3` reads Handy's data from another folder.

</details>

## 📝 License

MIT
