SHELL := /bin/sh

CARGO ?= cargo
BIN := termcmp
PACKAGE := termcmp
CRATE_PATH := crates/termcmp

.PHONY: help build release install install-shell doctor check test clippy fmt clean

help:
	@printf '%s\n' \
		'Targets:' \
		'  build          Build the workspace in debug mode' \
		'  release        Build the termcmp binary in release mode' \
		'  install        Install the local termcmp binary to ~/.cargo/bin' \
		'  install-shell  Install shell integration with the installed binary' \
		'  doctor         Run termcmp doctor from PATH' \
		'  check          Run cargo check --all-targets' \
		'  test           Run cargo test' \
		'  clippy         Run clippy with warnings denied' \
		'  fmt            Check rustfmt formatting' \
		'  clean          Remove Cargo build artifacts'

build:
	$(CARGO) build

release:
	$(CARGO) build --release -p $(PACKAGE)
	@# Strip provenance xattr + ad-hoc sign on macOS so Gatekeeper doesn't SIGKILL.
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		xattr -cr target/release/$(BIN); \
		codesign --force --sign - target/release/$(BIN); \
	fi

install:
	$(CARGO) install --path $(CRATE_PATH) --locked --force
	@# Strip provenance xattr + ad-hoc sign on macOS so Gatekeeper doesn't SIGKILL.
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		xattr -cr "$${CARGO_HOME:-$$HOME/.cargo}/bin/$(BIN)"; \
		codesign --force --sign - "$${CARGO_HOME:-$$HOME/.cargo}/bin/$(BIN)"; \
	fi

install-shell:
	$(BIN) install

doctor:
	$(BIN) doctor

check:
	$(CARGO) check --all-targets

test:
	$(CARGO) test

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --check

clean:
	$(CARGO) clean
