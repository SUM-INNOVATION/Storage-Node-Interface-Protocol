# Storage-Node-Interface-Protocol

[![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-blue)](LICENSE-MIT) [![version 0.4.0-rc4](https://img.shields.io/badge/version-0.4.0--rc4-blue)](CHANGELOG.md) [![rust edition 2024](https://img.shields.io/badge/rust-2024-orange)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html) [![toolchain 1.85](https://img.shields.io/badge/toolchain-1.85-orange)](rust-toolchain.toml) [![SNIP V2 (chain plan v3.2)](https://img.shields.io/badge/SNIP-V2_chain_plan_v3.2-brightgreen)](#v2-lifecycle-chain-plan-v32) [![archive: Linux x86_64](https://img.shields.io/badge/archive-Linux_x86__64-blue)](docs/reference/PLATFORM-SUPPORT.md)

A native decentralized storage protocol for the SUM Chain blockchain. The L1 acts as a cryptographic ledger — storing only merkle roots, access lists, and fee pools — while actual file data lives off-chain in a libp2p P2P mesh of storage nodes. Nodes earn Koppa by proving they hold data through randomized Proof of Retrievability challenges, with 3x replication enforced by a deterministic assignment algorithm that both the L1 and storage nodes compute identically from on-chain state. No smart contracts, no IPFS dependency — storage economics are settled directly at the consensus layer.

---

## Platform support

Client mode (file user) and archive mode (long-running operator)
have different platform stories:

| Environment | Client | Archive |
|---|---|---|
| Linux | ✅ Supported | ✅ Supported |
| macOS (Apple Silicon) | ✅ Supported | ⚠️ Experimental |
| Windows (via WSL2) | ⚠️ With caveats | ❌ Not supported |
| ChromeOS (via Crostini) | ⚠️ With caveats | ❌ Not supported |

Archive operation is Linux-first; macOS may join after one
operator's long-run validation completes. Windows and ChromeOS
users run SNIP as clients through their Linux-compatible
environments (WSL2 / Crostini). For the full matrix, rationale,
per-environment setup recipes, promotion criteria, and items
not planned for `v0.4.x`, see
[`docs/PLATFORM-SUPPORT.md`](docs/reference/PLATFORM-SUPPORT.md).

---

## Install

Prebuilt binaries are published for **Linux x86_64**. Every other
supported platform builds from source — see
[`docs/INSTALL.md`](docs/reference/INSTALL.md) and
[`docs/PLATFORM-SUPPORT.md`](docs/reference/PLATFORM-SUPPORT.md) for
per-environment recipes.

The recommended first install is the **manual-verify path**
(download → check SHA256 → extract → move binaries). See
[`docs/INSTALL.md`](docs/reference/INSTALL.md) for the step-by-step
commands.

A curl-pipe convenience script is also published with each
release. It requires you to pin a version explicitly — there is
no `--latest`:

```bash
# Replace v0.4.0 with the release you want to install.
curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/v0.4.0/install.sh \
    | sh -s -- --version v0.4.0
```

The script installs `sum-node` and `e2e-helper` into
`$HOME/.local/bin` by default. It refuses to run on anything
other than Linux x86_64 and does not invoke `sudo` itself. To
install system-wide, run the curl-pipe under your own `sudo` and
pass `--prefix /usr/local`.

---

## V2 Lifecycle (chain plan v3.2)

Steps 0–8 above describe the **V2 protocol** (`/sum/storage/v2`) — the chain-canonical path on mainnet. This section consolidates the state-machine reference for V2, plus operator commands for recovering a stalled ingest (`resume`, `abandon`). The legacy **V1 protocol** (`/sum/storage/v1`) is preserved for backwards compatibility: V1 files have no per-push Merkle proof, no chain-recorded coverage bitmap, and rely on the `MarketSyncWorker` self-healing loop ([crates/sum-node/src/market_sync.rs](crates/sum-node/src/market_sync.rs)) plus V1 hash + linear-probe assignment ([crates/sum-store/src/assignment.rs](crates/sum-store/src/assignment.rs)).

Both protocols coexist on the same libp2p swarm. The `VersionedShardCodec` ([crates/sum-net/src/codec.rs:275-426](crates/sum-net/src/codec.rs#L275-L426)) dispatches per-stream on the negotiated protocol name; V1 wire bytes are bit-compatible with what nodes have been speaking since the project shipped. There is no automatic V2 → V1 fallback — a peer that doesn't advertise `/sum/storage/v2` surfaces as an `OutboundFailure` and the caller must retry V1 explicitly ([crates/sum-net/src/lib.rs:191-198](crates/sum-net/src/lib.rs#L191-L198)).

### V2 file lifecycle states

```
                         (file doesn't exist on chain)
                                      │
                                      │ RegisterFilePendingV2 finalizes
                                      ▼
                                  ┌────────┐
                                  │Pending │
                                  └────────┘
                  ActivateFileV2  │      │  AbandonFileV2
                  (all assigned   │      │  (after grace +
                   chunks         │      │   strict-> rule)
                   attested)      ▼      ▼
                              ┌──────┐ ┌──────────┐
                              │Active│ │Abandoned │
                              └──────┘ └──────────┘
```

`AbandonFileV2` is admissible only when `current_height > created_at + activation_grace_blocks` (chain plan v3.2 §3.5, strict greater-than). On success, the chain burns the configured `abandonment_fee_percent` of the fee deposit; the rest refunds to the owner ([sum-chain `crates/state/src/storage_metadata.rs:1654-1656`](https://github.com/SUM-INNOVATION/sum-chain/blob/main/crates/state/src/storage_metadata.rs#L1654-L1656)).

### V2 ingest walkthrough

Alice runs `sum-node ingest-v2 file.pdf`. The pipeline executes seven stages:

1. **Chunk locally** (same as V1: 1 MB chunks, BLAKE3 leaves, Merkle tree, `DataManifest`).
2. **`RegisterFilePendingV2`** ([crates/sum-node/src/tx_builder.rs:115-147](crates/sum-node/src/tx_builder.rs#L115-L147)) — Alice signs and submits a tx with merkle_root, chunk_count, fee deposit, visibility (`Public` or `Private`), and an initial access list (empty for Public; one `AccessEntryV2` per recipient for Private, including the owner). Wait for finalization. The chain captures `assignment_height = current_block_height` at this point — that's the snapshot that determines per-chunk assignment for the lifetime of the file.
3. **Read the snapshot** via `storage_getActiveNodesAtHeight(assignment_height)` (chain plan §5.3 / Ask 15).
4. **Push chunks with Merkle proofs inline.** Each push carries `(data, merkle_root, chunk_index, merkle_path)`. The receiving node validates four things before persisting: the file is registered and not Abandoned (`storage_getFileInfoV2`), `chunk_index < chunk_count`, the receiving archive is in the snapshot AND in the V2 deterministic assignment for this `chunk_index`, and `verify_merkle_proof_bytes_for_tree(blake3(data), chunk_index, merkle_path, merkle_root, chunk_count)` succeeds. Wire CID is never trusted — leaf hash is derived from `data`. Per (chunk, peer) retry budget is 2.
5. **`ManifestPush`** sends the CBOR manifest to each distinct assigned archive. Receivers re-derive the merkle_root from the manifest's chunk descriptors and reject any mismatch. Receivers ACK as soon as the manifest is persisted; attestation runs as a background spawn so inbound request latency is decoupled from chain finality.
6. **`AcceptAssignmentV2`** is what each archive submits after ManifestPush. It carries a list of `chunk_index: u32` values; chain OR-merges those bits into a per-(file, archive) bitmap. Files whose per-archive assignment exceeds `max_chunk_indices_per_tx` (default 65,536) split across multiple OR-merge txs that compose into the same bitmap.
7. **`storage_getAssignmentCoverageV2`** poll until `can_activate_now == true` (every chunk has at least one accepting archive that's currently `Active`), then submit **`ActivateFileV2`**. File transitions Pending → Active. PoR challenges become eligible after `activated_at_height + activation_grace_blocks`.

### Private V2 files

Set `--visibility private` on `ingest-v2` ([crates/sum-node/src/main.rs:182-197](crates/sum-node/src/main.rs#L182-L197)) and the entire chunk + manifest payload is encrypted end-to-end before anything touches the wire or an archive's disk. The encryption envelope lives in the [`sum-crypto`](crates/sum-crypto/) crate:

- **Per-file master key.** `K_file` = 32 random bytes from `OsRng` ([crates/sum-node/src/ingest_v2.rs:711-765](crates/sum-node/src/ingest_v2.rs#L711-L765)). Fresh per file.
- **Per-chunk AEAD.** Each chunk is encrypted with ChaCha20-Poly1305 under a key derived as `HKDF-SHA256(salt=chunk_index_be, ikm=K_file, info="snip-chunk-key-v1")`; the 12-byte nonce is derived the same way with `info="snip-chunk-nonce-v1"`; AAD = `chunk_index_be` ([crates/sum-crypto/src/chunk.rs:34-73](crates/sum-crypto/src/chunk.rs#L34-L73)). The ciphertext (with 16-byte tag) is what archives store on disk and what the manifest's `blake3_hash` commits to; the plaintext hash travels separately as `plaintext_blake3_hash` ([crates/sum-types/src/storage.rs:41-48](crates/sum-types/src/storage.rs#L41-L48)).
- **Manifest AEAD.** The CBOR manifest is encrypted under `HKDF-SHA256(salt="", ikm=K_file, info="snip-manifest-key-v1")` with an all-zero nonce and AAD = `b"snip-manifest-v1"` ([crates/sum-crypto/src/manifest.rs:46-72](crates/sum-crypto/src/manifest.rs#L46-L72)). Safe because `K_file` is fresh per file and the manifest is encrypted exactly once. Archives store the opaque ciphertext blob in a `<root>.opaque` sidecar instead of the public `.cbor` ([crates/sum-store/src/manifest_index.rs](crates/sum-store/src/manifest_index.rs)).
- **Per-recipient key wrap.** For each `--recipient <base58_addr[:expires_at_height]>`, the client fetches the recipient's registered X25519 public key via `account_getEncryptionPublicKey` ([crates/sum-node/src/rpc_client.rs:249-269](crates/sum-node/src/rpc_client.rs#L249-L269)), runs ephemeral X25519 ECDH, derives a KEK via `HKDF-SHA256(info="snip-recipient-kek-v1")`, and wraps `K_file` with ChaCha20-Poly1305 using the recipient's 20-byte L1 address as AAD. The resulting 80-byte bundle (`eph_pub(32) || ct(32) || tag(16)`) is what populates `AccessEntryV2.encrypted_key_bundle` on chain ([crates/sum-crypto/src/recipient.rs:112-156](crates/sum-crypto/src/recipient.rs#L112-L156)). Low-order X25519 points are rejected via constant-time comparison on both wrap and unwrap.
- **Recipient setup.** Each recipient must first publish their X25519 public key on chain via `sum-node register-encryption-key` ([crates/sum-node/src/main.rs:251-258](crates/sum-node/src/main.rs#L251-L258)). The key is derived deterministically from the recipient's Ed25519 wallet seed via HKDF (`info="snip-x25519-encryption-key-v1"` — [crates/sum-crypto/src/recipient.rs:82-95](crates/sum-crypto/src/recipient.rs#L82-L95)). The owner is auto-added to the initial access list; supplying additional `--recipient` flags makes the file owner-shared at registration time. Recipients without a registered encryption key cause ingest to abort **before** any chain state is written.

**Private download** ([crates/sum-node/src/download_private.rs](crates/sum-node/src/download_private.rs)) layers four extra steps on top of Step 8:
1. **Access lookup.** `find_my_access_entry` paginates the file's access list (256 entries per page, max 64 pages) looking for the downloader's own L1 address.
2. **Expiry check.** `finalized_height <= expires_at` (inclusive — no grace beyond `expires_at`) ([crates/sum-node/src/download_private.rs:163-207](crates/sum-node/src/download_private.rs#L163-L207)).
3. **Key unwrap.** Derive own X25519 secret from the Ed25519 seed via the same HKDF used at register-time, then `unwrap_for_self(bundle)` to recover `K_file` ([crates/sum-crypto/src/recipient.rs:160-198](crates/sum-crypto/src/recipient.rs#L160-L198)).
4. **Decrypt + verify.** Pull the opaque manifest, decrypt under `K_file`, rebuild the Merkle tree from the manifest's `plaintext_blake3_hash` descriptors and check against the chain-recorded root. Pull each ciphertext chunk (BLAKE3-verify against the manifest's `blake3_hash`), decrypt under `K_file`, verify the plaintext against `plaintext_blake3_hash`, then verify the assembled whole against `manifest.file_hash` ([crates/sum-node/src/download_private.rs:318-361](crates/sum-node/src/download_private.rs#L318-L361)).

**Sharing, revoking, and updating access** are owner-only operations:

- **`sum-node share <merkle_root> --recipient <addr[:height]|:none>`** ([crates/sum-node/src/main.rs:280-294](crates/sum-node/src/main.rs#L280-L294)) — the owner unwraps `K_file` from their own access bundle locally, re-wraps it for the new recipient's registered X25519 key, and submits `AddAccessV2` ([crates/sum-node/src/tx_builder.rs:218-230](crates/sum-node/src/tx_builder.rs#L218-L230)). The chain never sees `K_file`.
- **`sum-node revoke <merkle_root> --recipient <addr>`** ([crates/sum-node/src/main.rs:296-308](crates/sum-node/src/main.rs#L296-L308)) — submits `RemoveAccessV2` ([crates/sum-node/src/tx_builder.rs:242-257](crates/sum-node/src/tx_builder.rs#L242-L257)) removing the chain-side access entry. Does **not** rotate `K_file`: a revoked recipient still holds their old bundle locally, but the chain ACL denies them on the next pull. For forward secrecy, revoke + re-ingest under a fresh key.
- **`sum-node update-access <merkle_root> --recipient <addr:height|addr:none>`** ([crates/sum-node/src/main.rs:310-322](crates/sum-node/src/main.rs#L310-L322)) — submits `UpdateAccessV2` ([crates/sum-node/src/tx_builder.rs:269-286](crates/sum-node/src/tx_builder.rs#L269-L286)) to change only the entry's `expires_at`, byte-preserving the encrypted bundle. Requires an explicit `:<height>` or `:none` directive — a bare `<addr>` is rejected so operator intent is unambiguous.

### Resume and abandon

If anything after step 2 fails — a network partition, a slow chain, or a missing manifest ACK — the file is left `Pending` on chain. Two operator commands recover:

- **`sum-node resume <merkle_root> <path>`** — re-chunks the file (asserts the path's computed merkle_root equals the one passed; otherwise typed `RootMismatch`), reads chain state, and runs only the residual work. If the file is already `Active`, no-op (reports the chain-recorded heights). If `Abandoned`, terminal. Otherwise pulls `coverage.missing_indices` (paginated via `missing_offset`) and runs a partial push wave restricted to those chunk indices, then re-runs ManifestPush (idempotent on receivers), then waits for coverage, then activates.
- **`sum-node abandon <merkle_root>`** — pre-checks lifecycle is Pending and `current_height > created_at + activation_grace_blocks` before submitting (otherwise returns `NotAdmissible` with the earliest admissible height; saves a wasted tx fee). On success, the file is permanently Abandoned and 90 % of the deposit refunds.

### V2 vs V1 — when does each one fire?

| Protocol | Triggered by | Receiver behaviour |
|---|---|---|
| `/sum/storage/v1` | Legacy `sum-node ingest` and the V1 `--client ingest` path | Original V1 push: verify CID, write to disk, gossip an announcement. ACL is enforced on pulls but pushes have no chain-level proof. |
| `/sum/storage/v2` | `sum-node ingest-v2`, `resume`, `abandon`, `share`, `revoke`, `update-access`, plus the V2 dispatcher's manifest/chunk/manifest-push/manifest-pull replies | Chain-rooted: every push is Merkle-proof verified against chain state before disk write; every successful manifest push triggers an attestation tx; pulls run through the same ACL gate as V1. |

V2 is the chain-canonical path going forward. V1 stays operational for legacy traffic and read-only access to existing files; new ingest should use V2 to get chain-attestable replication.

---

## Summary

| Layer | What it knows | What it stores |
|-------|--------------|----------------|
| **Blockchain (Val 1, Val 2)** | File identities (merkle_root), ownership, access rules, fee pools, node registrations, challenge/proof history | Metadata only — never file data. ~100 bytes per file. |
| **Storage nodes (N1-N10)** | Which chunks they hold, which peers have what, their assignment | Actual file chunks on disk. ~3 MB per node for a 10 MB file with 10 nodes. |
| **Uploader (Alice)** | The merkle_root of her file | Nothing after upload — she can delete her local copy once R=3 confirmations are received per chunk. Alice is NOT a storage node. |
| **Downloader (Bob)** | The merkle_root (shared by Alice) | The reconstructed file after download. Bob is NOT a storage node. |

**No central server.** Files are spread across independent nodes that don't trust each other.

**No file data on-chain.** The blockchain stores 32 bytes of merkle_root per file, keeping it lightweight.

**Economic security.** Nodes post collateral (stake). Honest storage earns Koppa. Cheating costs Koppa. The math makes honesty the only profitable long-term strategy.

**Deterministic coordination.** No central coordinator assigns work. Every participant independently computes the same assignment from public on-chain data using the same hash function with the same inputs.

**Cryptographic integrity.** Every chunk is content-addressed (CID = hash of contents). Every Merkle proof is mathematically verifiable. You cannot fake a proof without possessing the actual data.

---

## CLI Reference

SNIP has two operator roles, exercised through the same `sum-node`
binary with different flags:

1. **Client / user (Alice, Bob).** Stores and retrieves files by
   paying storage and transaction fees. Does **not** register as an
   archive node, does **not** stake, does **not** run `listen`.
   Cannot earn rewards; cannot be slashed.
2. **Archive / operator (N1–N10).** Registers as `ArchiveNode` on
   chain, stakes `1_000_000_000` base units (1 Koppa), runs
   `listen`, stores chunks on behalf of clients, can earn rewards,
   can be slashed for protocol violations.

Mainnet operation requires at least `R = 3` archive operators
online (chain plan `assignment_replication_factor`). Clients can
store files once that bootstrap quorum exists.

**For external users (Alice/Bob) — client mode:**

| Command | What it does |
|---------|-------------|
| `sum-node --client --key-file <seed> --rpc-url <url> ingest-v2 <path> --visibility public` | V2 upload: chunk locally, push to R=3 V2-assigned archives, finalize on chain, clean up local chunks, exit |
| `sum-node --client --key-file <seed> --rpc-url <url> download <merkle_root> --output <path>` | Download a complete file by merkle root: manifest fetch, parallel chunk download, CID verification, merkle root verification, file reassembly, exit |

V2 (`ingest-v2`) is the chain-canonical path on mainnet and is what
clients should use for new files. V1 `--client ingest` is retained
for legacy traffic only.

**For storage node operators — node mode:**

One-time on-chain setup, then long-running `listen`:

| Command | What it does |
|---------|-------------|
| `sum-node --key-file <seed> --rpc-url <url> register-encryption-key` | One-time registration of the archive's X25519 encryption pubkey on chain (required to receive Private V2 file shares; safe to skip for Public-only operators, but recommended). One tx, no stake. |
| `sum-node --key-file <seed> --rpc-url <url> register-node --stake 1000000000` | One-time on-chain registration as `ArchiveNode` with the 1-Koppa stake commitment. Waits for finality. |
| `sum-node --key-file <seed> --rpc-url <url> --profile production listen` | Run as an archive node: serve chunks, enforce ACLs, respond to PoR challenges, run MarketSync + GC, dispatch V2 inbound when a signing key is present |
| `sum-node ingest <path>` | V1 upload (legacy): chunk, push to R=3 assigned nodes, stay running to serve chunks |
| `sum-node fetch <cid>` | Download a single chunk by CID from a LAN peer |
| `sum-node send <message>` | Broadcast a test gossipsub message |

For the full mainnet bring-up sequence — host prerequisites, fleet
coordination, first throwaway round-trip, failure triage — see
[`docs/operations/MAINNET-BRINGUP.md`](docs/operations/MAINNET-BRINGUP.md). For
chain-version compatibility and wire-format facts (including the
mainnet pin) see [`docs/reference/CHAIN-COMPAT.md`](docs/reference/CHAIN-COMPAT.md).

**For V2 lifecycle operations (chain plan v3.2 — Phase 0b):**

| Command | What it does |
|---------|-------------|
| `sum-node ingest-v2 <path> [--visibility public\|private] [--recipient <addr[:expires_at_height]>]...` | V2 ingest pipeline: chunk → `RegisterFilePendingV2` → push with Merkle proofs → ManifestPush → coverage poll → `ActivateFileV2`. `--visibility private` ([crates/sum-node/src/main.rs:182-187](crates/sum-node/src/main.rs#L182-L187)) generates a fresh `K_file`, encrypts every chunk + the manifest, and wraps `K_file` for the owner plus each `--recipient` ([crates/sum-node/src/main.rs:189-197](crates/sum-node/src/main.rs#L189-L197)). Each recipient's X25519 pubkey is fetched from chain via `account_getEncryptionPublicKey`; recipients without a registered key cause ingest to abort BEFORE any chain state is created. On any post-register failure the file is left `Pending` and the command exits with operator guidance to run `resume` or `abandon`. Requires `--key-file`. |
| `sum-node resume <merkle_root_hex> <path>` | Re-run only the residual V2 work for a `Pending` file. Re-chunks the path and asserts its computed root matches the explicit `merkle_root_hex`. Detects already-`Active` (no-op) and `Abandoned` (terminal) states. Requires `--key-file`. |
| `sum-node abandon <merkle_root_hex>` | Submit `AbandonFileV2`. Pre-checks lifecycle == Pending AND `current_height > created_at + activation_grace_blocks` before burning a tx fee; rejects in-process otherwise. Chain-only — does not require libp2p connectivity. Requires `--key-file`. |
| `sum-node share <merkle_root_hex> --recipient <addr[:expires_at_height\|:none]>` | Owner-only: add a recipient to a Private V2 file. Recovers `K_file` locally from the owner's own access bundle on chain, wraps it for the recipient's registered X25519 key, submits `AddAccessV2` ([crates/sum-node/src/tx_builder.rs:218-230](crates/sum-node/src/tx_builder.rs#L218-L230)). The chain never sees `K_file`. Recipients without a registered encryption key cause `share` to abort BEFORE any tx is submitted. Requires `--key-file`. |
| `sum-node revoke <merkle_root_hex> --recipient <addr>` | Owner-only: remove a recipient's chain-side access entry via `RemoveAccessV2` ([crates/sum-node/src/tx_builder.rs:242-257](crates/sum-node/src/tx_builder.rs#L242-L257)). Does NOT rotate `K_file` — the revoked recipient still holds their old bundle locally, but chain ACL denies them on the next pull. For forward secrecy, revoke + re-ingest. Requires `--key-file`. |
| `sum-node update-access <merkle_root_hex> --recipient <addr:expires_at_height\|addr:none>` | Owner-only: change a recipient's expiry on a Private V2 file via `UpdateAccessV2` ([crates/sum-node/src/tx_builder.rs:269-286](crates/sum-node/src/tx_builder.rs#L269-L286)). The encrypted bundle is preserved byte-for-byte; only `expires_at` changes. REQUIRES an explicit `:<height>` to set or `:none` to clear — a bare `<addr>` is rejected so operator intent is unambiguous. Requires `--key-file`. |

**Key flags:**
- `--client` — Run in client mode ([crates/sum-node/src/main.rs:90-95](crates/sum-node/src/main.rs#L90-L95)). Ingest pushes to R=3 and exits (no serving). Listen is not available.
- `--key-file <path>` — Ed25519 private key seed (hex-encoded; env `SUM_KEY_FILE`). Without it, generates a random keypair (dev mode, PoR + V2 ingest/resume/abandon/share/revoke/update-access disabled).
- `--profile <production|dev>` — fail-closed (production) vs fail-open (dev) policy on chain RPC errors. Production refuses to fall back to hardcoded `IngestParams` if `chain_getChainParams` fails; dev allows defaults with a warning.
- `--rpc-url <url>` — SUM Chain L1 JSON-RPC endpoint (default `http://127.0.0.1:9944`; env `SUM_RPC_URL`).
- `--chain-id <u64>` — chain id used to sign V1 + V2 transactions (default `1337`; env `SUM_CHAIN_ID`). Most subcommands accept the flag's value as-is; `register-node` reads `chain_id` live from RPC ([crates/sum-node/src/main.rs:2066](crates/sum-node/src/main.rs#L2066)) so the operator cannot mis-flag the tx against a different network and burn a fee.
- `--attest-fee <koppa>` — `u128` fee per V2 tx (RegisterFilePendingV2 / ActivateFileV2 / AbandonFileV2 / AcceptAssignmentV2). Default `1_000_000`; env `SUM_ATTEST_FEE`. Must be ≥ chain `min_fee`.
- `--por-poll-secs <secs>` — PoR challenge poll interval (default 10; env `SUM_POR_INTERVAL`). Controls how often the `PorWorker` calls `storage_getActiveChallenges`.
- `--market-sync-secs <secs>` — MarketSync poll interval (default 30; env `SUM_MARKET_SYNC_INTERVAL`). V1-legacy self-healing cadence.
- `--push-wait-secs <secs>` — V2 ingest S2 push-wave wall-clock timeout (default 120).
- `--manifest-push-wait-secs <secs>` — V2 ingest S3 manifest-push wall-clock timeout (default 60).
- `--activation-wait-secs <secs>` — V2 ingest S4 coverage-poll wall-clock timeout (default 300). NOT `activation_grace_blocks` — that's a chain-side parameter for `AbandonFileV2` admissibility, not for ingest progression.
- `--upload-timeout-secs <seconds>` — time to wait for R=3 push confirmations during V1 ingest (default 120) ([crates/sum-node/src/main.rs:152-153](crates/sum-node/src/main.rs#L152-L153)).
- `--gc-grace-secs <seconds>` — how long to keep unassigned chunks before garbage collection deletes them (default 3600 = 1 hour; env `SUM_GC_GRACE`).
- `--max-concurrent <n>` — maximum parallel chunk fetches during download (default 10).
- `--enable-wan` — enable Kademlia DHT + TCP transport for internet-wide peer discovery (env `SUM_ENABLE_WAN`).
- `--bootstrap-peer <multiaddr>` — bootstrap peer for Kademlia (repeatable or comma-separated; env `SUM_BOOTSTRAP_PEERS`).
- `--tcp-port <port>` — TCP listen port for WAN connections (default 0 = OS-assigned; env `SUM_TCP_PORT`).
- `--udp-port <port>` — UDP listen port for the QUIC transport (default 0 = OS-assigned; env `SUM_UDP_PORT`) ([crates/sum-node/src/main.rs:115-122](crates/sum-node/src/main.rs#L115-L122)). Pin a stable port for reliable WAN QUIC dialability — e.g. behind UPnP UDP port-forward, or to give DCUtR a fixed hole-punch target. With the default `0` the OS picks a fresh ephemeral UDP port on every restart.
- `--relay-server` — opt in to advertising this node as a libp2p Circuit Relay v2 server (only meaningful with `--enable-wan` and on a publicly-reachable host; env `SUM_RELAY_SERVER`).

---

## Garbage Collection

When nodes join or leave the network, the deterministic assignment recomputes (because the sorted node list changes). A node that was assigned to a chunk may no longer be assigned after a new node registers. Without garbage collection, that node holds the unassigned chunk indefinitely, wasting disk space.

The `GarbageCollector` runs automatically after each MarketSync cycle (every 30 seconds). It:
1. Enumerates all chunks on disk
2. Compares against the current assignment (computed from on-chain state)
3. Marks unassigned chunks with a timestamp
4. Deletes chunks that have been unassigned longer than the grace period (`--gc-grace-secs`, default 1 hour)

**Safety guarantees:**
- Never deletes a chunk that is currently assigned
- Respects the grace period to avoid thrashing from transient node list changes (e.g., a node briefly going offline and coming back)
- Pauses entirely if the L1 has not been polled within 5 minutes (avoids deleting based on stale assignment state)
- Logs every deletion with the CID and how long the chunk was unassigned

**What happens to the nodes Alice initially pushed to?**

In Step 3, Alice pushes each chunk to R=3 assigned nodes. These "entry nodes" are the first holders of the data. Over time, the network may change — new nodes join, old nodes leave, and the deterministic assignment recomputes. An entry node may no longer be assigned to the chunks it originally received from Alice.

When this happens, the GC treats entry nodes the same as any other node. There is no special retention for being the "first" to hold a chunk. The lifecycle is:

1. Alice pushes chunk 0 to N1, N3, N7 (all three are currently assigned)
2. N11 joins the network. Assignment recomputes. Chunk 0 is now assigned to N3, N7, N11.
3. N11's MarketSyncWorker detects it should hold chunk 0, fetches it from N3 or N7.
4. N1's GC marks chunk 0 as unassigned (N1 is no longer in the assignment for chunk 0).
5. After the grace period (default 1 hour), N1's GC deletes chunk 0 from its disk.

This is safe because N11 has already fetched the chunk within the 30-second MarketSync cycle — well before N1's 1-hour grace period expires. The grace period exists precisely to ensure this ordering: new assignment holders fetch the data before old holders delete it.

The GC does not track how a chunk was acquired (push from Alice, fetch from MarketSync, or local ingest). All chunks are stored identically on disk as `<cid>.chunk` files with no provenance metadata. The only thing that determines whether a chunk stays or goes is the current on-chain assignment.

---

## Network Modes

**LAN mode (default).** Without `--enable-wan`, peer discovery uses mDNS only. Two computers on the same WiFi or LAN will discover each other automatically. No bootstrap peers needed.

**WAN mode.** With `--enable-wan` and at least one `--bootstrap-peer`, nodes use Kademlia DHT for internet-wide peer discovery. TCP/Noise/Yamux transport is added alongside QUIC for NAT/firewall compatibility. Both mDNS (LAN) and Kademlia (WAN) run simultaneously — LAN peers are discovered instantly, WAN peers via DHT propagation.

```bash
# WAN example — Node B bootstraps off Node A across the internet.
# Pinning --udp-port matters: QUIC is always-on, and without a stable
# UDP port DCUtR cannot reliably hole-punch the relay circuit into a
# direct connection on restart ([crates/sum-node/src/main.rs:115-122](crates/sum-node/src/main.rs#L115-L122)).
sum-node --enable-wan --tcp-port 4001 --udp-port 4001 \
  --bootstrap-peer /ip4/<node_a_public_ip>/tcp/4001/p2p/<node_a_peer_id> \
  listen
```

**Firewall requirements:** TCP port (default OS-assigned, or set via `--tcp-port`) must be reachable for inbound WAN connections. QUIC/UDP port should also be open for optimal performance.

**NAT traversal (shipped):** AutoNAT detects whether this node is publicly reachable; nodes behind symmetric NATs reserve a slot on a Circuit Relay v2 (a publicly-reachable peer that's started with `--relay-server`) and DCUtR hole-punches the relay circuit into a direct QUIC connection. See [`docs/operations/OPERATOR-RUNBOOK.md`](docs/operations/OPERATOR-RUNBOOK.md) for the NAT state machine and relay configuration.
