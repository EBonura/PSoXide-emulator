.DEFAULT_GOAL := help
.PHONY: help check test fmt fmt-check build run
help:
	@echo "make check | test | build | run   (or just: cargo run --release)"
check:
	cargo check --locked --workspace --all-features
test:
	cargo test --locked --workspace
fmt:
	cargo fmt --all
fmt-check:
	cargo fmt --all -- --check
build:
	cargo build --locked --release -p psoxide-emulator
run:
	cargo run --locked --release -p psoxide-emulator
