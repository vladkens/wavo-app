.PHONY: prepare check test build update clean

CARGO_FLAGS := --release --locked
# The binary's extension: .exe on Windows, where make sees OS=Windows_NT.
EXE := $(if $(filter Windows_NT,$(OS)),.exe)

prepare:
	cargo +nightly fmt
	cargo clippy $(CARGO_FLAGS) --fix --all-targets --allow-dirty --allow-staged

check:
	cargo +nightly fmt --check
	cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings
	$(if $(CROSS),cargo-zigbuild clippy $(CARGO_FLAGS) --keep-going --all-targets \
	  $(addprefix --target ,$(CROSS)) -- -D warnings)

test:
	cargo test $(CARGO_FLAGS)

build:
	cargo build $(CARGO_FLAGS)
	ls -lh target/release/wavo-app$(EXE)

update:
	cargo upgrade --incompatible
	cargo update

clean:
	cargo clean

# wavo-app
.PHONY: bundle dist pack run run-copy install uninstall shots fixtures test-models

APP := target/Wavo.app
PROFILE := release
# FLAVOR=dev: Wavo Dev, `make run`'s, an app of its own beside the installed Wavo: its bundle id
# and data folder me.vladkens.wavo-dev, so its own permissions, login item and history; it never
# updates itself. The binary takes it from WAVO_FLAVOR (build.rs), the bundle from shipo's
# overrides, with an orange icon made into target/ (scripts/dev-icon.py).
DEV := $(filter dev,$(FLAVOR))
DEV_APP := target/Wavo Dev.app
DEV_ICON := target/dev-icon.png
DEV_SHIPO := --id me.vladkens.wavo-dev --name "Wavo Dev" --icon $(DEV_ICON)
# dist's target: this machine's, or T.
T ?= $(shell rustc -vV | sed -n 's/^host: //p')

# shipo, the packer (`cargo install --locked shipo`), makes the app and the release files from
# Cargo.toml's [package.metadata.shipo]. The app is signed with a Developer ID in the keychain, else
# "Wavo Local Signing", which shipo makes in the login keychain before the first bundle, so macOS
# keeps the grants across rebuilds; no bundle when it can't sign (CI: ad hoc). Built, never opened.
bundle: # target/Wavo.app from the release profile, or PROFILE; FLAVOR=dev: target/Wavo Dev.app
	$(if $(DEV),WAVO_FLAVOR=dev) cargo build --locked --profile $(PROFILE)
	$(if $(DEV),python3 scripts/dev-icon.py assets/icon.png $(DEV_ICON))
	shipo app --bin target/$(PROFILE)/wavo-app $(if $(DEV),$(DEV_SHIPO))

dist: # T's binary in target/distrib/T, for CI (vladkens/actions/rust-build); pack makes the files
	cargo build --locked --profile dist --target $(T)
	mkdir -p target/distrib/$(T)
	cp target/$(T)/dist/wavo-app$(EXE) target/distrib/$(T)/

pack: # the release files of target/distrib's binaries, signed by env's secrets, else ad hoc
	shipo release target/distrib $(if $(TAG),--tag $(TAG))

# scripts/run.py: the app's permissions are reset first when the bundle's signature is not the one
# they were granted to. Wavo Dev asks for its own permissions once, and starts with Wavo's hotkey,
# which a running Wavo holds: its menu says the hotkey is taken; set another in its settings.
run: # Wavo Dev beside the installed Wavo: quit the running Wavo Dev, start target/Wavo Dev.app
	$(MAKE) bundle FLAVOR=dev
	python3 scripts/run.py "$(DEV_APP)"

run-copy: # the same on a copy of the installed Wavo's history, which stays as it is
	$(MAKE) bundle FLAVOR=dev
	python3 scripts/run.py --copy-history "$(DEV_APP)"

install: # quit the running Wavo, install target/Wavo.app into /Applications and start it
	$(MAKE) bundle
	python3 scripts/run.py $(APP) /Applications/Wavo.app

# The installed app's binary, else the release build's, built again: `make run` leaves Wavo Dev's
# there. ARGS go to it (--dry-run, --yes).
INSTALLED := /Applications/Wavo.app/Contents/MacOS/wavo-app

uninstall: # remove Wavo and all it left (data, models, permissions, the app): then a clean install
	$(if $(wildcard $(INSTALLED)),,cargo build $(CARGO_FLAGS))
	$(or $(wildcard $(INSTALLED)),target/release/wavo-app) uninstall $(ARGS)

shots: # the settings window's pages and the update window's states, headless, into target/shots/
	WAVO_SHOTS=$(CURDIR)/target/shots cargo test $(CARGO_FLAGS) -- every_page_renders every_state_renders

# WAV fixtures and reference transcripts in target/fixtures/ (scripts/fixtures.py, macOS
# voices and ffmpeg); regenerated when the script or a text changes.
FIXTURES := target/fixtures/.stamp

fixtures: $(FIXTURES) # speech fixtures for test-models

$(FIXTURES): scripts/fixtures.py $(wildcard scripts/fixtures/*.txt)
	python3 scripts/fixtures.py target/fixtures
	touch $@

# The ignored tests that need real models from the Hugging Face cache or the fixtures, by module
# (not those that need a microphone); WAVO_TEST_MODEL_RU and WAVO_TEST_MODEL_EN pick the models by
# path.
MODEL_TESTS := engine::tests:: audio::tests::fixtures session::tests::the_tail_after_speech

test-models: fixtures # the ignored tests on real models
	cargo test $(CARGO_FLAGS) -- --ignored --test-threads=1 --nocapture $(MODEL_TESTS)
