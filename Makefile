.DEFAULT_GOAL := help
.PHONY: help bootstrap verify-components material-audit check test fmt fmt-check build run
# Locked components (components.lock.json) are hydrated by the SDK's Rust
# psoxide-components, built once per locked SDK revision into .tools/. It has to
# exist before Cargo can load this workspace, whose members are the hydrated
# paths; `cargo install --git` builds in its own temporary workspace, so the
# unhydrated tree does not get in the way.
SDK_REV    := $(shell sed -n '/"sdk": *{/,/"revision"/s/.*"revision": *"\([0-9a-f]*\)".*/\1/p' "$(CURDIR)/components.lock.json")
ifeq ($(SDK_REV),)
$(error no sdk revision found in "$(CURDIR)/components.lock.json")
endif
SDK_TOOLS  := $(CURDIR)/.tools/sdk-$(SDK_REV)
COMPONENTS := $(SDK_TOOLS)/bin/psoxide-components
XTASK      := $(SDK_TOOLS)/bin/xtask
# Extra psoxide-components / material-audit flags, e.g. COMPONENTS_ARGS="--source sdk=/path/to/PSoXide".
COMPONENTS_ARGS     ?=
MATERIAL_AUDIT_ARGS ?=
SDK_GIT    := https://github.com/EBonura/PSoXide

SDK_INSTALL = cargo install --locked --git $(SDK_GIT) --rev $(SDK_REV) --root "$(SDK_TOOLS)"

help:
	@echo "make bootstrap | check | test | build | run | material-audit"
bootstrap:
	@[ -x "$(COMPONENTS)" ] || $(SDK_INSTALL) psoxide-link
	"$(COMPONENTS)" --root "$(CURDIR)" $(COMPONENTS_ARGS)
verify-components:
	@[ -x "$(COMPONENTS)" ] || $(SDK_INSTALL) psoxide-link
	"$(COMPONENTS)" --root "$(CURDIR)" --check
material-audit:
	@[ -x "$(XTASK)" ] || $(SDK_INSTALL) xtask
	"$(XTASK)" material-audit --repo "$(CURDIR)" $(MATERIAL_AUDIT_ARGS)
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
