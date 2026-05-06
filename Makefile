# Discoverable operator commands for SUM-Storage-Node-Protocol.
# Targets are thin wrappers around cargo / scripts; nothing is hidden.
# Run `make` (no args) to print this help.

.DEFAULT_GOAL := help
.PHONY: help fmt lint lint-strict test build release-check smoke audit-logs

help:
	@printf "Targets:\n"
	@printf "  test           cargo test --workspace\n"
	@printf "  fmt            cargo fmt --check\n"
	@printf "  lint           cargo clippy --workspace --all-targets (warnings allowed)\n"
	@printf "  lint-strict    cargo clippy --workspace --all-targets -- -D warnings\n"
	@printf "  build          cargo build --release -p sum-node\n"
	@printf "  audit-logs     scripts/audit-logs.sh (privacy guardrail)\n"
	@printf "  release-check  fmt + lint-strict + test + build + audit-logs\n"
	@printf "  smoke RPC=URL  read-only chain smoke against RPC (e.g. RPC=http://localhost:8545)\n"

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

audit-logs:
	scripts/audit-logs.sh

release-check: fmt lint-strict test build audit-logs
	@printf "release-check: ok\n"

smoke:
	@if [ -z "$(RPC)" ]; then \
		printf "usage: make smoke RPC=<rpc-url>\n  e.g. make smoke RPC=http://localhost:8545\n" >&2; \
		exit 2; \
	fi
	cargo run -p sum-node --bin e2e-helper -- smoke --rpc-url "$(RPC)"
