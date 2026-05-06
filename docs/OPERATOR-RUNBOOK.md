# Operator runbook

How to run, monitor, and recover a SNIP node. Pairs with
[`README.md`](../README.md) (design rationale) and
[`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) (release flow).

> All command examples use placeholders — `<merkle-root-hex>`,
> `<recipient-l1-address>`, `<owner-l1-address>`, `<tx-hash>`,
> `<archive-host>`, `<validator-host>`, `<path-to-key-file>`,
> `<chain-rpc-host>`. Substitute values for your environment.
> Real RPC URLs, validator/archive hostnames, funded addresses, and
> on-disk key paths MUST NOT appear in this file.

## Preflight

Before starting any node, confirm:

1. **Toolchain.** `rustc --version` matches
   [`rust-toolchain.toml`](../rust-toolchain.toml). The repo pins
   `stable`; pre-`1.85` toolchains will not build (`edition = "2024"`).
2. **Workspace path.** The repo MUST live on a non-iCloud, non-cloud-
   synced filesystem. Cloud-sync layers will silently evict source
   files and `Cargo.toml` artifacts during builds, causing
   `crate ... required to be available in rlib format` errors that
   survive `cargo clean`.
3. **Chain compatibility.** Read [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md).
   Confirm the internal chain release tag SNIP is built against
   matches the chain you're connecting to. Live-chain pinning is
   provided out-of-band by chain ops.
4. **Build.**
   ```bash
   make release-check     # fmt + lint-strict + test + release build
   ```
   Refuses to ship if anything is dirty.

## Key file

A SNIP node's identity is a 32-byte Ed25519 seed in hex. The same
seed:

- generates the libp2p peer ID;
- derives the L1 address;
- derives the X25519 encryption keypair via HKDF (domain
  `snip-x25519-encryption-key-v1`) for Phase 4a Private files.

Generate once, store outside the repo, restrict permissions:

```bash
openssl rand -hex 32 > <path-to-key-file>
chmod 600 <path-to-key-file>
```

Pass via `--key-file <path-to-key-file>` or `SUM_KEY_FILE=<path-to-key-file>`.
Without `--key-file` the binary generates an ephemeral random
keypair and runs in dev mode (PoR disabled, warning logged at
startup) — never acceptable in production.

> The seed never leaves disk; logs print only the derived peer ID
> and L1 address (verified by [`PRIVACY-AUDIT.md`](PRIVACY-AUDIT.md)).
> Treat the seed file like a wallet private key: off-machine
> backup, no chat, no tickets, no PRs.

## First-run sequence

> All commands assume `RUST_LOG=info` and the env vars below. Set
> them once to keep the rest of the doc copy-pastable.
>
> ```bash
> export SUM_KEY_FILE=<path-to-key-file>
> export SUM_RPC_URL=https://<chain-rpc-host>      # live
> # OR for local-mirror:
> # export SUM_RPC_URL=http://localhost:8545
> export RUST_LOG=info
> ```

Storage operator:

```bash
# 1. Top up the L1 account so it can pay fees.
#    (out of band — chain team faucet / staking flow)

# 2. Register the node on chain (requires V2 enabled — see CHAIN-COMPAT).
cargo run --release -p sum-node --bin e2e-helper -- \
    register-node --seed-hex $(cat $SUM_KEY_FILE) --rpc-url $SUM_RPC_URL

# 3. Register the X25519 encryption pubkey (required to receive
#    Private V2 file shares). Idempotent: re-running overwrites.
cargo run --release -p sum-node -- register-encryption-key

# 4. Start serving.
cargo run --release -p sum-node -- listen
```

Client / file owner:

```bash
# Public ingest (no encryption, world-readable).
cargo run --release -p sum-node -- \
    ingest-v2 ./photo.jpg --visibility public

# Private ingest, owner-only.
cargo run --release -p sum-node -- \
    ingest-v2 ./diary.txt --visibility private

# Private ingest, shared with two recipients (one with expiry).
cargo run --release -p sum-node -- \
    ingest-v2 ./report.pdf --visibility private \
    --recipient <recipient-l1-address> \
    --recipient <recipient-l1-address>:<expires-at-height>

# Download (auto-detects Public vs Private).
cargo run --release -p sum-node -- \
    download <merkle-root-hex> --output ./out.bin --max-concurrent 4

# Add another recipient post-ingest.
cargo run --release -p sum-node -- \
    share <merkle-root-hex> --recipient <recipient-l1-address>:<expires-at-height>

# Update an existing recipient's expiry (or clear it).
cargo run --release -p sum-node -- \
    update-access <merkle-root-hex> --recipient <recipient-l1-address>:<expires-at-height>
cargo run --release -p sum-node -- \
    update-access <merkle-root-hex> --recipient <recipient-l1-address>:none

# Revoke a recipient. Note: does NOT rotate K_file; for forward
# secrecy, revoke + re-ingest under a fresh key.
cargo run --release -p sum-node -- \
    revoke <merkle-root-hex> --recipient <recipient-l1-address>
```

## Resume / abandon

V2 ingest is multi-step: `RegisterFilePendingV2` → push chunks →
push manifest → coverage poll → `ActivateFileV2`. Failure between
steps leaves the file `Pending` on chain. The owner has two
recovery options:

- **Resume.** Re-runs the post-register portion against the same
  `<merkle-root-hex>`. Owner MUST pass the original file path so
  chunks can be re-derived for any missing pushes; mismatch
  surfaces as `RootMismatch`.
  ```bash
  cargo run --release -p sum-node -- \
      resume <merkle-root-hex> ./original/path
  ```
- **Abandon.** Submits `AbandonFileV2`, releasing the deposit (minus
  the chain-side abandonment fee). Subject to the chain's grace
  rule: refused before `created_at + activation_grace_blocks`. SNIP
  pre-checks chain state and emits a clean "wait until height N"
  message rather than burning a tx fee.
  ```bash
  cargo run --release -p sum-node -- abandon <merkle-root-hex>
  ```

Choose `resume` when the failure was transient (push timeout, peer
churn). Choose `abandon` when the file or recipients are wrong and
re-ingestion is required.

## Local mirror

Local-mirror setup is provided by chain ops out-of-band. Use the
artifact / runbook supplied for the target internal chain release.
The mirror is the source of truth for hermetic E2E validation; live
chain RPC is for read-only smoke only (see
[`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) §4).

## Monitoring

- **Logs.** `RUST_LOG` environment variable controls verbosity. The
  binary calls `EnvFilter::try_from_default_env`. Useful filters:
  - `info` (default) — operator-facing status.
  - `info,sum_node::download_private=debug` — debug just the Private
    download path.
  - `warn` — production noise floor.
- **Metrics.** [`metrics.rs`](../crates/sum-node/src/metrics.rs)
  exposes `MetricsSnapshot` (chunks served, peers connected, fetches
  in flight, GC churn). Currently log-only; a Prometheus exporter
  is on the roadmap. Inspect via
  `RUST_LOG=info,sum_node::metrics=debug`.
- **On-disk state.** All operator-facing state is under the configured
  store root (default: relative to the working directory). Chunk
  files, manifests (`<root>.opaque` for Private, `<root>.json` for
  Public), and the per-Private-file ACL sidecar
  (`<root>.private_chunks`) live there. Don't put the store root
  under cloud-sync.

## Recovery scenarios

| Symptom                                                        | Likely cause                                                     | Recovery                                                                                                                                            |
|----------------------------------------------------------------|------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------|
| Listen crashes mid-session                                      | bug or OOM                                                       | Restart `listen`; chunk store + manifests are crash-safe.                                                                                           |
| Ingest stalls past timeout                                     | transient peer churn                                             | `resume` with the same `<merkle-root-hex>` and original path.                                                                                        |
| Ingest fails before ActivateFile                                | recipient lacks encryption pubkey                                | Recipient runs `register-encryption-key`; retry ingest.                                                                                             |
| Download returns `ManifestFetchAllArchivesFailed`              | all V2-assigned archives offline / wrong-rooted                  | Wait for chain reassignment; retry. If persistent, file an issue with the structured fields (`assigned_total`, `tried`, `resolvable`, `unresolvable`, `last_reason`). |
| Download returns `NoAccess`                                    | not on the file's access list                                    | Owner must `share` to your address; you must have run `register-encryption-key` first.                                                              |
| Download returns `AccessExpired`                               | your access entry's `expires_at` ≤ finalized height              | Owner must `update-access` to extend or remove the expiry.                                                                                          |
| `register-encryption-key` says `V2 disabled`                    | chain returned `v2_enabled_from_height: null`                    | Connecting to a non-V2 chain. Check `CHAIN-COMPAT.md` and confirm the target tag.                                                                   |
| `cargo build` fails with `crate ... required to be available in rlib format` | cloud-synced workspace evicted artifacts          | Move repo off cloud-sync (see Preflight #2).                                                                                                        |

## Security posture (operator view)

- The seed file is the keys to the kingdom. `chmod 600`, off-machine
  backup, never check in.
- Public files are world-readable by design — treat as such.
- Private files: the chain stores wrapped key bundles (per recipient,
  `K_file` encrypted to that recipient's X25519 pubkey). Chain never
  sees `K_file` in plaintext.
- **Revocation does NOT rotate `K_file`.** A revoked recipient's
  on-disk cache of ciphertext + bundle remains decryptable. For
  forward secrecy, revoke + re-ingest under a fresh `K_file`.
- Logs do not contain seed, `K_file`, X25519 secret, or plaintext.
  Pinned by [`PRIVACY-AUDIT.md`](PRIVACY-AUDIT.md) and the audit
  guardrail in [`scripts/audit-logs.sh`](../scripts/audit-logs.sh)
  (lands in WS4).
