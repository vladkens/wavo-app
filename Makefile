.PHONY: prepare check test build update clean

prepare:
	cargo +nightly fmt
	cargo clippy --release --all-targets --fix --allow-dirty --allow-staged

check:
	cargo +nightly fmt --check
	cargo clippy --release --all-targets -- -D warnings

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

.PHONY: fixtures test-models

# WAV fixtures and reference transcripts in target/fixtures/ (scripts/gen-fixtures.sh, macOS
# voices and ffmpeg); regenerated when the script or a text changes.
FIXTURES := target/fixtures/.stamp

fixtures: $(FIXTURES) # speech fixtures for test-models

$(FIXTURES): scripts/gen-fixtures.sh $(wildcard scripts/fixtures/*.txt)
	scripts/gen-fixtures.sh target/fixtures
	touch $@

# Tests that need real models from the Hugging Face cache (#[ignore]); WAVO_TEST_MODEL_RU and
# WAVO_TEST_MODEL_EN pick them by path. One test runs target/release/wavo-app.
test-models: fixtures # the ignored tests on real models
	cargo build --release
	cargo test --release -- --ignored --test-threads=1 --nocapture
