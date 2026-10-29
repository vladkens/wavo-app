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
.PHONY: bundle dist pack run install uninstall shots fixtures test-models

APP := target/Wavo.app
PROFILE := release
# Windows has python, where python3 may be the Store's stub.
PY := $(if $(filter Windows_NT,$(OS)),python,python3)
# nimbo's checkout at the rev Cargo.lock pins: its scripts/pack.py makes the app and the release
# files from Cargo.toml's [package.metadata.nimbo].
NIMBO = $(shell cargo metadata --format-version 1 --locked | $(PY) -c 'import json, os, sys; \
  print(next(os.path.dirname(p["manifest_path"]) for p in json.load(sys.stdin)["packages"] \
  if p["name"] == "nimbo"))')
# dist's target: this machine's, or T.
T ?= $(shell rustc -vV | sed -n 's/^host: //p')
GLIBC := 2.28

# Signed with a Developer ID in the keychain, else "Wavo Local Signing", made in the login keychain
# before the first bundle, so macOS keeps the grants across rebuilds; no bundle when it can't sign
# (CI: ad hoc). Built, never opened.
bundle: # target/Wavo.app from the release profile, or PROFILE
	cargo build --locked --profile $(PROFILE)
	$(PY) "$(NIMBO)/scripts/pack.py" app --bin target/$(PROFILE)/wavo-app

dist: # T's binary in target/distrib/T, for CI (vladkens/actions/rust-build); pack makes the files
	$(if $(findstring linux-gnu,$(T)),cargo zigbuild --locked --profile dist --target $(T).$(GLIBC),\
	  cargo build --locked --profile dist --target $(T))
	mkdir -p target/distrib/$(T)
	cp target/$(T)/dist/wavo-app$(EXE) target/distrib/$(T)/

NIMBO_REV = $(shell sed -n 's|^source = "git+https://github.com/vladkens/nimbo?.*\([0-9a-f]\{40\}\)"$$|\1|p' Cargo.lock)

pack: # the release files from target/distrib's binaries, signed with the secrets in env, else ad hoc
	curl -fsSL --create-dirs -o target/pack.py https://raw.githubusercontent.com/vladkens/nimbo/$(NIMBO_REV)/scripts/pack.py
	$(PY) target/pack.py release target/distrib $(if $(TAG),--tag $(TAG))

# scripts/run.py: Wavo's permissions are reset first when the bundle's signature is not the one
# they were granted to.
run: # quit the running Wavo, start target/Wavo.app
	$(MAKE) bundle
	python3 scripts/run.py $(APP)

install: # the same, installed into /Applications
	$(MAKE) bundle
	python3 scripts/run.py $(APP) /Applications/Wavo.app

# The installed app's binary, else the release build's; ARGS go to it (--dry-run, --yes).
INSTALLED := /Applications/Wavo.app/Contents/MacOS/wavo-app

uninstall: # remove Wavo and all it left (data, models, permissions, the app): then a clean install
	$(or $(wildcard $(INSTALLED)),target/release/wavo-app) uninstall $(ARGS)

shots: # the settings window's pages, headless, into target/shots/
	WAVO_SHOTS=$(CURDIR)/target/shots cargo test $(CARGO_FLAGS) every_page_renders

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
MODEL_TESTS := engine::tests:: audio::tests::fixtures

test-models: fixtures # the ignored tests on real models
	cargo test $(CARGO_FLAGS) -- --ignored --test-threads=1 --nocapture $(MODEL_TESTS)
