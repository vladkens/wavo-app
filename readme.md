<div align="center">

<img src="assets/icon.png" alt="Wavo" width="128" />

# Wavo

**Local dictation for macOS, Windows and Linux**

[<img src="https://badges.ws/github/license/vladkens/wavo-app" alt="license" />](https://github.com/vladkens/wavo-app/blob/main/LICENSE)
[<img src="https://badges.ws/badge/follow-@uid127-black" alt="follow @uid127" />](https://x.com/uid127)
[<img src="https://badges.ws/badge/-/buy%20me%20a%20coffee/ff813f?icon=buymeacoffee&label" alt="donate" />](https://buymeacoffee.com/vladkens)

</div>

Hold a hotkey, speak, release, and the text lands where you were typing: a chat, an editor, a terminal, any app. Speech is recognized on your computer by [wavo](https://github.com/vladkens/wavo), on the GPU or a Mac's Neural Engine: no account, no subscription, and your voice never leaves the machine. Wavo lives in the menu bar (the notification area on Windows, the tray on Linux), its download is about 6 MB, and its windows are drawn by [nimbo](https://github.com/vladkens/nimbo).

## 🌟 Features

- 🎙️ Hold the hotkey to talk, or tap it to start and stop; Esc cancels. Option-Space on macOS, Ctrl+Alt+Space elsewhere, or a shortcut of your own, a lone modifier key too on macOS and Windows
- ⌨️ The text is pasted into the app you were typing in, and the clipboard gets back what it held
- 🔒 Fully local: the only traffic is a model's download from Hugging Face, once
- 🧠 Parakeet Ultra for 25 European languages, GigaAM for Russian, Whisper for 100 languages: [wavo's models](https://github.com/vladkens/wavo#-models), downloaded in Settings
- ⚡ On Apple Silicon, Parakeet and GigaAM run on the Neural Engine: 11 s of speech becomes text in about 60 ms on an M2
- 💊 A small HUD at the bottom or top of the screen shows the level and the time while you speak, and never takes the focus
- 📜 A searchable history of every dictation with its recording: play it back, transcribe it again with another model, copy it; kept forever, for a time, or the last N
- 📦 The history with its recordings moves to another computer as a zip, and Handy's history comes over too
- 🪶 An idle model is unloaded after 5 minutes (or at once, or never), so Wavo stays small between dictations
- 🖥️ macOS 14+ on Apple Silicon, Windows 10 and 11 (x64, Arm), Linux on X11 and Wayland (x86_64, aarch64)

## 🤖 How it's made

Wavo is written by AI agents following [agents.md](agents.md); I choose what to build and review every pull request. Its speech engine, [wavo](https://github.com/vladkens/wavo), and its UI toolkit, [nimbo](https://github.com/vladkens/nimbo), are made the same way, each with its own tests. This isn't a weekend prompt: I've written software by hand for years — [macmon](https://github.com/vladkens/macmon) (Rust, Apple Silicon system monitor), [twscrape](https://github.com/vladkens/twscrape) (Python, X/Twitter scraping) and [ecloop](https://github.com/vladkens/ecloop) (C, low-level elliptic-curve math) — and agents are how I build now.

## 📥 Installation

Wavo has no release yet: download the file for your system from the [dev pre-release](https://github.com/vladkens/wavo-app/releases/tag/dev), the newest build of `main`:

- macOS 14 or later, Apple Silicon: `Wavo-dev-macos.zip`. Unzip it and move `Wavo.app` into `/Applications`. Or build it with Rust: `make install` builds `Wavo.app`, installs it into `/Applications` and starts it.
- Windows 10 or 11: `Wavo-dev-windows-x64.zip`, or `-arm64` for Windows on Arm. Unzip it anywhere and run `Wavo.exe`. Its icon is in the notification area, perhaps under the ^ arrow.
- Linux, x86_64 or aarch64: `Wavo-dev-linux-<arch>.AppImage`. Make it executable (`chmod +x`) and run it, on X11 or Wayland.

Then pick a model in Settings › Models (Parakeet Ultra is the recommended one, 0.74 GB) and allow what the system asks for: on macOS, the microphone and Accessibility, which the paste needs.

<details>
<summary>The first launch</summary>

Wavo is signed ad hoc until it has a Developer ID, so macOS refuses the first launch: right-click `Wavo.app` → Open (on macOS 15 and later: System Settings › Privacy & Security › Open Anyway), or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/Wavo.app
```

An ad hoc signature changes with every build, so after an update macOS asks for Accessibility and the microphone again.

On Windows `Wavo.exe` is not signed yet: SmartScreen says "Windows protected your PC" at the first launch; More info → Run anyway.

</details>

<details>
<summary>Linux: Wayland, GNOME and the tray</summary>

On Wayland the paste goes through the desktop's RemoteDesktop portal, which asks once; the hotkey needs its GlobalShortcuts portal (KDE Plasma 6, GNOME 48 and later), and the menu's "Allow the Hotkey…" lets the desktop ask for it. Where there is no such portal (GNOME before 48 on Wayland, as in Ubuntu 24.04, where the paste still works), add a shortcut in the desktop's own keyboard settings that runs `/path/to/Wavo-dev-linux-<arch>.AppImage toggle`: it starts a dictation and the next one stops it.

On GNOME the HUD doesn't show under Wayland, and the tray icon tells the state. The tray icon needs a panel that shows StatusNotifierItems: KDE, Xfce, Cinnamon, MATE, or GNOME with the AppIndicator extension, which Ubuntu ships.

</details>

## 🚀 Usage

Hold the hotkey, speak and let go: the HUD shows the level while you speak, then a wave while the text is made, and the text is pasted where the cursor was. A short tap starts a dictation that the next press ends, for longer talks; Settings › General › Activation keeps only one of the two. Esc cancels a recording or its transcription.

The menu-bar icon shows the state and opens the menu: the recent dictations to copy again, Copy Last Dictation, the model and the microphone in use, Settings and Quit. Settings has five pages:

- **General**: the microphone, the model and its language, the shortcut and how it starts a dictation, the paste and the clipboard, launch at login, and the permissions macOS asks for
- **Models**: download, use and delete models
- **History**: the dictations by day, with a search; a selected one shows its whole text, plays its recording, and can be transcribed again, given back its previous text, or deleted
- **Advanced**: where the HUD shows, when the model unloads, the Neural Engine, how long the history is kept, its folders, and export and import
- **About**: the version and acknowledgements

Settings and the history with its recordings are kept in `~/Library/Application Support/me.vladkens.wavo` on macOS, `%LOCALAPPDATA%\me.vladkens.wavo` on Windows and `~/.local/share/me.vladkens.wavo` on Linux. Models are in the Hugging Face cache (`~/.cache/huggingface/hub`), shared with the [wavo](https://github.com/vladkens/wavo) command line and `hf`.

The app's binary takes a few commands; on macOS it is `/Applications/Wavo.app/Contents/MacOS/wavo-app`, on Windows `Wavo.exe`, on Linux the AppImage:

```sh
wavo-app toggle               # start or stop a dictation in the running app
wavo-app quit                 # quit the running app
wavo-app uninstall --dry-run  # list what uninstall would remove
wavo-app uninstall            # remove Wavo and all it left: data, caches, models, permissions, the app
```

<details>
<summary>Moving to another computer</summary>

Export the history with its recordings on the old computer with Advanced › Data › Export…, and bring it in on the new one with Advanced › Data › Import…: the dictations it does not have yet are added.

</details>

<details>
<summary>Moving from Handy</summary>

The history of [Handy](https://github.com/cjpais/Handy), with its recordings, comes over through an archive; Handy itself is only read. Quit Handy, then run:

```sh
curl -fsSL https://raw.githubusercontent.com/vladkens/wavo-app/main/scripts/handy.py | python3 -
```

It writes `~/Downloads/handy-export.zip`: bring it in with Advanced › Data › Import…. Dictations Wavo already has are left, so importing twice adds nothing. `HANDY_DIR=<folder>` before `python3` reads Handy's data from another folder.

</details>

## 🤝 Contributing

Pull requests are welcome, and so are your agents: point them at [agents.md](agents.md). `make check` lints, `make test` runs the tests, `make shots` draws the settings pages into `target/shots/` without opening a window, and on a Mac `make run` builds `Wavo.app` and starts it.

## 🙏 Credits

- [Handy](https://github.com/cjpais/Handy): the app that inspired Wavo.
- [wavo](https://github.com/vladkens/wavo): the speech recognition, built on [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp)'s reference and the GGUF conversions [handy-computer](https://huggingface.co/handy-computer) publishes. Wavo ships no weights: each model is downloaded under its own license, listed in [wavo's credits](https://github.com/vladkens/wavo#-credits).
- [nimbo](https://github.com/vladkens/nimbo): the UI toolkit Wavo is drawn with.

## 📝 License

Distributed under the [MIT License](LICENSE).

## 🔍 See also

- [cjpais/Handy](https://github.com/cjpais/Handy) – Offline speech-to-text for macOS, Windows and Linux, on Tauri.
- [vladkens/wavo](https://github.com/vladkens/wavo) – The speech engine behind Wavo, as a command line, a local server and a Rust library.
