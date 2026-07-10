# Crate map

SNIP is a Cargo workspace with five crates. Each has a single
responsibility. Public interfaces and integration points across
crates are named below; per-file walkthroughs live inside each
crate's `src/`.

## sum-types

**Path**: [`crates/sum-types`](../../crates/sum-types)

Pure data types that cross crate boundaries. No I/O, no async. The
"lingua franca" between everything else.

Key entry points:

- `storage` — `ChunkDescriptor`, `DataManifest`, `CHUNK_SIZE`,
  `REPLICATION_FACTOR`.
- `rpc_types` — RPC-wire mirrors for chain responses:
  `ChainParamsInfo`, `StorageFileInfoV2`, `AccessEntryV2`,
  `ActiveNodeSnapshotInfo`. `Option<u64>` handling for
  `v2_enabled_from_height` (`Some(0)` vs `None`) is pinned here.
- `config` — `NetConfig`, `StoreConfig`, `RpcConfig`.
- `error` — workspace-wide `SumError`.

## sum-crypto

**Path**: [`crates/sum-crypto`](../../crates/sum-crypto)

Cryptographic primitives that don't belong in `sum-types` because
they carry secret material. Owns the `Zeroizing` wrappers, the
X25519 keypair derivation from the Ed25519 seed (HKDF over the
domain `snip-x25519-encryption-key-v1`), and the per-recipient
`K_file` wrapping used by Private V2 ingest.

Key entry points:

- `recipient::wrap_for_recipient`, `unwrap_from_bundle` —
  X25519 → AEAD wrap of `K_file` for a recipient's registered
  encryption pubkey.
- `x25519_keypair_from_ed25519_seed` — deterministic derivation.
- `decrypt_and_verify_chunk` — AEAD open + tag check on Private
  ciphertext chunks.

## sum-net

**Path**: [`crates/sum-net`](../../crates/sum-net)

libp2p wrangling — the swarm, discovery (mDNS + Kademlia DHT),
transport (QUIC + optional TCP/Noise/Yamux), Circuit Relay v2 +
DCUtR for NAT traversal, gossipsub for chunk announcements, and
the `SumNet` handle other crates use to talk to the mesh.

Key entry points:

- `SumNet` — top-level handle. Constructors decide LAN-only vs WAN
  based on `NetConfig`.
- `pull_manifest_v2`, `pull_chunk_v2`, `push_chunk_v2` — V2 wire
  helpers used by ingest and download.
- `identity` — Ed25519 seed → libp2p peer ID, L1 address, base58
  encoding.
- `codec::VersionedShardCodec` — per-stream dispatch on the
  negotiated protocol name (`/sum/storage/v1` vs `v2`).
- `swarm::SumSwarm` — the event loop, WAN bootstrap
  (`bootstrap_kademlia`), and behaviour composition.

## sum-store

**Path**: [`crates/sum-store`](../../crates/sum-store)

Everything about persisted chunks and the Merkle math over them.
Content-addressed on-disk chunk store, chunker, BLAKE3 Merkle DAG,
CBOR-serialized manifests, deterministic per-chunk assignment
(both V1 hash+probe and V2 rendezvous-hash), garbage collection,
and health reporting.

Key entry points:

- `chunker` — 1 MB chunk splitting.
- `merkle` — Merkle DAG + proof construction.
- `manifest`, `manifest_index` — CBOR (de)serialization + persistent
  `merkle_root → DataManifest` lookup.
- `assignment` — V1 hash + linear-probe algorithm.
- `assignment_v2` — V2 rendezvous-hash algorithm shared with the
  chain.
- `store::ChunkStore` — on-disk `<cid>.chunk` files. `list_all_cids`,
  `delete`, `mmap`, `write` are the surface `serve.rs` and `gc.rs`
  build on.
- `gc::GarbageCollector` — mark-and-sweep with grace period +
  L1-reachability pause.
- `serve` — inbound chunk / manifest / push handler shared by V1
  and V2 dispatchers.
- `verify` — BLAKE3, CID, Merkle-path verification.

## sum-node

**Path**: [`crates/sum-node`](../../crates/sum-node)

The daemon and CLI. Wires the other four crates together, owns the
JSON-RPC client to the SUM Chain, orchestrates ingest / download /
resume / abandon / share / revoke / update-access / register-*,
runs the background workers (`PorWorker`, `MarketSyncWorker`,
`AssignmentAttestor`), and enforces ACLs.

Key entry points:

- `main.rs` — the CLI. Every subcommand's dispatch fn is named at
  the top of the file; see [`../reference/cli.md`](../reference/cli.md).
- `rpc_client::L1RpcClient` — JSON-RPC surface. Contract tests
  pin the wire-response deserialization.
- `tx_builder` — bincode-v1 transaction serialization matching the
  chain's exact byte layout. Fixtures pin the byte shape; a
  fixture diff is a chain-compat break.
- `tx_wait` — poll `chain_getTransactionStatus` to
  `Finalized` / `Failed` / `Dropped`.
- `ingest_v2::IngestPipeline` — V2 ingest state machine
  (`s1_register_pending` → `s2_push_chunks` → `s3_push_manifest` →
  `s4_wait_coverage` → `s5_activate`), plus `resume` and `abandon`.
- `access` — `run_share`, `run_revoke`, `run_update_access` and
  the `find_my_access_entry` helper used on the read path.
- `download`, `download_private`, `download_v2_routing`,
  `download_route` — file retrieval, split by public/private + V1/V2.
- `upload::UploadOrchestrator` — V1 upload path (still used by V1
  `ingest`).
- `assignment_attestor` — the `AcceptAssignmentV2` submitter run
  from the V2 inbound dispatcher.
- `push_validator` — the V2 push admission check (chain-side row,
  chunk-index, assignment membership, Merkle proof) applied on
  every inbound push.
- `inbound_v2::V2Dispatcher` — routes V2 wire requests to the
  correct handler + spawns attestation asynchronously.
- `por_worker` — PoR responder background loop.
- `market_sync::MarketSyncWorker` — V1-legacy self-healing loop.
- `acl::AclChecker` — ACL gate applied before every serve.
- `profile::NodeProfile` — `production` (fail-closed) vs `dev`
  (fail-open).
- `bin/e2e_helper.rs` — diagnostic + WS2b test helper CLI.

## How the crates interact

```
                +-----------+
                | sum-node  |  (CLI + workers + orchestration)
                +-----+-----+
                      |
        +-------------+-------------+
        |             |             |
        v             v             v
   +---------+   +---------+   +-----------+
   | sum-net |   |sum-store|   |sum-crypto |
   +----+----+   +----+----+   +-----------+
        |             |
        v             v
              +-----------+
              | sum-types |   (shared data)
              +-----------+
```

- Every crate depends on `sum-types`; nothing depends on `sum-node`.
- `sum-node` is the composition root; unit tests inside `sum-node`
  drive `sum-net`, `sum-store`, and `sum-crypto` end-to-end without
  the CLI.
- Cross-crate wire fixtures live in `sum-node/src/tx_builder.rs`
  (chain-tx bytes) and `sum-types/src/rpc_types.rs` (RPC-response
  bytes).
