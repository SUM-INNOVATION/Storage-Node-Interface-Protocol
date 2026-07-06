# Archive Node Mainnet Bring-Up

Concise field guide for operators standing up a SNIP archive node on
mainnet. Pairs with — and references — the canonical documents:

- [`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md) "Mainnet pin / deployed chain"
  — wire facts (`chain_id`, RPC, `v2_enabled_from_height`, TxPayload
  tags, intentionally-used RPC methods).
- [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md) — runbook with
  per-archive registration, recovery scenarios, monitoring, security
  posture.
- [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) § 7 —
  pre-final-release gates.

This guide does not duplicate those — it sequences the mainnet
bring-up specifically and adds the bits the runbook doesn't have
yet (three-archive coordination with placeholders, the throwaway
Public V2 round-trip with both success and below-quorum paths,
mainnet-specific failure triage).

## 0. What this does

- Registers and starts SNIP archive nodes against live mainnet.
- Brings the active-archive count to **at least three**, the chain
  plan's `assignment_replication_factor`. Below three, V2 ingest
  cannot complete — the file lands on chain in `Pending` and stays
  there until quorum is reached.
- Validates the bring-up with a single throwaway Public V2
  ingest + download round-trip.
- **Use throwaway data only.** Do not put customer or production
  data on mainnet until final-release criteria pass (see § 9).

## 1. Host prerequisites

Per-host, before `make release-check`:

- Rust toolchain matching `rust-toolchain.toml`. See the runbook's
  "Preflight" section for the exact pin.
- Network egress to `https://rpc.sumchain.io` (TLS over 443).
- Stable, non-cloud-synced disk path for the chunk store. Cloud-sync
  layers silently evict source files; see the runbook's
  "Preflight #2".
- Secure key file path (off the repo, off shared backups):

  ```bash
  openssl rand -hex 32 > /secure/path/archive-N.seed.hex
  chmod 600 /secure/path/archive-N.seed.hex
  ```

- Mainnet funds in the archive's L1 account: enough for tx fees
  (`register-encryption-key`, `register-node`) **plus** the
  `--stake 1_000_000_000` commitment.
- A funded **owner/client identity** distinct from the archive
  identities is **recommended** for the first ingest. Reusing an
  archive key as the ingest client can create local peer-identity
  collisions when the same host also runs `listen`; use a separate
  owner key for the canary. Generate fresh with
  `openssl rand -hex 32` and fund from your wallet, or derive a
  separate account from your mnemonic via the chain team's BIP-39
  tool.
- If this archive is to serve external peers: a stable QUIC UDP
  port reachable from the mesh. Pin via `--udp-port <N>`
  (see `sum-node listen --help`). LAN-only operators can skip.

## 2. Install SNIP at the current release-candidate tag

Each host:

```bash
git clone https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol.git
cd Storage-Node-Interface-Protocol
git checkout v0.4.0-rc4    # or the current release-candidate tag

make release-check
# fmt + lint + tests + release build + audit-logs; must end with `release-check: ok`
```

`make release-check` produces a release binary at
`target/release/sum-node`. Use that path in the commands below
(or substitute `cargo run --release -p sum-node --bin sum-node --` —
both forms work).

## 3. Read-only mainnet checks (no writes, no fees)

Run these in order. **Do not proceed past the first failure.**

### 3.1 — RPC reachability + chain_id + V2 enablement gate

```bash
make smoke RPC=https://rpc.sumchain.io SMOKE_ARGS=--require-v2
```

Expected output (eyeball — exact text matters):

```text
smoke target: https://rpc.sumchain.io
[1/2] chain_getChainParams ........... OK (chain_id=1, R=3, v2_enabled_from_height=Some(5200000))
[2/2] chain_getBlockHeight ........... OK (finalized height=<N>, finality=finalized)
V2 state: ENABLED_FROM_HEIGHT (v2_enabled_from_height=Some(5200000) → enabled at height 5200000)

(skipped: ...)

smoke: ok
```

### 3.2 — Funded balance on the operator's L1 address

```bash
cargo run --release -p sum-node --bin e2e-helper -- balance \
    --rpc-url https://rpc.sumchain.io \
    --address <archive-N-l1-address>
```

Expected: a non-zero integer string. Zero ⇒ funding flow incomplete.

### 3.3 — Three-archive gate (expected to FAIL pre-bootstrap)

```bash
cargo run --release -p sum-node --bin e2e-helper -- \
    active-nodes-at-height \
    --rpc-url https://rpc.sumchain.io \
    --height finalized \
    --require-archives 3
```

Before bootstrap: prints `active_archives: <0..2>` and exits 2.
Expected to start passing once § 4 has registered three archives.

## 4. Per-archive registration

Repeat for each of three distinct archive identities — substitute
`/secure/path/archive-N.seed.hex` (N ∈ {1, 2, 3}) and the
corresponding `<archive-N-l1-address>`. To derive the L1 address
from a seed:

```bash
cargo run --release -p sum-node --bin e2e-helper -- l1-address \
    --seed-hex "$(cat /secure/path/archive-N.seed.hex)"
```

Detailed field-level explanations live in
[`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md) § "Mainnet bring-up"
(steps 3–5). The summary:

### 4.1 — Smallest mainnet write canary: encryption-key registration

```bash
target/release/sum-node \
    --key-file /secure/path/archive-N.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    register-encryption-key
```

Stable stdout contract on success:

```text
tx_hash: 0x<hex>
finalized_height: <N>
```

### 4.2 — Archive registration

```bash
target/release/sum-node \
    --key-file /secure/path/archive-N.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    register-node --stake 1000000000
```

Same `tx_hash:` / `finalized_height:` stdout contract.

### 4.3 — Verify on chain

```bash
cargo run --release -p sum-node --bin e2e-helper -- node-record \
    --rpc-url https://rpc.sumchain.io \
    --address <archive-N-l1-address>
```

Expect:

```text
{
  "address": "<archive-N-l1-address>",
  "role": "ArchiveNode",
  "status": "Active",
  "staked_balance": 1000000000,
  ...
}
```

`null` ⇒ registration didn't finalize on this chain.

### 4.4 — Start serving

```bash
target/release/sum-node \
    --key-file /secure/path/archive-N.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    --profile production \
    --enable-wan \
    --udp-port 4242 \
    listen
```

`--profile production` is mandatory for mainnet — it fails closed
on every uncertain ACL path. `--profile dev` MUST NEVER touch
mainnet.

## 5. Three-archive bootstrap gate

Run from any host with mainnet egress (no key needed; this is
read-only):

```bash
cargo run --release -p sum-node --bin e2e-helper -- \
    active-nodes-at-height \
    --rpc-url https://rpc.sumchain.io \
    --height finalized \
    --require-archives 3
echo "exit=$?"
```

| Exit | Meaning | Action |
|---:|---|---|
| `0` | quorum met (≥ 3 archives) | proceed to § 6 |
| `2` | below threshold | bring more archives online (back to § 4) or wait for the in-flight registrations to finalize |
| `1` | RPC / wire failure | re-check § 3.1 smoke; investigate before retry |

The chain plan's `assignment_replication_factor = 3` makes this a
hard pre-flight: V2 ingest below quorum will register on chain but
never advance past `Pending` until three resolvable peers exist at
the snapshot height.

## 6. First throwaway Public V2 round-trip

This is the canonical "first real bytes on mainnet" milestone.
**Use throwaway content — operator personal data, NOT customer
data.**

### 6.1 — Create a throwaway payload

Random bytes are fine; the goal is byte-identical reassembly, not
content.

```bash
head -c 4096 /dev/urandom > /tmp/snip-canary.bin
shasum -a 256 /tmp/snip-canary.bin > /tmp/snip-canary.sha256
cat /tmp/snip-canary.sha256
# → <sha256>  /tmp/snip-canary.bin   (record this; we'll cross-check after download)
```

### 6.2 — Public V2 ingest

Use the **owner/client identity** from § 1, not an archive key:

```bash
target/release/sum-node \
    --client \
    --key-file /secure/path/owner.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    --chain-id 1 \
    --enable-wan \
    --bootstrap-peer /ip4/<archive-1-ip>/udp/4242/quic-v1/p2p/<archive-1-peer-id> \
    --bootstrap-peer /ip4/<archive-2-ip>/udp/4242/quic-v1/p2p/<archive-2-peer-id> \
    --bootstrap-peer /ip4/<archive-3-ip>/udp/4242/quic-v1/p2p/<archive-3-peer-id> \
    ingest-v2 /tmp/snip-canary.bin \
    --visibility public
```

The ingest's outcome depends on whether § 5 cleared. Two outcomes
are possible — both can occur during bootstrap, but only the first
counts as the release-promotion round-trip.

#### 6.2.a — Success path (≥ 3 archives active)

The pinned stable stdout contract is exactly two lines (the only
machine-parseable lines this command guarantees):

```text
merkle_root: 0x<hex>
lifecycle: Active
```

Tracing / log output may also report the chain tx hashes
(`RegisterFilePendingV2`, `ActivateFileV2` — the two writes the
ingest submitted), but those are **not** pinned as a stable stdout
contract; treat them as best-effort diagnostics. Capture any tx
hashes that appear for the release notes, alongside the
guaranteed `merkle_root:` line. Then proceed to § 6.3.

#### 6.2.b — Below-quorum path (< 3 archives active during ingest)

The chain accepted `RegisterFilePendingV2` (the file row exists)
but the S2 push wave couldn't satisfy `R=3`, so the file is on
chain in `Pending`. The CLI surfaces a tracing warn line followed
by the same two stable stdout lines, this time with `lifecycle:
Pending`:

```text
WARN sum_node::ingest_v2: S2 under-replicated chunks count=<N>
WARN sum_node: V2 ingest PENDING — file is registered on chain; run `resume` or `abandon`
              root=<hex> failed_stage=Push suggested=Resume
              under_replicated_count=<N> ...
merkle_root: 0x<hex>
lifecycle: Pending
```

If the command prints `lifecycle: Pending`, capture the
`merkle_root:` and `lifecycle:` lines and follow the resume or
abandon path. The chain row stays `Pending` until either:

- **Resume.** Bring the archive count to ≥ 3 (§ 4 + § 5),
  confirm via the § 5 gate, then re-drive the post-register
  portion:

  ```bash
  target/release/sum-node \
      --client \
      --key-file /secure/path/owner.seed.hex \
      --rpc-url https://rpc.sumchain.io \
      --chain-id 1 \
      resume <merkle-root-hex> /tmp/snip-canary.bin
  ```

  On success this prints `merkle_root: 0x<hex>` + `lifecycle:
  Active` and the file is fully activated. Continue to § 6.3.

- **Abandon** (only if the file is the wrong content / wrong
  recipients and you want the deposit back). Subject to the
  chain's grace-blocks rule; SNIP pre-checks chain state and
  emits a clean "wait until height N" message rather than
  burning a tx fee:

  ```bash
  target/release/sum-node \
      --client \
      --key-file /secure/path/owner.seed.hex \
      --rpc-url https://rpc.sumchain.io \
      --chain-id 1 \
      abandon <merkle-root-hex>
  ```

  Then re-do § 6.1 with fresh bytes (so you don't collide with
  the abandoned merkle root) and try again.

For the release-promotion gate (§ 9), only the **success path
(6.2.a) executed in a single run with ≥ 3 archives active during
ingest** counts. A `Pending → resume → Active` sequence is fine
for proving the bring-up but is a separate milestone for the
release notes.

### 6.3 — Download from a different identity to a fresh path

Public files are world-readable; `archive-2.seed.hex` here is just
an illustrative non-owner key proving the download isn't coupled
to the ingesting identity. Any non-owner seed file works —
including a fresh `openssl rand -hex 32` reader generated locally.

```bash
mkdir -p /tmp/snip-download
target/release/sum-node \
    --client \
    --key-file /secure/path/archive-2.seed.hex \
    --rpc-url https://rpc.sumchain.io \
    --chain-id 1 \
    --enable-wan \
    --bootstrap-peer /ip4/<archive-1-ip>/udp/4242/quic-v1/p2p/<archive-1-peer-id> \
    --bootstrap-peer /ip4/<archive-3-ip>/udp/4242/quic-v1/p2p/<archive-3-peer-id> \
    download <merkle-root-hex> \
    --output /tmp/snip-download/recovered.bin
```

### 6.4 — Verify byte-identical reassembly

```bash
cmp /tmp/snip-canary.bin /tmp/snip-download/recovered.bin && echo "byte-identical ✓"
# Or via hash:
shasum -a 256 /tmp/snip-download/recovered.bin
# Must match /tmp/snip-canary.sha256 exactly.
```

If `cmp` is silent and the recorded SHA-256 matches, this archive
fleet has cleared the canonical first-bytes milestone. Capture the
following for the release notes:

- `merkle_root` (from § 6.2 stdout — pinned).
- Any tx hashes printed by the CLI / tracing output for
  `RegisterFilePendingV2` and `ActivateFileV2` (or the resume
  activate, on the resumed path). These are not pinned stdout but
  are useful diagnostics.
- The three archive L1 addresses that were live during the
  successful ingest (or resume).

## 7. Private file readiness

Public V2 is the canonical first milestone. **Private mainnet
ingest is not a release-promotion gate** but is supported and
ready when needed.

Each account that wants to receive Private V2 file shares must run
`register-encryption-key`. The command can be re-run for the same
account, but each invocation still submits a tx and burns fees;
avoid unnecessary repeats. Recipients without a registered
encryption pubkey cause Private ingest to abort *before* any chain
state is created.

Owner-side ops — `share` / `revoke` / `update-access` — work as
documented in [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md)
"First-run sequence" and "Resume / abandon" sections. Do not
exercise them with customer data until § 9 promotion criteria
pass and Private V2 has its own throwaway round-trip on record.

## 8. Operational notes

- `--profile production` is mandatory; `--profile dev` MUST NEVER
  touch mainnet.
- Run the listener under `tmux` / `screen` for the manual canary,
  then move to `systemd` (or your supervisor of choice) only after
  every command in § 4 has succeeded interactively. A surprise
  reboot mid-`register-node` is recoverable; a surprise reboot
  mid-`listen` is recoverable; a surprise reboot mid-`ingest-v2`
  on customer data is not yet a tested path.
- Seed files: never in git, never in chat, never in tickets, never
  in plaintext backups. `chmod 600`; off-machine backup only if
  encrypted at rest.
- Mainnet `chain_id` is `1`. Local mirror `chain_id` is `1337`.
  If you ever see a smoke output reporting the wrong `chain_id`,
  you are pointed at the wrong RPC — STOP, do not write.
- The chain side's RocksDB / Docker / validator-binary
  distribution is chain-team operational territory and is **out
  of scope** for SNIP. SNIP only speaks JSON-RPC; nothing in this
  guide depends on the validator's storage shape.

## 9. Promotion criteria to final `v0.4.0`

The current release-candidate tag is not yet final production.
Promotion to a `v0.4.0` annotated tag requires **all four** gates
from [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) § 7 to pass:

1. Fresh-machine local-mirror E2E suite (11/11) clones the rc tag
   and runs the harness from a clean environment.
2. Mainnet read-only smoke green at `https://rpc.sumchain.io`.
3. `e2e-helper active-nodes-at-height ... --require-archives 3`
   exits `0` (≥ 3 ArchiveNode/Active rows on mainnet).
4. Throwaway Public V2 ingest + download round-trip succeeds
   byte-identically via the **success path (§ 6.2.a)** — i.e.
   the ingest ran with ≥ 3 archives active and the chain row went
   directly to `Active`. A `Pending → resume → Active` sequence
   (§ 6.2.b) does not satisfy this gate; it satisfies the
   bootstrap-readiness milestone but the release-promotion gate
   is the single-pass success.

When promoting:

- Record the four gate outcomes in the `v0.4.0` release notes.
- Record the mainnet `merkle_root`, any tx hashes the CLI / log
  output reported, and the three archive L1 addresses from § 6.
- Cut `v0.4.0` as a new annotated tag at the **latest
  release-candidate commit** that has passed all four gates —
  not necessarily `rc4`. If a follow-up rc lands docs or other
  changes after rc4 (this guide is itself one such follow-up),
  promote from the rc that includes them.

If any gate fails, fix on a follow-up branch, ship a higher rc,
repeat.

## 10. Failure triage

Targeted to mainnet bring-up. Recovery scenarios for ingest /
download / share / revoke runtime issues live in
[`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md) "Recovery scenarios"
— this section covers the bring-up-specific paths.

| Symptom | Likely cause | Fix |
|---|---|---|
| `make smoke` exits 1; `chain_id` ≠ `1` | wrong RPC URL; pointed at testnet / mirror | re-check the URL; do **not** proceed to any write |
| `make smoke` exits 1; `V2 state: PENDING (...)` | chain hasn't reached `v2_enabled_from_height` yet | wait for finality; re-check |
| `make smoke` exits 1; `V2 state: DISABLED` | chain returned `v2_enabled_from_height: null` | wrong chain or chain rolled back V2 — escalate to chain ops |
| `e2e-helper balance` returns `"0"` | funding flow incomplete | resolve out-of-band before any tx |
| `register-encryption-key` → `Failed(<code>)` | tx submitted but chain rejected | look up the chain's `Failed(N)` codes; common `40` means V2 not yet enabled at the finalized height (impossible if § 3.1 was clean — investigate clock drift) |
| `register-node` → `Failed(<code>)` | duplicate registration, insufficient stake, or stake below chain minimum | run `e2e-helper node-record` against your address; if already `Active`, the registration succeeded earlier and can be skipped |
| `register-*` `Dropped` | tx evicted from mempool before finality | re-run the same command; the CLI re-fetches a fresh nonce |
| `register-*` timeout | finality budget elapsed | check finalized head moved; if stuck, escalate to chain ops; otherwise re-run |
| `active-nodes-at-height ... --require-archives 3` exits 2 indefinitely | fewer than three archives registered, or some are `Pending` / `Slashed` | re-check via the human report (drop `--require-archives`) — the role × status breakdown shows which states the chain sees |
| `ingest-v2` prints `lifecycle: Pending` | bootstrap not complete (`active_archives < R`) OR a registered archive is not currently `listen`-ing | re-check § 5 gate. If the gate clears but ingest still goes Pending, ssh into each registered archive and confirm `listen` is actually running and reachable on its UDP port |
| `ingest-v2` `RegisterFilePendingV2 validity check failed` | chain rejected register | most likely the merkle root collides with an already-registered file — pick fresh throwaway bytes |
| `download` `ManifestFetchAllArchivesFailed` | all V2-assigned archives offline / wrong-rooted | wait for chain reassignment; re-run. If persistent, capture the structured fields and escalate |
| `download` hangs past 5 min | LAN / WAN egress mis-configured; archive(s) unreachable from your client | re-check archive `--udp-port` reachability; archive logs will show inbound chunk requests if your client is dialing |
| `resume` `RootMismatch` | `<merkle-root-hex>` doesn't match the file path passed to resume | re-check both: the recorded root from § 6.2.b's stdout, and the original file path you ingested |

For everything else, see the recovery-scenarios table in
[`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md).
