# Language shortcuts

Goal: more shortcuts besides the main one, each dictating in its own language with its own model (#73): ⌥Space with Auto-detect as today, ⌥⇧Space Russian with gigaam-v3, ⌃⌥Space English with parakeet-v2. The person, 2026-11-23, after a week of Keyboard layout (v0.3.0): switching the layout all the time annoys more than it helps; Auto-detect by default, and keys bound to a language or a model. For a person who speaks Russian mostly, gigaam-v3 is the better model by more than its error rate: smaller and faster.

## Decisions

- nimbo listens to several hotkeys at once (`.worktrees/hotkey-slots` there, branch `feat/hotkey-slots`): `Hotkeys::set(&[Option<Hotkey>])`, each edge saying its place (`which`), each place's error in `Status::slots`. Place 0 is the Shortcut, place `1 + i` the `i`th of More Shortcuts.
- Settings: `shortcuts: [{ hotkey, language, model }]`. `hotkey` is nimbo's grammar, empty while none is recorded; `language` a code or `auto`; `model` a model or `None` for the Model (shown as "parakeet-v3 (default)", as a keyboard language's). The Shortcut keeps Model and Language, Keyboard layout too.
- General, a section More Shortcuts under Dictation: a row per shortcut (its language, its model, its recorder, Remove), then Add Shortcut. Picking a language sets the model to the best downloaded for it (the keyboard rows' rule); its picker lists the default, the downloaded models that know the language, and Download more…, which opens Models in that language. A shortcut's model is in use: Models won't delete it.
- A hotkey that one place has is refused in another by nimbo; the row shows why under its keys (the row has no room for a mark beside them), the Shortcut's row with its warning mark, the menu a warning per hotkey that does not work.
- Session: a press starts the dictation of its place; the dictation goes by that place's model and language until the next press of another. One engine, as with Keyboard layout: a press of another place's shortcut loads its model while the person speaks. Transcribe Again goes by the place of the last dictation, as a dictation would now.
- Activation is the same for all. A press of any shortcut stops a latched recording; a held recording ends only by its own shortcut's release; another shortcut's press while one is held changes nothing.
- HUD: a shortcut of a language other than Auto-detect shows the language chip (`RU`), as Keyboard layout does.
- Not in this work: removing Keyboard layout (the person decides on the result), a shortcut per model alone without a language (a row with Auto-detect and a model is that).

## Tasks

- [x] 1. nimbo: several hotkeys (its plan, `docs/plans/20261123-hotkey-slots.md` there): vladkens/nimbo#90.
- [x] 2. Settings and session: `Shortcut`, `Settings::route(which, keyboard)`, `Input` with `which`, the machine's starter, the dictation's place, the HUD language. Tests: the route, the machine with two places, a session dictating with a second shortcut (`WAVO_FAKE_MIC`-like fake input) in its model and language.
- [x] 3. App: the places to nimbo, a menu warning per hotkey that does not work, the recorder's pause for any recorder.
- [x] 4. Window: More Shortcuts. Tests: add, pick a language (the model follows), record, remove; the model in use; a refused hotkey's warning; shots.
- [x] 5. Translations (`make i18n`), `make check`, `make test`; the VMs: macOS with two shortcuts, the dictation's model and language read from `history.db`. 2026-11-23: translated, `make check` and `make test` pass (built against nimbo's branch by a `[patch]` not committed). The M2's VMs, `WAVO_FAKE_MIC` with a Russian clip, keys posted: macOS 27.0.1 with ⌥Space (Auto-detect, parakeet-v3), ⌥⇧Space (Russian, gigaam-v3) and right ⌥ alone (English, parakeet-v2), each held, each dictation in its own model and language; right ⌥ then Space within 300 ms dictates as ⌥Space; ⌥⇧Space tapped latches, ⌥Space's press stops it in Russian; the HUD shows RU under ⌥⇧Space. Windows 11 ARM64 and Kubuntu 24.04 Plasma 5.27 on X11 with Ctrl+Alt+Space, F5 (Russian, gigaam-v3), F6 (English) and right Ctrl alone (Windows), each in its own; F5 held with F6 pressed meanwhile dictates as F5, then F6 held as F6. Not run: Wayland's portal (none on the M2's desktops binds a shortcut).
