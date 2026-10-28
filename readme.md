<div align="center">

<img src="assets/icon.png" alt="Wavo" width="128" />

# Wavo

**Local dictation for macOS, Windows and Linux**

[<img src="https://badges.ws/github/license/vladkens/wavo-app" alt="license" />](https://github.com/vladkens/wavo-app/blob/main/LICENSE)
[<img src="https://badges.ws/badge/follow-@uid127-black" alt="follow @uid127" />](https://x.com/uid127)
[<img src="https://badges.ws/badge/-/buy%20me%20a%20coffee/ff813f?icon=buymeacoffee&label" alt="donate" />](https://buymeacoffee.com/vladkens)

</div>

**Hold a hotkey, speak, release**, and the text lands where you were typing: a chat, an editor, a terminal, any app. Speech is recognized **on your computer** by [wavo](https://github.com/vladkens/wavo), on the GPU or a Mac's Neural Engine: no account, no subscription, and your voice never leaves the machine. Wavo lives in the menu bar or the tray, and its windows are drawn by [nimbo](https://github.com/vladkens/nimbo).

<p align="center">
  <a href="https://github.com/user-attachments/assets/1826b949-3c28-47e8-a6f4-9affad10cc09"><img src="https://github.com/vladkens/wavo-app/blob/assets/wavo-greeting.webp?raw=true" width="640" alt="A dictation typed into TextEdit, then found in Wavo's history; click for the video with sound" /></a>
</p>

## 🌟 Features

- 🎙️ **Hold to talk**: hold the hotkey and speak, or tap it to start and stop; Esc cancels
- ⌨️ **Pastes where you type**: into any app, and the clipboard keeps what it held
- 🔒 **Fully local**: no account, no cloud; only the model is downloaded, once
- ⚡ **Fast**: 11 s of speech becomes text in about 60 ms on an M2's Neural Engine
- 🌍 **100 languages**: Parakeet Ultra for 25 European ones, GigaAM for Russian, Whisper for the rest
- 💊 **A quiet HUD**: a small pill shows the level while you speak and never takes the focus
- 📜 **History**: every dictation with its recording, searchable; play it back or transcribe it again
- 🖥️ **Every desktop**: macOS, Windows and Linux on X11 and Wayland, in a 6 MB download

## 🚀 Usage

**Download** the file for your system from the [dev pre-release](https://github.com/vladkens/wavo-app/releases/tag/dev), the newest build (there is no release yet), and run it. Then pick a model in Settings › Models: **Parakeet Ultra** is the recommended one.

<details>
<summary>Which file, and the first launch</summary>

- **macOS** 14 or later, Apple Silicon: `Wavo-dev-macos.zip`; move `Wavo.app` into `/Applications`. It is signed ad hoc for now, so macOS refuses the first launch: right-click → Open (on macOS 15 and later: System Settings › Privacy & Security › Open Anyway), or run `xattr -dr com.apple.quarantine /Applications/Wavo.app`. After an update macOS asks for Accessibility and the microphone again.
- **Windows** 10 or 11: `Wavo-dev-windows-x64-setup.exe`, or `-arm64` for Windows on Arm; it installs Wavo for you alone, with no administrator, with a Start menu entry. It is not signed yet: SmartScreen says "Windows protected your PC", More info → Run anyway. Its icon is in the notification area, perhaps under the ^ arrow.
- **Linux**, x86_64 or aarch64: `Wavo-dev-linux-<arch>.AppImage`; make it executable (`chmod +x`) and run it. On Debian and Ubuntu, `Wavo-dev-linux-<arch>.deb` instead: `sudo apt install ./Wavo-dev-linux-<arch>.deb`, then Wavo is in the apps menu.

</details>

<details>
<summary>Linux: Wayland, GNOME and the tray</summary>

On Wayland the paste goes through the desktop's RemoteDesktop portal, which asks once, at the first paste or earlier through the menu's "Allow Paste…"; the hotkey needs its GlobalShortcuts portal (KDE Plasma 6, GNOME 48 and later), and the menu's "Allow the Hotkey…" lets the desktop ask for it. Where there is no such portal (GNOME before 48 on Wayland, as in Ubuntu 24.04, where the paste still works), add a shortcut in the desktop's own keyboard settings that runs `/path/to/Wavo-dev-linux-<arch>.AppImage toggle` (`wavo-app toggle` from the .deb): it starts a dictation and the next one stops it.

The tray icon needs a panel that shows StatusNotifierItems: KDE, Xfce, Cinnamon, MATE, or GNOME with the AppIndicator extension, which Ubuntu ships.

</details>

**Hold the hotkey** (Option-Space on macOS, Ctrl+Alt+Space elsewhere), speak and let go: the HUD shows the level while you speak, and the text is pasted where the cursor was. **A short tap** starts a dictation that the next press ends, for longer talks; **Esc** cancels.

The menu-bar icon opens the recent dictations, the model and the microphone. **Settings** changes the shortcut, the language and the rest, and keeps the **History**: search it, play a recording back, transcribe it again with another model.

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

<details>
<summary>Removing Wavo</summary>

The app's `uninstall` command removes all Wavo left: the history, settings and caches, the models it downloaded, and on macOS its permissions and the app itself. `--dry-run` lists them first. On Windows remove the app itself afterwards in Settings › Apps, and with a .deb by `sudo apt remove wavo`.

```sh
/Applications/Wavo.app/Contents/MacOS/wavo-app uninstall          # macOS
& "$env:LOCALAPPDATA\Programs\Wavo\Wavo.exe" uninstall --yes   # Windows, without the question
./Wavo-dev-linux-<arch>.AppImage uninstall                        # Linux, or wavo-app uninstall
```

</details>

## 🤝 Contributing

Pull requests are welcome, and so are your agents: point them at [agents.md](agents.md). `make check` lints, `make test` runs the tests, `make shots` draws the settings pages into `target/shots/` without opening a window, and on a Mac `make install` builds `Wavo.app`, installs it into `/Applications` and starts it.

## 🤖 How it's made

Wavo is written by AI agents following [agents.md](agents.md); I choose what to build and review every pull request. Its speech engine, [wavo](https://github.com/vladkens/wavo), and its UI toolkit, [nimbo](https://github.com/vladkens/nimbo), are made the same way, each with its own tests. This isn't a weekend prompt: I've written software by hand for years — [macmon](https://github.com/vladkens/macmon) (Rust, Apple Silicon system monitor), [twscrape](https://github.com/vladkens/twscrape) (Python, X/Twitter scraping) and [ecloop](https://github.com/vladkens/ecloop) (C, low-level elliptic-curve math) — and agents are how I build now.

## 🙏 Credits

- [Handy](https://github.com/cjpais/Handy): the app that inspired Wavo.
- [wavo](https://github.com/vladkens/wavo): the speech recognition, built on [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp)'s reference and the GGUF conversions [handy-computer](https://huggingface.co/handy-computer) publishes. Wavo ships no weights: each model is downloaded under its own license, listed in [wavo's credits](https://github.com/vladkens/wavo#-credits).
- [nimbo](https://github.com/vladkens/nimbo): the UI toolkit Wavo is drawn with.

## 📝 License

Distributed under the [MIT License](LICENSE).
