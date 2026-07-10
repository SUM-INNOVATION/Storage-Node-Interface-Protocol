# V2 state machine and lifecycle operations

Reference for the chain-plan-v3.2 file lifecycle SNIP submits to
the chain — `RegisterFilePendingV2` → push → `ActivateFileV2`, with
`resume` and `abandon` as recovery paths — and for the Private V2
encryption flow layered on top. The narrative walkthrough that puts
each step in a file's story lives in [`lifecycle.md`](lifecycle.md);
this file is the compact reference.

## V2 Lifecycle (chain plan v3.2)

Steps 0–8 in [`lifecycle.md`](lifecycle.md) describe the **V2 protocol** (`/sum/storage/v2`) — the chain-canonical path on mainnet. This section consolidates the state-machine reference for V2, plus operator commands for recovering a stalled ingest (`resume`, `abandon`). The legacy **V1 protocol** (`/sum/storage/v1`) is preserved for backwards compatibility: V1 files have no per-push Merkle proof, no chain-recorded coverage bitmap, and rely on the `MarketSyncWorker` self-healing loop ([crates/sum-node/src/market_sync.rs](../../crates/sum-node/src/market_sync.rs)) plus V1 hash + linear-probe assignment ([crates/sum-store/src/assignment.rs](../../crates/sum-store/src/assignment.rs)).

Both protocols coexist on the same libp2p swarm. The `VersionedShardCodec` ([crates/sum-net/src/codec.rs:275-426](../../crates/sum-net/src/codec.rs#L275-L426)) dispatches per-stream on the negotiated protocol name; V1 wire bytes are bit-compatible with what nodes have been speaking since the project shipped. There is no automatic V2 → V1 fallback — a peer that doesn't advertise `/sum/storage/v2` surfaces as an `OutboundFailure` and the caller must retry V1 explicitly ([crates/sum-net/src/lib.rs:191-198](../../crates/sum-net/src/lib.rs#L191-L198)).

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
2. **`RegisterFilePendingV2`** ([crates/sum-node/src/tx_builder.rs:115-147](../../crates/sum-node/src/tx_builder.rs#L115-L147)) — Alice signs and submits a tx with merkle_root, chunk_count, fee deposit, visibility (`Public` or `Private`), and an initial access list (empty for Public; one `AccessEntryV2` per recipient for Private, including the owner). Wait for finalization. The chain captures `assignment_height = current_block_height` at this point — that's the snapshot that determines per-chunk assignment for the lifetime of the file.
3. **Read the snapshot** via `storage_getActiveNodesAtHeight(assignment_height)` (chain plan §5.3 / Ask 15).
4. **Push chunks with Merkle proofs inline.** Each push carries `(data, merkle_root, chunk_index, merkle_path)`. The receiving node validates four things before persisting: the file is registered and not Abandoned (`storage_getFileInfoV2`), `chunk_index < chunk_count`, the receiving archive is in the snapshot AND in the V2 deterministic assignment for this `chunk_index`, and `verify_merkle_proof_bytes_for_tree(blake3(data), chunk_index, merkle_path, merkle_root, chunk_count)` succeeds. Wire CID is never trusted — leaf hash is derived from `data`. Per (chunk, peer) retry budget is 2.
5. **`ManifestPush`** sends the CBOR manifest to each distinct assigned archive. Receivers re-derive the merkle_root from the manifest's chunk descriptors and reject any mismatch. Receivers ACK as soon as the manifest is persisted; attestation runs as a background spawn so inbound request latency is decoupled from chain finality.
6. **`AcceptAssignmentV2`** is what each archive submits after ManifestPush. It carries a list of `chunk_index: u32` values; chain OR-merges those bits into a per-(file, archive) bitmap. Files whose per-archive assignment exceeds `max_chunk_indices_per_tx` (default 65,536) split across multiple OR-merge txs that compose into the same bitmap.
7. **`storage_getAssignmentCoverageV2`** poll until `can_activate_now == true` (every chunk has at least one accepting archive that's currently `Active`), then submit **`ActivateFileV2`**. File transitions Pending → Active. PoR challenges become eligible after `activated_at_height + activation_grace_blocks`.

### Private V2 files

Set `--visibility private` on `ingest-v2` ([crates/sum-node/src/main.rs:182-197](../../crates/sum-node/src/main.rs#L182-L197)) and the entire chunk + manifest payload is encrypted end-to-end before anything touches the wire or an archive's disk. The encryption envelope lives in the [`sum-crypto`](../../crates/sum-crypto/) crate:

- **Per-file master key.** `K_file` = 32 random bytes from `OsRng` ([crates/sum-node/src/ingest_v2.rs:711-765](../../crates/sum-node/src/ingest_v2.rs#L711-L765)). Fresh per file.
- **Per-chunk AEAD.** Each chunk is encrypted with ChaCha20-Poly1305 under a key derived as `HKDF-SHA256(salt=chunk_index_be, ikm=K_file, info="snip-chunk-key-v1")`; the 12-byte nonce is derived the same way with `info="snip-chunk-nonce-v1"`; AAD = `chunk_index_be` ([crates/sum-crypto/src/chunk.rs:34-73](../../crates/sum-crypto/src/chunk.rs#L34-L73)). The ciphertext (with 16-byte tag) is what archives store on disk and what the manifest's `blake3_hash` commits to; the plaintext hash travels separately as `plaintext_blake3_hash` ([crates/sum-types/src/storage.rs:41-48](../../crates/sum-types/src/storage.rs#L41-L48)).
- **Manifest AEAD.** The CBOR manifest is encrypted under `HKDF-SHA256(salt="", ikm=K_file, info="snip-manifest-key-v1")` with an all-zero nonce and AAD = `b"snip-manifest-v1"` ([crates/sum-crypto/src/manifest.rs:46-72](../../crates/sum-crypto/src/manifest.rs#L46-L72)). Safe because `K_file` is fresh per file and the manifest is encrypted exactly once. Archives store the opaque ciphertext blob in a `<root>.opaque` sidecar instead of the public `.cbor` ([crates/sum-store/src/manifest_index.rs](../../crates/sum-store/src/manifest_index.rs)).
- **Per-recipient key wrap.** For each `--recipient <base58_addr[:expires_at_height]>`, the client fetches the recipient's registered X25519 public key via `account_getEncryptionPublicKey` ([crates/sum-node/src/rpc_client.rs:249-269](../../crates/sum-node/src/rpc_client.rs#L249-L269)), runs ephemeral X25519 ECDH, derives a KEK via `HKDF-SHA256(info="snip-recipient-kek-v1")`, and wraps `K_file` with ChaCha20-Poly1305 using the recipient's 20-byte L1 address as AAD. The resulting 80-byte bundle (`eph_pub(32) || ct(32) || tag(16)`) is what populates `AccessEntryV2.encrypted_key_bundle` on chain ([crates/sum-crypto/src/recipient.rs:112-156](../../crates/sum-crypto/src/recipient.rs#L112-L156)). Low-order X25519 points are rejected via constant-time comparison on both wrap and unwrap.
- **Recipient setup.** Each recipient must first publish their X25519 public key on chain via `sum-node register-encryption-key` ([crates/sum-node/src/main.rs:251-258](../../crates/sum-node/src/main.rs#L251-L258)). The key is derived deterministically from the recipient's Ed25519 wallet seed via HKDF (`info="snip-x25519-encryption-key-v1"` — [crates/sum-crypto/src/recipient.rs:82-95](../../crates/sum-crypto/src/recipient.rs#L82-L95)). The owner is auto-added to the initial access list; supplying additional `--recipient` flags makes the file owner-shared at registration time. Recipients without a registered encryption key cause ingest to abort **before** any chain state is written.

**Private download** ([crates/sum-node/src/download_private.rs](../../crates/sum-node/src/download_private.rs)) layers four extra steps on top of Step 8:
1. **Access lookup.** `find_my_access_entry` paginates the file's access list (256 entries per page, max 64 pages) looking for the downloader's own L1 address.
2. **Expiry check.** `finalized_height <= expires_at` (inclusive — no grace beyond `expires_at`) ([crates/sum-node/src/download_private.rs:163-207](../../crates/sum-node/src/download_private.rs#L163-L207)).
3. **Key unwrap.** Derive own X25519 secret from the Ed25519 seed via the same HKDF used at register-time, then `unwrap_for_self(bundle)` to recover `K_file` ([crates/sum-crypto/src/recipient.rs:160-198](../../crates/sum-crypto/src/recipient.rs#L160-L198)).
4. **Decrypt + verify.** Pull the opaque manifest, decrypt under `K_file`, rebuild the Merkle tree from the manifest's `plaintext_blake3_hash` descriptors and check against the chain-recorded root. Pull each ciphertext chunk (BLAKE3-verify against the manifest's `blake3_hash`), decrypt under `K_file`, verify the plaintext against `plaintext_blake3_hash`, then verify the assembled whole against `manifest.file_hash` ([crates/sum-node/src/download_private.rs:318-361](../../crates/sum-node/src/download_private.rs#L318-L361)).

**Sharing, revoking, and updating access** are owner-only operations:

- **`sum-node share <merkle_root> --recipient <addr[:height]|:none>`** ([crates/sum-node/src/main.rs:280-294](../../crates/sum-node/src/main.rs#L280-L294)) — the owner unwraps `K_file` from their own access bundle locally, re-wraps it for the new recipient's registered X25519 key, and submits `AddAccessV2` ([crates/sum-node/src/tx_builder.rs:218-230](../../crates/sum-node/src/tx_builder.rs#L218-L230)). The chain never sees `K_file`.
- **`sum-node revoke <merkle_root> --recipient <addr>`** ([crates/sum-node/src/main.rs:296-308](../../crates/sum-node/src/main.rs#L296-L308)) — submits `RemoveAccessV2` ([crates/sum-node/src/tx_builder.rs:242-257](../../crates/sum-node/src/tx_builder.rs#L242-L257)) removing the chain-side access entry. Does **not** rotate `K_file`: a revoked recipient still holds their old bundle locally, but the chain ACL denies them on the next pull. For forward secrecy, revoke + re-ingest under a fresh key.
- **`sum-node update-access <merkle_root> --recipient <addr:height|addr:none>`** ([crates/sum-node/src/main.rs:310-322](../../crates/sum-node/src/main.rs#L310-L322)) — submits `UpdateAccessV2` ([crates/sum-node/src/tx_builder.rs:269-286](../../crates/sum-node/src/tx_builder.rs#L269-L286)) to change only the entry's `expires_at`, byte-preserving the encrypted bundle. Requires an explicit `:<height>` or `:none` directive — a bare `<addr>` is rejected so operator intent is unambiguous.

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
