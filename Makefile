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
.PHONY: bundle dist run install uninstall signing shots fixtures test-models

APP := target/Wavo.app
PROFILE := release

# target/Wavo.app (scripts/bundle.py), signed with a Developer ID when the keychain has one (make
# signing), else with Wavo's own identity, "Wavo Local Signing", made in the login keychain before
# the first bundle; no bundle when it can't sign (CI: ad hoc). Built, never opened.
bundle: # target/Wavo.app from the release profile, or PROFILE
	cargo build --locked --profile $(PROFILE)
	python3 scripts/bundle.py target/$(PROFILE)/wavo-app assets/icon.png $(APP)

dist: # this OS's release file in target/distrib from the dist profile, for CI (vladkens/actions/rust-build)
	python3 scripts/dist.py

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

# scripts/signing-setup.py, the maintainer's, run twice: first the request for Apple's Developer ID
# certificate, then with CER= that certificate: the login keychain, the notary key and the repo's
# secrets that CI signs and notarizes the release with (scripts/sign.py).
signing: # set up the release signing, once (CER= Apple's certificate)
	python3 scripts/signing-setup.py $(CER)

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
