# Wavo app

The dictation app (`wavo-app`, `publish = false`): hold a hotkey, speak, release, and the text lands where you were typing; speech is recognized on this machine by [wavo](https://github.com/vladkens/wavo), the UI is drawn by [nimbo](https://github.com/vladkens/nimbo). macOS first. It comes over from its experiment by [the port plan](docs/plans/20261023-port.md), one step per PR.

## Dependencies

- wavo, like nimbo, is a git dependency on its `main`: `Cargo.lock` holds the commit and `cargo update -p wavo` moves it; a `[patch."https://github.com/vladkens/wavo"]` added to `.cargo/config.toml`, never committed, builds against a local wavo checkout.
- nimbo is a git dependency on its `main` until it is on crates.io: missing toolkit pieces go into nimbo, not around it.
- Change dependencies only with `cargo add` / `cargo remove` / `cargo upgrade`, never by hand.
- One profile, `release` (no LTO, incremental, stripped), so everything builds once; `make dist` adds thin LTO for the release file. It unwinds on a panic, as the engine thread needs; keep it so.

## Rules

- Never open a window on the person's Mac without their permission: no GUI runs, no GUI tests. Headless paths and unit tests are free to run; the HUD's tests draw it in nimbo's harness.
- `make bundle` builds `target/Wavo.app` and is free to run; never open it. `make run` and `make install` start the app: the person's commands.
- Tests and manual runs never touch the person's data: a temp `WAVO_HOME` every time.
- Never record from the real microphone (macOS would ask the person for permission): `WAVO_FAKE_MIC=<wav>` plays a file instead; a test that needs a device is `#[ignore]`.
- No generated or binary files in git: screenshots and speech fixtures are made into `target/`; the app icon is the one binary.
- Start every `.rs` file with `// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app`.
- `make check` (nightly fmt, clippy with `-D warnings`) and `make test` pass before a commit.
- `make test-models` runs the ignored tests on real models from the Hugging Face cache, which tests only read: anything that downloads gets a temp `HF_HOME`.
- Commit only when asked: one short lowercase line, no body.
