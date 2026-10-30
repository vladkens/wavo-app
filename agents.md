# Wavo app

The dictation app (`wavo-app`, `publish = false`) for macOS, Windows and Linux: hold a hotkey, speak, release, and the text lands where you were typing; speech is recognized on the machine by [wavo](https://github.com/vladkens/wavo), the UI is drawn by [nimbo](https://github.com/vladkens/nimbo). Commands are in `wavo-app --help`, plans in `docs/plans/`.

## Dependencies

- wavo and nimbo are git dependencies on their `main`, pinned by `Cargo.lock`; a missing toolkit piece goes into nimbo, not around it; change dependencies only with `cargo add` / `cargo remove` / `cargo upgrade`, never by hand.
- To build against a local wavo checkout, add a `[patch."https://github.com/vladkens/wavo"]` to `.cargo/config.toml` and never commit it.
- The packer is the [shipo](https://github.com/vladkens/shipo) CLI, installed by `cargo install --locked shipo`; Cargo.toml's `[package.metadata.shipo]` is its config.
- Build and test with the `release` profile (`--release --locked`, as the Makefile does) so everything builds once; the `dist` profile is only for `make dist` in CI.
- zig is only for the lint: `cargo-zigbuild` runs only in `make check CROSS=…` (CI's cross clippy of the macOS and Windows code from Linux); never use zig or `cargo-zigbuild` to build binaries or release files or to pick the glibc version (a glibc floor goes through a pinned old-Debian sysroot with the normal toolchain).
- Release CI: the build jobs only compile and get no secrets (third-party build scripts run there); packing and signing happen only in the one publish job on Ubuntu (`make pack`, run by vladkens/actions rust-publish), never as an installer or signing step per build runner; no Wine.

## Rules

- Never open a window on the person's Mac without their permission: no GUI runs, no GUI tests; headless commands except `uninstall`, and tests, are free to run, and the HUD and the settings window are tested in nimbo's harness.
- `make bundle` builds `target/Wavo.app` and is free to run, but never open the app; `make run`, `make install` and `make uninstall` are the person's commands.
- Run `wavo-app uninstall` only with `--dry-run`: even under a temp `WAVO_HOME` it turns launch at login off, resets the macOS permissions and defaults, and deletes the real caches and the models in the Hugging Face cache.
- Tests and manual runs never touch the person's data: a temp `WAVO_HOME` every time.
- Never record from the real microphone (macOS would ask the person for permission): `WAVO_FAKE_MIC=<wav>` plays a file instead, and a test that needs a device is `#[ignore]`.
- Tests only read the Hugging Face cache (`make test-models` runs on its real models); anything that downloads gets a temp `HF_HOME`.
- No generated or binary files in git: screenshots and speech fixtures are made into `target/`; the app icon is the one binary.
- Start every `.rs` file with `// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app`.
- `make check` (nightly fmt, clippy with `-D warnings`) and `make test` pass before a pull request.
- A commit is one short lowercase line, no body.
