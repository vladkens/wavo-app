# Keyboard languages

Done: 2026-11-14

Goal: a bilingual person dictates with one shortcut, and the language follows the keyboard layout: the Russian layout dictates Russian with the model chosen for Russian, the English layout English with its own. A model for one language makes fewer errors than a multilingual one, and auto-detection fails on accented speech and short phrases. The HUD shows the language of every recording, so a wrong layout is caught while speaking.

Design: the mockup at https://claude.ai/code/artifact/3bba1f0e-a921-464f-a342-811b1684a61e (part 1 and part 2; part 3, a shortcut per language, comes later).

## Decisions

- Language gets one more choice, "Keyboard layout" (`language = "keyboard"`), after Auto-detect, offered where nimbo can read the layout (macOS for now). Every other choice works as today.
- With it, a row per language of the person's enabled input sources follows the Language row, nothing to add by hand: layouts of one language are one row (ABC and U.S. are English). A layout whose language isn't known (an emoji palette) has no row.
- A row's picker: "Same as Model" first (the default, stored as no entry), then the downloaded models that know the language, single-language ones first, then "Download more…", which opens Models filtered to that language. `keyboard_models: { "ru": "gigaam-v3" }` stores the rows. The Model row stays as it is: the model for "Same as Model" rows and for a layout without a language.
- A dictation in keyboard mode is told the layout's language (a model that takes no language isn't told, as today), and its text keeps it in the history; a layout without a language is auto-detected.
- The session keeps the language the app tells it on every change of the input source. A press reads it to load the right model while the person speaks; the recording's close (after the tail) reads it again and decides. So switching the layout while speaking changes the whole recording; one recording is one language.
- One engine as today: a dictation in another language than the one before loads the other model. A switch of layout reloads it at once (while recording, too), and the new model is prepared for the Neural Engine once, where a prepared one loads in about 0.1 s. Two engines only if the person feels it.
- A model chosen in a row is in use: Models doesn't delete it. A row's model gone from disk is what a gone Model is today: the first downloaded transcribes, with the warning.
- Transcribe Again goes by the layout in use, as a dictation would now: a dictation made in the wrong layout is fixed by switching the layout and transcribing it again.
- HUD: in keyboard mode a chip with the two-letter code (`RU`, `EN`) right after the red dot, while recording and transcribing; the pill widens for it. It pops (grows and settles back) when it appears and when the language changes. No full names.
- nimbo: a module `input_source`: `current()` and `enabled()` on the main thread (macOS's Text Input Sources must be asked there since macOS 14), `available()`, and `Change::InputSource` told when the selected or the enabled sources change. macOS only; Windows and Linux report none, and the app offers no Keyboard layout there.
- Not in this work: a shortcut per language (needs nimbo #66), Windows and Linux, "For Russian" labels on Models.
- No commits: the person reviews and commits. Work stays uncommitted in this worktree on `feat/keyboard-languages`, and in nimbo's `.worktrees/input-source` on `feat/input-source`; `.cargo/config.toml` patches nimbo to that worktree and is never committed.

## Tasks

### 1. nimbo: the input source

Files (nimbo): new `src/input_source.rs`, `src/platform/macos/app/input_source.rs`, stubs for Windows and Linux, `src/system.rs`, `src/platform/macos/app/hardware.rs`, `src/lib.rs`, `src/platform/mod.rs` docs.

- [x] `InputSource { id, name, language }`, `current()`, `enabled()` (keyboard layouts and input modes the person can select), `available()`; the language is the first of the source's languages, its primary subtag lower case (`en`, `ru`, `zh`).
- [x] `Change::InputSource` from the distributed notifications of a selected and of an enabled source change; refused off macOS.
- [x] A unit test for the language subtag; a headless run of a scratch binary on the main thread prints the sources (no window).

### 2. Settings and routing

Files: `src/settings.rs`, `src/session.rs`, `src/app.rs`.

- [x] `KEYBOARD`, `keyboard_models`, `Settings::route(keyboard) -> (model, language)`, with unit tests.
- [x] `Handle::keyboard(language)` and the session's last known language; the app sends it at start and on `Change::InputSource`.
- [x] Press: the route's model is loaded, again on a switch while recording; close: the route decides the model and the language, kept in the dictation and its entry. Transcribe Again by the layout in use.
- [x] `Status::language`: the dictation's language in keyboard mode.
- [x] Session tests: two models by layout, a switch while recording, a layout without a language, a row without a model, a mode other than keyboard as today.

### 3. Settings window

Files: `src/window.rs`, `src/app.rs`.

- [x] Shell's `keyboard_languages()`, read again on `Change::InputSource`.
- [x] "Keyboard layout" in Language where available, its rows and pickers, "Download more…" to Models filtered by the row's language; a row's model is in use.
- [x] Harness tests and the window shots.

### 4. HUD

Files: `src/hud.rs`, `src/app.rs`.

- [x] The language in `Show::Recording` and `Show::Transcribing`; the chip, the wider pill, the flash.
- [x] Harness tests and the HUD shots in both styles.

## Checks

Each task: `make check` and `make test` here, `make check` and `make test` in nimbo's worktree for task 1. Never open the app or a window, never record from the microphone, a temp `WAVO_HOME` in every run.

Follow-up for the person, not a task: `make -C /Users/user/Code/wavo-app/.worktrees/keyboard-languages run`, set Language to Keyboard layout, and dictate in both layouts, switching once while speaking.

## Result

Dictation follows the keyboard layout on macOS, Windows, and Linux under GNOME and Plasma: a model per layout language, the default model for the rest, the language in the HUD; nimbo's `input_source` in vladkens/nimbo#74. Rounds 2–8 below are what the person's tries changed and how it was checked.

- nimbo: +185 lines of library code; `make check` and `make test` pass but for `the_api_doc_is_the_gallerys`, which compares against the `assets` worktree's `photo.png` from #72, stale there and not touched here. A headless run on the main thread listed this Mac's sources: `U.S.` (`en`) and `Russian – PC` (`ru`).
- wavo-app: `make check` and `make test` pass (203 tests). The window shots `general-keyboard-*` and the HUD's `*-language` in `target/shots/`.
- Moving to nimbo's main brought #72's rename of `png_image` to `image` (two calls).

## Round 2

The person tried it: dictation follows the layout and works. Changes asked for:

- [x] "Same as Model" said nothing. Choosing Keyboard layout now starts each layout language on its best downloaded model (the recommended one, else the fewest errors), stored as the person's choice. A picker lists the models that know the language as Models orders them; a language without a model of its own shows the Model it goes to, "(doesn’t know Russian)" when it doesn't.
- [x] Short model names everywhere (`parakeet-v2`, as Models shows them): the Model picker, the keyboard pickers, the menu, the notes and warnings.
- [x] The languages sit inside the Language row, under it, indented, one line each (name, layouts dimmed, picker), no hairlines: one block. nimbo: a row with a footer centres its label on a control taller than the label's line (the picker sat 3.5 points low).
- [x] HUD: the language after the time, quiet: a faint plate in the text's colour, the code dimmed; it lights up and fades back when it appears and at a change, in place of the grow.

## Round 3

- [x] Language's ⓘ with Auto-detect: what it does, and with a model of one language that every dictation is in its language. Model's ⓘ with Keyboard layout: it takes a layout of any other language.
- [x] A keyboard language's layouts in the label's size, dimmed: one baseline with the name.
- [x] Models: Use and "✓ In use" centred in one column, so the rows line up.
- [x] HUD: the chip white again (the plain pill's text colour there), after the time, with no animation.
- [x] nimbo: an open menu (a picker's list) closes when the window loses the focus, as AppKit's do.

## Round 4

- [x] A keyboard language's picker starts with the default model by name, `parakeet-v2 (default)`: what a language without a model of its own, a new layout's, and one no downloaded model knows go to; picking it drops the language's own. With Keyboard layout the Model row is "Default model", its ⓘ says what it takes.
- [x] Models: Use as wide as "✓ In use", both centred in one column.
- [x] Models: the models dictations go to (the Model, a keyboard language's) first in Downloaded, then by rank as before.

## Round 5

- [x] Models: In use without the check, so it and Use centre on one line.
- [x] General's Dictation: Microphone, the model, Shortcut, Activation, then Language last, which opens into the keyboard languages.

## Round 6

- [x] General: Activation before Shortcut (how the key records, then the key).
- [x] The macOS 27 VM on the M2 (test host, `WAVO_FAKE_MIC` with `ru-typical.wav`, real models, `wavo-app toggle`, layouts switched by a TIS helper): ABC goes to the default model in `en`, Russian to gigaam-v3 in `ru`, ABC switched to Russian while recording goes to gigaam-v3 in `ru`, German (no model of its own) to the default in `de` (not sent to parakeet-v2), Auto-detect ignores the layout. The HUD shows EN, then RU after the switch while recording. The settings window lists English, German, Russian, and drops and brings back German as it is disabled and enabled while it is open.
- Not run: Windows and Linux (the option isn't offered there; CI's cross clippy builds them), the menu closing on focus loss (nimbo's harness test), per-document input sources.

## Round 7

- [x] Settings: choosing another microphone clears the slow start warning at once (`fix/mic-replug`, its own PR).
- [x] nimbo: the input source on Windows (the foreground window's layout, `GetKeyboardLayoutList`, polled every 200 ms while watched) and on Linux under GNOME (GSettings `org.gnome.desktop.input-sources`) and Plasma (`org.kde.keyboard` over D-Bus), a layout's language and name from XKB's `evdev.xml`. Elsewhere `available()` is false and the app offers no Keyboard layout. nimbo's library code: +452 of 600.
- [x] The app watches the input source wherever nimbo tells it.
- [x] A probe on nimbo alone (`isprobe`: the sources, the one in use, its changes) in the M2's VMs, switching by a key press (GNOME, Super+Space through uinput), D-Bus (Plasma) or `WM_INPUTLANGCHANGEREQUEST` (Windows): Windows 11 ARM64, Ubuntu 24.04 GNOME 46 on X11 and on Wayland, Kubuntu 24.04 Plasma 5.27 on X11 and on Wayland, Kubuntu 22.04 Plasma 5.24 on X11: each lists its layouts with their languages, tells the one in use and each switch.
- Not run: wavo-app itself on Windows and Linux (a whole build there); a window manager without GNOME or Plasma, Sway, Hyprland (none tell the layout).

## Round 8

- [x] wavo-app itself, built from these branches in the M2's VMs, five dictations each with `WAVO_FAKE_MIC` and `wavo-app toggle`, the result read from `history.db`: English layout to parakeet-v2 in `en`, Russian to gigaam-v3 in `ru`, English switched to Russian while recording to gigaam-v3 in `ru`, German (no model) to parakeet-v2 in `de` (not sent), Auto-detect with the Russian layout to parakeet-v2 without a language. All five right on Ubuntu 24.04 GNOME 46 (X11, Wayland), Kubuntu 24.04 Plasma 5.27 (X11, Wayland) and Windows 11 ARM64; the HUD's EN and RU and the settings' rows on the screenshots, German's row gone at once when its layout is removed. On GNOME's Wayland the HUD (XWayland's popup there) didn't show on the screenshots, nor did that of a build from before this work.
- [x] nimbo's review: a GNOME variant (`us+dvorak`) named and given its language by the variant's own registry entry, Plasma's ids apart for two variants of a layout, GSettings read after its handler connects, a failed poll thread on Windows not left as watched. Known and documented: GNOME's per-window sources aren't followed, Plasma 5 with one layout tells none, two Windows layouts of one language share its name.
