# Discoverable operator commands for SUM-Storage-Node-Protocol.
# Targets are thin wrappers around cargo / scripts; nothing is hidden.
# Run `make` (no args) to print this help.

.DEFAULT_GOAL := help
.PHONY: help fmt lint lint-strict test build release-check

help:
	@printf "Targets:\n"
	@printf "  test           cargo test --workspace\n"
	@printf "  fmt            cargo fmt --check\n"
	@printf "  lint           cargo clippy --workspace --all-targets (warnings allowed)\n"
	@printf "  lint-strict    cargo clippy --workspace --all-targets -- -D warnings\n"
	@printf "  build          cargo build --release -p sum-node\n"
	@printf "  release-check  fmt + lint-strict + test + build (run before tagging a release)\n"

fmt:
	cargo fmt --check

lint:
	cargo clippy --workspace --all-targets

lint-strict:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

build:
	cargo build --release -p sum-node

release-check: fmt lint-strict test build
	@printf "release-check: ok\n"
