.PHONY: prepare check test build update clean

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

build:
	cargo build --release
	ls -lh target/release/wavo-app

update:
	cargo upgrade --incompatible
	cargo update

clean:
	cargo clean

.PHONY: bundle dist run install reset shots fixtures test-models

APP := target/Wavo.app
PROFILE := release

# target/Wavo.app (scripts/bundle.py), signed with Wavo's own identity, "Wavo Local Signing",
# made in the login keychain before the first bundle; no bundle when it can't sign (CI: ad hoc).
# Built, never opened.
bundle: # target/Wavo.app from the release profile, or PROFILE
	cargo build --profile $(PROFILE)
	uv run scripts/bundle.py target/$(PROFILE)/wavo-app assets/icon.png $(APP)

dist: bundle # target/distrib/Wavo-dev.zip, the release file for CI (vladkens/actions/rust-build)
	mkdir -p target/distrib
	ditto -c -k --keepParent $(APP) target/distrib/Wavo-dev.zip

# scripts/run.py: Wavo's permissions are reset first when the bundle's signature is not the one
# they were granted to.
run: # quit the running Wavo, start target/Wavo.app built with the quick fast profile
	$(MAKE) bundle PROFILE=fast
	uv run scripts/run.py $(APP)

install: # the same, installed into /Applications
	$(MAKE) bundle PROFILE=fast
	uv run scripts/run.py $(APP) /Applications/Wavo.app

# The installed app's binary, else the release build's; ARGS go to it (--dry-run, --yes).
INSTALLED := /Applications/Wavo.app/Contents/MacOS/wavo-app

reset: # delete Wavo's data and permissions: a clean install to test
	$(or $(wildcard $(INSTALLED)),target/release/wavo-app) reset $(ARGS)

shots: # the settings window's pages, headless, into target/shots/
	WAVO_SHOTS=$(CURDIR)/target/shots cargo test --release every_page_renders

# WAV fixtures and reference transcripts in target/fixtures/ (scripts/fixtures.py, macOS
# voices and ffmpeg); regenerated when the script or a text changes.
FIXTURES := target/fixtures/.stamp

fixtures: $(FIXTURES) # speech fixtures for test-models

$(FIXTURES): scripts/fixtures.py $(wildcard scripts/fixtures/*.txt)
	uv run scripts/fixtures.py target/fixtures
	touch $@

# The ignored tests that need real models from the Hugging Face cache or the fixtures, by module
# (not those that need a microphone); WAVO_TEST_MODEL_RU and WAVO_TEST_MODEL_EN pick the models by
# path.
MODEL_TESTS := engine::tests:: audio::tests::fixtures

test-models: fixtures # the ignored tests on real models
	cargo test --release -- --ignored --test-threads=1 --nocapture $(MODEL_TESTS)
