.DEFAULT_GOAL := help
.PHONY: help check test fmt fmt-check build run gate material-audit refresh-sdk check-sdk
# The SDK crates the emulator links are vendored (committed) under crates/ and
# sdk/, at the revision pinned in components.lock.json, so none of the targets
# below needs a bootstrap step: `cargo run --release` works on a fresh clone.
# refresh-sdk and check-sdk are the only targets that fetch anything. They build
# the SDK's psoxide-components exporter once per pinned revision into .tools/.
SDK_REV    := $(shell sed -n '/"sdk": *{/,/"revision"/s/.*"revision": *"\([0-9a-f]*\)".*/\1/p' "$(CURDIR)/components.lock.json")
ifeq ($(SDK_REV),)
$(error no sdk revision found in "$(CURDIR)/components.lock.json")
endif
SDK_TOOLS  := $(CURDIR)/.tools/sdk-$(SDK_REV)
COMPONENTS := $(SDK_TOOLS)/bin/psoxide-components
XTASK      := $(SDK_TOOLS)/bin/xtask
# The vendored paths are the lock's "paths" list.
VENDORED   := $(shell sed -n '/"paths": *\[/,/\]/p' "$(CURDIR)/components.lock.json" | grep -o '"[^"]*/[^"]*"' | tr -d '"')
# Extra psoxide-components / material-audit flags, e.g. COMPONENTS_ARGS="--source sdk=/path/to/PSoXide".
COMPONENTS_ARGS     ?=
MATERIAL_AUDIT_ARGS ?=
SDK_GIT    := https://github.com/EBonura/PSoXide

SDK_INSTALL = cargo install --locked --git $(SDK_GIT) --rev $(SDK_REV) --root "$(SDK_TOOLS)"

help:
	@echo "make check | test | build | run | gate | material-audit | refresh-sdk | check-sdk"
	@echo "(or just: cargo run --release)"
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

# The fleet gate (TESTING.md): every game's journey on the CPU and hardware
# renderers. GATE_ARGS e.g. "run nitroxide" or "run all --jobs 2".
GATE_ARGS ?= run all --strict
gate:
	cargo run --locked --release -p psoxide-gate -- $(GATE_ARGS)

material-audit:
	@[ -x "$(XTASK)" ] || $(SDK_INSTALL) xtask
	"$(XTASK)" material-audit --repo "$(CURDIR)" $(MATERIAL_AUDIT_ARGS)

# Re-export the vendored SDK crates from the pinned revision and stage them.
# Run it after changing the revision in components.lock.json. The exporter
# refuses to replace tracked files, so the old copies are removed first. The SDK
# workspace manifest lists every SDK crate; only the vendored ones are kept as
# members, so cargo can load it.
refresh-sdk:
	@[ -x "$(COMPONENTS)" ] || $(SDK_INSTALL) psoxide-link
	git rm -r -q --cached --ignore-unmatch -- $(VENDORED)
	rm -rf $(VENDORED)
	"$(COMPONENTS)" --root "$(CURDIR)" $(COMPONENTS_ARGS)
	@for name in $$(sed -n 's|^ *"crates/\(.*\)",$$|\1|p' sdk/Cargo.toml); do \
		[ -d "sdk/crates/$$name" ] || sed -i.bak "\|^ *\"crates/$$name\",$$|d" sdk/Cargo.toml; \
	done; rm -f sdk/Cargo.toml.bak .components-receipt.json
	git add -- $(VENDORED)

# Fails when the committed copies differ from the pinned revision (CI runs it).
check-sdk: refresh-sdk
	git diff --cached --exit-code HEAD -- $(VENDORED)
