# Local development

Build SNIP from source, run the test suite, and exercise the node against a
local chain mirror. This is the contributor's entry point.

## Toolchain

SNIP is a Cargo workspace on the Rust 2024 edition, minimum toolchain 1.85. The
pinned toolchain is declared in [`rust-toolchain.toml`](../../rust-toolchain.toml)
(stable channel, with `rustfmt`, `clippy`, and `rust-src`). With rustup
installed, the correct toolchain is selected automatically when you build in the
repo.

## Build

The `Makefile` wraps the common commands; run `make` with no arguments to list
them. Nothing is hidden, each target is a thin wrapper over `cargo` or a script.

```bash
make build          # cargo build --release -p sum-node
make test           # cargo test --workspace
make fmt            # cargo fmt --check
make lint           # cargo clippy --workspace --all-targets (warnings allowed)
make lint-strict    # cargo clippy --workspace --all-targets -- -D warnings
```

The release binary lands at `target/release/sum-node`. The workspace also builds
a second binary, `e2e-helper`, used for test and operations helpers.

## The full pre-PR gate

Before opening a PR, run the same gate CI runs:

```bash
make release-check
```

This runs, in order: `fmt`, `lint-strict`, `test`, `build`, and `audit-logs`.
The last one is a privacy guardrail ([`scripts/audit-logs.sh`](../../scripts/audit-logs.sh))
that greps the source for log statements that could leak key material; it is
part of the release gate for a reason (see
[`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md)). Any failure blocks the
release.

## Workspace layout

Five crates (see [`architecture/OVERVIEW.md`](../architecture/OVERVIEW.md) for
the full map):

| Crate | Role |
|-------|------|
| `sum-types` | Shared types, config, RPC response shapes (leaf, no internal deps) |
| `sum-crypto` | AEAD chunk/manifest encryption, per-recipient key wrap (leaf) |
| `sum-net` | libp2p networking: transports, discovery, NAT traversal, wire codec |
| `sum-store` | Local chunk store, Merkle trees, assignment, GC, manifest index |
| `sum-node` | The binary: orchestrates the others, holds the CLI and workers |

## Running against a local chain mirror

Most integration work needs a running SUM Chain node. The node defaults to
`http://127.0.0.1:9944` for RPC (a chain node on the same host); the local
mirror in the chain repo typically serves on `http://localhost:8545`. The
mirror's `chain_id` is `1337` (mainnet is `1`).

### Read-only smoke check

Once a chain is reachable, confirm SNIP can talk to it:

```bash
make smoke RPC=http://localhost:8545
# or require V2 to be enabled:
make smoke RPC=http://localhost:8545 SMOKE_ARGS=--require-v2
```

This runs `e2e-helper smoke`, a read-only check that queries chain params and
height. It does not submit transactions.

### Full local-mirror E2E suite

The end-to-end suite drives real ingest and download against a running mirror:

```bash
make e2e-mirror
```

This assumes a mirror at `http://localhost:8545`, funded via the extra-alloc
overlay matching the `e2e_keys/` fixtures at the repo root. It is **not** part of
`release-check` or PR CI, and each ignored test fails fast with actionable
guidance if a precondition is not met. Standing the mirror up is covered in the
chain repo and referenced from
[`OPERATOR-RUNBOOK.md`](../operations/OPERATOR-RUNBOOK.md).

## Running a node from source

```bash
RUST_LOG=info cargo run --bin sum-node -- --key-file dev.hex listen
```

Omitting `--key-file` generates a random keypair in dev mode (PoR disabled),
which is fine for wire and discovery testing but cannot register or stake. For
local testing without a chain, `--profile dev` relaxes the fail-closed ACL
paths; never use it in a real deployment.

## See also

- [`architecture/OVERVIEW.md`](../architecture/OVERVIEW.md): the crate map and data flow
- [`RELEASE-CHECKLIST.md`](../operations/RELEASE-CHECKLIST.md): the full release flow
- [`CLI.md`](../reference/CLI.md): every flag and subcommand
