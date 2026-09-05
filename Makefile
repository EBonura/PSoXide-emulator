.DEFAULT_GOAL := help
.PHONY: help bootstrap verify-components check test fmt fmt-check build run
help:
	@echo "make bootstrap | check | test | build | run"
bootstrap:
	python3 tools/bootstrap-components.py
verify-components:
	python3 tools/bootstrap-components.py --check
check: bootstrap
	cargo check --locked --workspace --all-features
test: bootstrap
	cargo test --locked --workspace
fmt: bootstrap
	cargo fmt --all
fmt-check: bootstrap
	cargo fmt --all -- --check
build: bootstrap
	cargo build --locked --release -p frontend
run: bootstrap
	cargo run --locked --release -p frontend

.PHONY: examples
examples: bootstrap
	$(MAKE) -f tools/sdk-examples.mk examples
