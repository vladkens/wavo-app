# Language switch

Goal: a bilingual person dictates each language with its own model (Russian with gigaam-v3, English with parakeet-v2) and switches with one extra hotkey: each press moves to the next language, also during a recording, and the app keeps it until the next press. The person, 2026-11-23..26: following the keyboard layout (v0.3.0) is tiring, macOS keeps the layout per window; a shortcut per language (#91) means hunting for free keys, and the settings got overloaded; multilingual models write short accented English in Cyrillic, and language detection is no fix (753 of the person's dictations: whisper-turbo right on 71% of English under 3 s, 0.8 s each); a key taken while the hotkey is held (Space) works not everywhere (not on Wayland, not in a tap-started recording) and looks odd. Wanted: one way that works everywhere, few controls, the last language remembered, a circle of languages.

Today (main at 955c768): `language` (a code, `auto` or `keyboard`) and `model`; with `keyboard`, `keyboard_models` per layout language and the HUD's language chip; nimbo pinned at 7ecb8b1, before several hotkeys (nimbo #90, merged: `set(&[Option<Hotkey>])`, `Edge { which }`, every platform and the Wayland portal).

## Decisions

- Settings: the Model and Language rows stay, the current pair. Under Language, the lines: a line is a language (Auto-detect too) and its model (the downloaded models that know it, best first, then "Download more…"), a tick on the current one, "✕" to remove it; then "Add a Language to Switch To…", which adds a line with the best downloaded model for it, the current pair first when there were none. Removing one of two leaves none. Language offers the lines' languages beside the model's; choosing a line's language or a model keeps the line in step. nimbo has no radio list, so a tick rather than a radio.
- With two lines or more, a "Switch language" shortcut row after Shortcut, recorded the same way, empty at first. Nothing else.
- Stored: `languages: [{language, model}]` in the cycle's order and `switch_hotkey`; `language` and `model` stay the current pair, so their readers stay as they are. A switch writes them to `settings.json` at once: app-wide, across restarts, nothing per window.
- The switch is nimbo's place 1 next to the dictation's place 0. Its press moves to the next line, after the last back to the first, in any state: idle (for the next dictation), recording held or latched, transcribing (for the next one). It never starts or stops a recording; its release and repeats do nothing.
- A recording goes whole to the line current when it closes (after the tail); the last switch wins, nothing is split. A switch loads the new model at once and restarts the live preview on it (`apply_settings`, as a layout change did).
- HUD: the chip (`RU`, `EN`) on every recording with two lines or more, popping on a change, as in keyboard mode; a switch while idle shows the pill's check with the code (`RU`, Auto for Auto-detect) for a second, as the time to text does.
- Menu: with two lines or more, the lines as checked items ("Russian · gigaam-v3", the current one on) in place of the Model submenu.
- One engine: a switch drops the loaded model and loads the other (about 0.1 s once prepared for the Neural Engine). Two engines only if the measure says so.
- nimbo: no new API. One fix in macOS's `lone`: within the 300 ms window, another place's chord aborts a modifier alone only when it has that modifier (⌥Space still aborts right ⌥; F6 no longer aborts fn), so fn then the switch at once keeps the dictation.
- Removed: Keyboard layout (`KEYBOARD`, `keyboard_models`, the input source watch, `Msg::Keyboard`, its rows and help texts), #91's shortcut per language (that PR is superseded; its several-hotkeys plumbing is kept).
- Migration in `Settings::load`: `language` and `model` become the one line; a stored `keyboard` becomes its rows as lines (a row without a model takes `model`), the current one the line of `model`, else the first; one row leaves no lines. Each load migrates the same way until the next save writes the new form (a load writes nothing).
- Not in this work: a switch held with a chord dictation hotkey on Windows and Linux (the OS matches a hotkey's modifiers exactly, so F6 with Ctrl+Alt held is Ctrl+Alt+F6), a modifier alone as the switch while fn is held (nimbo's `lone` wants no other modifier), a step back.

## Tasks

### 1. nimbo: a chord doesn't abort a modifier it lacks

- [x] `lone` takes the chords `judge` read; a press of another place's chord without the held key's modifier doesn't abort it. `judge` tests: fn then F6 within 300 ms (F6's `Down`, fn held), right ⌥ then ⌥Space (aborted, as now).

### 2. wavo-app: nimbo after #90

- [x] The pin past #90 and task 1: `set(&[dictation, switch])`, `Edge { which }`, per place errors in the menu (#91's `hotkey_errors`).

### 3. Settings and the session

Files: `src/settings.rs`, `src/session.rs`, `src/app.rs`.

- [x] `languages`, `switch_hotkey`, `next_language()`, the line's model kept with `model`; the migration, with tests (keyboard rows, a row without a model, plain settings, saved back).
- [x] Keyboard layout gone from settings, session and app.
- [x] Place 1's press → the switch, on the hotkeys' channel; `apply_settings`: the model loaded (warmed up while recording), the preview restarted.
- [x] Session tests: a switch while held and while latched loads the other model before the close and the entry has it; one in the tail counts; one while transcribing is for the next; idle; remembered in `settings.json`; one line binds no switch.

### 4. Window, menu, HUD

Files: `src/window.rs`, `src/menu.rs`, `src/hud.rs`, locales.

- [x] The lines under Language and the Switch Language row, the keyboard rows and texts gone; harness tests and shots; the 12 locales.
- [x] The menu's lines; their tests.
- [x] The chip's condition, the idle flash; their tests.

## Checks

- `make check` (with `CROSS=…` for Windows and Linux), `make test`, nimbo's `make check` and `make test`.
- The M2's VMs, wavo-app with `WAVO_FAKE_MIC`, keys posted, each dictation's model and language read from `history.db`: macOS (fn held + the switch while speaking, right after fn, idle, in a latched recording), Windows 11 and Kubuntu X11 (idle, latched). Not on the M2: the portal (Plasma 5.27 binds nothing).
- The time of a switch's model load on the M1 (`make test-models`), before deciding on two engines.
