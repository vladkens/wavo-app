# Wavo app

Local dictation for macOS, Windows and Linux: hold a hotkey, speak, release, and the text lands where you were typing. Speech is recognized on your machine by [wavo](https://github.com/vladkens/wavo); the UI is drawn by [nimbo](https://github.com/vladkens/nimbo).

Work in progress: the app is moving here from its experiment, step by step ([the plan](docs/plans/20261023-port.md)).

## 📥 Installation

Download the file for your system from the [latest release](https://github.com/vladkens/wavo-app/releases/latest), or the newest build of `main` from the [dev pre-release](https://github.com/vladkens/wavo-app/releases/tag/dev):

- macOS 14 or later: `Wavo-<version>-macos.zip`. Unzip it and move `Wavo.app` into `/Applications`. Or build it with Rust: `make install` builds `Wavo.app`, installs it into `/Applications` and starts it.
- Windows 10 or 11: `Wavo-<version>-windows-x64.zip`, or `-arm64` for Windows on Arm. Unzip it anywhere and run `Wavo.exe`. Its icon is in the notification area, perhaps under the ^ arrow.
- Linux, x86_64 or aarch64: `Wavo-<version>-linux-<arch>.AppImage`. Make it executable (`chmod +x`) and run it. It works on X11 and on Wayland. On Wayland the paste goes through the desktop's RemoteDesktop portal, which asks once, at the first paste or earlier through the menu's "Allow Paste…"; the hotkey needs its GlobalShortcuts portal (KDE Plasma 6, GNOME 48 and later). The tray icon needs a panel that shows StatusNotifierItems (KDE, Xfce, Cinnamon, MATE; GNOME with the AppIndicator extension, which Ubuntu ships).

The hotkey is Option-Space on macOS and Ctrl+Alt+Space elsewhere; Settings changes it. Where the desktop has no GlobalShortcuts portal (GNOME before 48 on Wayland, as in Ubuntu 24.04, where the paste still works), add a shortcut in its own keyboard settings that runs Wavo with `toggle` (`/path/to/Wavo-<version>-linux-<arch>.AppImage toggle`): it starts a dictation and the next one stops it.

<details>
<summary>The first launch</summary>

Wavo is signed ad hoc until it has a Developer ID, so macOS refuses the first launch: right-click `Wavo.app` → Open (on macOS 15 and later: System Settings › Privacy & Security › Open Anyway), or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/Wavo.app
```

An ad hoc signature changes with every build, so after an update macOS asks for Accessibility and the microphone again.

On Windows `Wavo.exe` is not signed yet: SmartScreen says "Windows protected your PC" at the first launch; More info → Run anyway. On Linux under Wayland, the menu's "Allow the Hotkey…" lets the desktop ask once for Wavo's shortcut.

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
