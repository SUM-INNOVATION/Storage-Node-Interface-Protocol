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

## Mainnet bring-up

Sequence for bringing a brand-new archive operator onto live
mainnet. Read [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md) "Mainnet pin /
deployed chain" first — the values quoted below are operator-
visible facts confirmed at the pinned chain commit.

### 0. Mainnet vs local mirror

Confirm at runtime, never assume.

| Field                      | Mainnet                   | Local mirror              |
|----------------------------|---------------------------|---------------------------|
| `chain_id`                 | `1`                       | `31337`                   |
| RPC                        | `https://rpc.sumchain.io` | `http://localhost:8545`   |
| `v2_enabled_from_height`   | `5200000`                 | `0` (V2 from genesis)     |

Signing against the wrong `chain_id` means the chain rejects the
tx and the fee is burned. Always gate on `make smoke` before any
write — the smoke check fails loudly if `chain_id` ≠ what the
SNIP build expects.

### 1. Read-only smoke

No write, no fee, no risk. Confirms RPC reachability,
`chain_id`, V2 enablement state, and finality cadence.

```bash
make smoke RPC=https://rpc.sumchain.io SMOKE_ARGS=--require-v2
```

Expected output (truncated):

```text
chain_id=1, R=3, v2_enabled_from_height=Some(5200000)
finalized height=<N>, finality=finalized
V2 state: ENABLED_FROM_FINALIZED_HEIGHT (active since 5200000)
smoke: ok
```

If the smoke check fails, do NOT proceed to any write step.

### 2. Balance check (canary, costs nothing)

Before submitting any tx, confirm the operator's funded address
actually carries balance. A zero balance means the funding flow
hasn't completed; submitting a tx would fail at the chain side
with `InsufficientBalance`.

```bash
cargo run -p sum-node --bin e2e-helper -- balance \
    --rpc-url https://rpc.sumchain.io \
    --address <operator_l1_address>
```

A non-zero, integer balance string is the success condition.

### 3. Smallest write canary — register encryption key

`register-encryption-key` is the lowest-blast-radius mainnet
write: one tx, no stake, idempotent on chain (re-running with the
same seed is a fee-burning no-op). It validates the operator's
end-to-end signing + finality flow before any stake is committed.

```bash
sum-node \
    --key-file /secure/path/archive.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    register-encryption-key
```

Stable stdout contract on success:

```text
tx_hash: 0x<hex>
finalized_height: <N>
```

If this command surfaces `Failed`, `Dropped`, or a timeout,
investigate before any larger write — the same failure mode will
hit `register-node` for non-recoverable reasons (wrong chain_id,
insufficient balance, RPC drift).

### 4. Archive registration

Once the encryption-key write has finalized cleanly:

```bash
sum-node \
    --key-file /secure/path/archive.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    register-node --stake 1000000000
```

`--stake` is the chain-side stake commitment. The default
matches the local-mirror fixture; mainnet operators should set
this per the chain team's published minimum (out-of-band).

### 5. Verify node record on chain

```bash
cargo run -p sum-node --bin e2e-helper -- node-record \
    --rpc-url https://rpc.sumchain.io \
    --address <archive_l1_address>
```

The response should show
`{ role: "ArchiveNode", status: "Active", staked_balance: <N>, ... }`.
A `null` response means the registration didn't finalize on this
chain — re-check finality status before assuming the archive is
operational.

### 6. Start serving

```bash
sum-node \
    --key-file /secure/path/archive.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    --profile production \
    listen
```

`--profile production` fails closed on every uncertain ACL path;
never use `--profile dev` against mainnet.

### Hard pre-flight before first ingest: ≥ 3 archives

The chain plan fixes `assignment_replication_factor = 3`. **Every
ingest's S2 push wave needs three distinct archive identities,
all currently registered AND listening, before V2 ingest can
activate.** A single archive registration is not enough; the
chain will accept `RegisterFilePendingV2` but the file will get
stuck in `Pending` because S2 cannot satisfy R=3.

Mainnet currently has **zero** registered archive nodes. Before
attempting the first mainnet ingest:

1. Confirm at chain head that ≥ 3 ArchiveNode/Active rows exist:

   ```bash
   curl -s -X POST https://rpc.sumchain.io \
       -H "Content-Type: application/json" \
       -d '{"jsonrpc":"2.0","id":1,"method":"chain_getBlockHeight","params":["finalized"]}' \
       | jq -r '.result.height'
   # → <H>

   curl -s -X POST https://rpc.sumchain.io \
       -H "Content-Type: application/json" \
       -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"storage_getActiveNodesAtHeight\",\"params\":[<H>]}" \
       | jq '[.result[] | select(.role=="ArchiveNode" and .status=="Active")] | length'
   # → must be ≥ 3 before first ingest
   ```

2. Pick one of two operational shapes for the bootstrap:

   **Option A — coordinated ramp.** Three independent operators
   on three hosts each register and start `listen` within a
   tight window. Each MUST use a distinct seed (= distinct L1
   address), distinct store root, distinct ports. No shared
   key material across hosts.

   **Option B — single-org bootstrap.** One operator runs three
   archive identities on three hosts. Same constraints (distinct
   seeds, store roots, ports). Trust is concentrated until
   external operators onboard.

   Either shape is acceptable for unblocking the first mainnet
   ingest. Coordinated ramp is preferred for diversity; single-
   org bootstrap is faster to prove the path.

3. Do NOT run the first mainnet ingest until step 1 returns
   `≥ 3`. Until then, ingest will register on chain but never
   finalize as `Active`, and the deposit sits in `Pending`
   until grace-blocks pass and the file is abandoned.

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

The local mirror is a **disposable single-validator devnet** for
V2 client integration testing. It is NOT production, NOT a public
testnet, and NOT representative of liveness or security
characteristics. Use it only to validate that SNIP talks to the
chain wire shape correctly at the pinned commit
([`CHAIN-COMPAT.md`](CHAIN-COMPAT.md)).

Validator keys are generated on first boot into a Docker named
volume. No validator signing material, faucet privates, or test
seeds are committed to the chain repo, and SNIP must never
consume an artifact that ships keys. The same rule applies
prospectively to every future re-pin.

### Bring up

```bash
git clone <chain repo> sum-chain
cd sum-chain
git checkout 5ff6c7485bdfa1eb9143b8712cfb9c50ed6659e0  # current SNIP pin
docker-compose -f deploy/snip-local-mirror.yaml up -d --build
```

The first `up --build` takes about **10 minutes** for the cargo
release stage of the validator image. Subsequent builds reuse the
docker layer cache (rust:1.85-slim base) and complete in seconds
to a few minutes — this is normal, not hung.

### Health check (read-only)

```bash
make smoke RPC=http://localhost:8545
```

Expected:

- `chain_getChainParams` returns `chain_id = 31337` and
  `v2_enabled_from_height = 0` → SNIP reports
  `V2 state: ENABLED_FROM_GENESIS`.
- `chain_getBlockHeight(["finalized"])` returns
  `finality = "finalized"` and a non-zero, advancing height.
  Blocks advance approximately every 2 seconds.

### Stop / wipe

`stop` / `start` is the **iteration default** — pause and resume the
same chain, same validator key, same chain DB. The chain mirror's
entrypoint enforces this: an existing chain DB always resumes;
genesis is never silently regenerated. Use `down -v` only when you
genuinely want a clean slate (fresh genesis, fresh validator key).

| Command | Effect |
|---|---|
| `docker-compose -f deploy/snip-local-mirror.yaml stop`     | Pause. Chain DB + validator key untouched. **Default for iteration.** |
| `docker-compose -f deploy/snip-local-mirror.yaml start`    | Resume the paused chain. Same height, same key, same state. |
| `docker-compose -f deploy/snip-local-mirror.yaml down`     | Stop and remove the container; preserves named volumes. Equivalent to `stop` + container cleanup. |
| `docker-compose -f deploy/snip-local-mirror.yaml down -v`  | Wipe everything. Next `up` runs genesis + regenerates the validator key. **Only when intended.** |

### Funded test accounts (optional, fresh-genesis only)

The funding overlay is **optional** and **fresh-genesis-only** —
the mirror reads it once at genesis. If the chain DB already
exists (from a prior `up`), the overlay is ignored. To activate
an overlay against a running mirror, you MUST `down -v` first
to wipe the chain DB.

#### File format

The mounted overlay is a **pure JSON object** with **numeric**
balances (NOT strings). This is the chain mirror's load-bearing
schema:

```json
{
  "<base58-address>": <balance-in-base-units>,
  "<base58-address>": <balance-in-base-units>
}
```

A string-encoded balance (`"1000000000000"` instead of
`1000000000000`) fails to parse and the mirror starts without
the overlay applied — silently, from your perspective, until
you check balances and find them all zero.

#### Generating addresses

The SNIP repo ships an `e2e-helper generate-e2e-keys` command
that produces fresh seeds for the WS2b harness's roles
(`owner`, `recipient`, `third_party`, `archive_1`, `archive_2`,
`archive_3`) and emits a snippet matching the schema above. The
chain plan fixes `assignment_replication_factor = 3`, so the
harness needs **three** archive identities — registering /
listening only one or two leaves ingest unable to satisfy quorum:

```bash
cd <snip-repo>
cargo run --release -p sum-node --bin e2e-helper -- \
    generate-e2e-keys --out e2e_keys > /tmp/snip-alloc-snippet.json
cat /tmp/snip-alloc-snippet.json
# {
#   "<base58_owner>": 1000000000000,
#   "<base58_recipient>": 1000000000000,
#   ...
# }
```

The seed files in `e2e_keys/` are 0o600 and covered by
`.gitignore`. **Never commit them.** Once a generated seed is
funded by the overlay it is operational signing material.

#### Activating the overlay (fresh genesis)

Compose volume mounts MUST be declared in YAML — `docker-compose`
does not accept a `-v` flag for runtime volumes. Use a separate
compose override file:

1. Stop and wipe the running mirror (if it has booted before):
   ```bash
   cd <chain-repo>
   docker-compose -f deploy/snip-local-mirror.yaml down -v
   ```
2. Place the overlay file where the override mount expects it:
   ```bash
   cp /tmp/snip-alloc-snippet.json deploy/extra-alloc.json
   ```
   (Verify the JSON has numeric balances, not strings.)
3. Bring up with the override file. The override should mount
   `deploy/extra-alloc.json` into the mirror container at
   `/config/extra-alloc.json:ro`. Example
   `deploy/snip-local-mirror.override.yaml`:
   ```yaml
   services:
     mirror:
       volumes:
         - ./extra-alloc.json:/config/extra-alloc.json:ro
   ```
   Then:
   ```bash
   docker-compose \
       -f deploy/snip-local-mirror.yaml \
       -f deploy/snip-local-mirror.override.yaml \
       up -d --build
   ```

#### Verifying the overlay landed

After bringing the mirror up with the overlay, verify three
properties before running any test:

1. **`chain_id` returns `31337`** — confirms the mirror booted
   and the RPC endpoint matches the documented value.
2. **Block height advances** — confirms the validator is
   producing blocks (not stuck mid-genesis).
3. **Each WS2b role address has a non-zero balance** — confirms
   the overlay parsed and applied.

The first two are covered by `make smoke RPC=http://localhost:8545
SMOKE_ARGS=--require-v2`. For the per-address balance check:

```bash
cd <snip-repo>
for role in owner recipient third_party archive_1 archive_2 archive_3; do
    seed=$(cat e2e_keys/$role.seed.hex)
    addr=$(cargo run --quiet -p sum-node --bin e2e-helper -- \
        l1-address --seed-hex "$seed")
    bal=$(cargo run --quiet -p sum-node --bin e2e-helper -- \
        balance --rpc-url http://localhost:8545 --address "$addr")
    echo "$role  $addr  balance=$bal"
done
```

Every line must show a non-zero balance. If any role shows
`balance="0"`, the overlay didn't activate (most common cause:
the chain DB wasn't wiped before re-up, OR the JSON used string
balances instead of numbers).

#### Intentional hard-fails (read these before debugging)

The chain mirror's startup script applies two hard-fails that
catch the most common operator mistakes. Both print clear
`[snip-mirror] ERROR: …` messages explaining the right next
action — do NOT treat them as bugs:

- **Template-placeholder rejection.** The example overlay file
  ships with placeholder addresses. If your `extra-alloc.json`
  still contains any of them (almost always a copy-paste
  mistake where the operator forgot to swap in their own
  generated addresses), the validator refuses to boot. Replace
  every placeholder with a real base58 address from your
  `e2e_keys/` snippet and retry.
- **Existing-DB rejection.** If a chain DB volume already
  exists and an overlay is mounted, the validator refuses to
  boot. Genesis allocations cannot fund accounts retroactively.
  Either submit a transfer tx from an already-funded account,
  or `down -v` to wipe and re-bring-up with the overlay (see
  next subsection).

#### Re-funding without re-genesis

If the chain DB already exists and you only need ONE more funded
account (not a full re-genesis), submit a transfer transaction
from an existing funded account. The overlay route requires
`down -v`, which destroys all chain state.

The mirror is the source of truth for hermetic E2E validation;
live chain RPC is for read-only smoke only (see
[`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) §4 and §5). The
SNIP-side WS2 E2E suite that drives this mirror end-to-end is
the next workstream.

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
