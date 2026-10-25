.PHONY: prepare check test

# Linux does not build the macOS shell, the only user of parts of the session: dead code is
# checked on macOS.
LINTS := -D warnings $(if $(filter Linux,$(shell uname -s)),-A dead_code)

prepare:
	cargo +nightly fmt
	cargo clippy --release --all-targets --fix --allow-dirty --allow-staged

check:
	cargo +nightly fmt --check
	cargo clippy --release --all-targets -- $(LINTS)

test:
	cargo test --release

.PHONY: install dist

APP := target/Wavo.app

# target/Wavo.app (scripts/bundle.py, signed with Wavo's own identity) into /Applications, started
# in place of the running Wavo (scripts/run.py).
install: # build, install and start Wavo
	cargo build --release
	uv run scripts/bundle.py target/release/wavo-app assets/icon.png $(APP)
	uv run scripts/run.py $(APP) /Applications/Wavo.app

dist: # target/distrib/Wavo-dev.zip with thin LTO: the release file CI builds
	CARGO_PROFILE_RELEASE_LTO=thin cargo build --release
	uv run scripts/bundle.py target/release/wavo-app assets/icon.png $(APP)
	mkdir -p target/distrib
	ditto -c -k --keepParent $(APP) target/distrib/Wavo-dev.zip
