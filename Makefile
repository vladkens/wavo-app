.PHONY: prepare check test build update clean

# macOS 14 is the least nimbo runs on; the bundle's Info.plist says the same (scripts/bundle.sh).
export MACOSX_DEPLOYMENT_TARGET := 14.0
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

.PHONY: bundle run install shots fixtures test-models unload-memory

ICON := target/Wavo.icns
APP := target/Wavo.app
PROFILE := release

$(ICON): assets/icon.png scripts/icns.sh
	scripts/icns.sh assets/icon.png $@

# target/Wavo.app (scripts/bundle.sh), signed with the identity $$WAVO_SIGN_IDENTITY when the
# keychain has it, ad hoc otherwise. Built, never opened.
bundle: $(ICON) # target/Wavo.app from the release profile, or PROFILE
	cargo build --profile $(PROFILE)
	scripts/bundle.sh target/$(PROFILE)/wavo-app $(ICON) $(APP)

run: # quit the running Wavo, start target/Wavo.app built with the quick fast profile
	$(MAKE) bundle PROFILE=fast
	scripts/run.sh $(APP)

install: # the same, installed into /Applications
	$(MAKE) bundle PROFILE=fast
	scripts/run.sh $(APP) /Applications/Wavo.app

shots: # the settings window's pages, headless, into target/shots/
	WAVO_SHOTS=$(CURDIR)/target/shots cargo test --release every_page_renders

# WAV fixtures and reference transcripts in target/fixtures/ (scripts/gen-fixtures.sh, macOS
# voices and ffmpeg); regenerated when the script or a text changes.
FIXTURES := target/fixtures/.stamp

fixtures: $(FIXTURES) # speech fixtures for test-models

$(FIXTURES): scripts/gen-fixtures.sh $(wildcard scripts/fixtures/*.txt)
	scripts/gen-fixtures.sh target/fixtures
	touch $@

# The ignored tests that need real models from the Hugging Face cache or the fixtures, by module
# (not those that need a microphone); WAVO_TEST_MODEL_RU and WAVO_TEST_MODEL_EN pick the models by
# path. The daemon's tests run target/release/wavo-app.
MODEL_TESTS := engine::tests:: session::tests:: audio::vad::tests::fixtures

test-models: fixtures # the ignored tests on real models
	cargo build --release
	cargo test --release -- --ignored --test-threads=1 --nocapture $(MODEL_TESTS)

# The headless app's footprint over 10 load and unload cycles of the first model found, with the
# malloc setting the bundle will have; fails when it grows by more than a few MB (macOS: uv and
# footprint).
unload-memory: fixtures # the footprint after unloading a real model, 10 times
	cargo build --release
	MallocSpaceEfficient=1 uv run scripts/unload-memory.py target/release/wavo-app target/fixtures/en-short.wav
