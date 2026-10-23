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
