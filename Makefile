# Discoverable operator commands for SUM-Storage-Node-Protocol.
# Targets are thin wrappers around cargo / scripts; nothing is hidden.
# Run `make` (no args) to print this help.

.DEFAULT_GOAL := help
.PHONY: help fmt lint lint-strict test build release-check smoke audit-logs e2e-mirror

help:
	@printf "Targets:\n"
	@printf "  test                     cargo test --workspace\n"
	@printf "  fmt                      cargo fmt --check\n"
	@printf "  lint                     cargo clippy --workspace --all-targets (warnings allowed)\n"
	@printf "  lint-strict              cargo clippy --workspace --all-targets -- -D warnings\n"
	@printf "  build                    cargo build --release -p sum-node\n"
	@printf "  audit-logs               scripts/audit-logs.sh (privacy guardrail)\n"
	@printf "  release-check            fmt + lint-strict + test + build + audit-logs\n"
	@printf "  smoke RPC=URL            read-only chain smoke (extra args via SMOKE_ARGS=...)\n"
	@printf "                           e.g. make smoke RPC=http://localhost:8545 SMOKE_ARGS=--require-v2\n"
	@printf "  e2e-mirror               manual local-mirror E2E suite (assumes mirror running)\n"
	@printf "                           — requires WS2b commit 2; never part of release-check\n"

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
		printf "usage: make smoke RPC=<rpc-url> [SMOKE_ARGS=<args>]\n  e.g. make smoke RPC=http://localhost:8545 SMOKE_ARGS=--require-v2\n" >&2; \
		exit 2; \
	fi
	cargo run -p sum-node --bin e2e-helper -- smoke --rpc-url "$(RPC)" $(SMOKE_ARGS)

e2e-mirror:
	@printf "e2e-mirror: running local-mirror E2E suite (assumes mirror at http://localhost:8545,\n" >&2
	@printf "            funded via extra-alloc overlay matching e2e_keys/ at the SNIP repo root).\n" >&2
	@printf "            Each ignored test fails fast with actionable guidance if a precondition\n" >&2
	@printf "            isn't met. NOT part of release-check or PR CI.\n\n" >&2
	cargo test -p sum-node --test e2e_mirror -- --ignored --test-threads=1
