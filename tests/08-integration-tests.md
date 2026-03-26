# Integration Tests — Documentation

**Location:** `tests/integration/`
**Scripts:** 2 (bash)
**Supporting binary:** `e2e-helper` (Rust)

---

## Overview

Unit tests (63 total in Phases 1-2) validate each component in isolation. Integration tests validate the full pipeline end-to-end:

| What unit tests cover | What integration tests cover |
|----|---|
| Chunker produces correct offsets/sizes | Two nodes actually transfer chunks over QUIC |
| Codec serializes/deserializes correctly | A real ShardRequest flows through mDNS discovery + gossipsub |
| MerkleTree proofs pass L1 verification algorithm | PoR worker reads a real chunk, builds a real proof, signs a real transaction |
| Base58 encoding round-trips | ACL interceptor queries a real L1 node |

---

## Test 1: `test_p2p.sh` — P2P Chunk Transfer

**Source:** `tests/integration/test_p2p.sh`
**L1 required:** No
**Typical duration:** ~30 seconds

### What it exercises

| Component | How |
|-----------|-----|
| `BinaryChunker::chunk_file()` | Ingests a 2.5 MB file into 3 chunks |
| `ChunkStore::put()` | Writes chunks to Node A's disk |
| `ManifestIndex::insert()` | Persists manifest with merkle root |
| `ChunkAnnouncement` gossipsub | Node A announces via `sum/storage/v1` topic |
| mDNS discovery | Node B discovers Node A on localhost |
| `/sum/storage/v1` request-response | Node B requests chunk from Node A |
| `ShardCodec` wire protocol | `[u32 BE length][bincode payload]` over QUIC |
| `serve::handle_request()` | Node A mmaps chunk, slices, responds |
| `FetchManager::on_chunk_received()` | Node B receives, verifies CID, stores |
| `verify::verify_cid()` | CID integrity check on received data |

### How to run

```bash
./tests/integration/test_p2p.sh
```

### Success criteria
- Node A ingests file and enters serve mode
- Node B discovers Node A via mDNS
- Node B fetches chunk and writes `<cid>.chunk` to disk
- Chunk file exists and has correct size (1,048,576 bytes for first chunk)
- Script exits with code 0

---

## Test 2: `test_e2e_l1.sh` — Full PoR Loop

**Source:** `tests/integration/test_e2e_l1.sh`
**L1 required:** Yes (running validators)
**Typical duration:** 3-5 minutes

### What it exercises

| Component | How |
|-----------|-----|
| `L1RpcClient` | Health check, balance query, nonce fetch, chain_id fetch |
| `tx_builder::build_register_archive_node_tx()` | Constructs bincode v1 NodeRegistry transaction |
| `tx_builder::build_register_file_tx()` | Constructs bincode v1 StorageMetadata::RegisterFile transaction |
| `tx_builder::build_submit_proof_tx()` | Constructs bincode v1 SubmitStorageProof transaction |
| `L1RpcClient::send_raw_transaction()` | Submits hex-encoded signed transactions to L1 mempool |
| `PorWorker::poll_and_respond()` | Background loop detects L1 challenge |
| `MerkleTree::build()` + `generate_proof()` | Rebuilds tree from manifest hashes, generates sibling path |
| `ChunkStore::get()` | Reads challenged chunk from disk for hashing |
| Ed25519 signing | Signs transaction with `blake3(bincode1_serialize(tx))` |
| L1 verification | L1 verifies Merkle proof, rewards node with 10 Koppa |

### How to run

```bash
./tests/integration/test_e2e_l1.sh \
    --rpc-url http://<validator-ip>:8545 \
    --seed-hex <64_hex_chars_of_funded_account>
```

### Success criteria
- L1 RPC is reachable
- Account is funded and registers as ArchiveNode
- File is ingested and registered on L1
- PoR worker detects a challenge and submits a proof
- Node remains Active (not slashed) after proof submission
- Script exits with code 0

---

## Supporting Binary: `e2e-helper`

**Source:** `crates/sum-node/src/bin/e2e_helper.rs`
**Purpose:** CLI tool for L1 interactions needed by `test_e2e_l1.sh`

Reuses the same `L1RpcClient` and `tx_builder` as the main `sum-node` binary, ensuring the test exercises the exact same code paths.

### Commands

| Command | Purpose |
|---------|---------|
| `health` | Verify L1 RPC is reachable |
| `l1-address` | Derive L1 base58 address from Ed25519 seed |
| `balance` | Query account balance |
| `block-number` | Query current block height |
| `node-record` | Query node registry (check ArchiveNode status) |
| `active-challenges` | Query active PoR challenges for a node |
| `register-node` | Submit NodeRegistry::Register(ArchiveNode) transaction |
| `register-file` | Submit StorageMetadata::RegisterFile transaction |

---

## tx_builder Extensions (Phase 2 → Integration)

Two new builder functions were added to support the E2E test:

### `build_register_archive_node_tx()`

Constructs a `TxPayload::NodeRegistry` transaction (variant index 17) with:
- `NodeRegistryOperation::Register` (variant index 0)
- `NodeRole::ArchiveNode` (variant index 1)
- Specified stake amount (minimum 1,000,000,000 = 1 Koppa)

**Test:** `tx_builder::tests::build_and_verify_register_node_tx` — builds tx, round-trips through bincode v1, verifies all fields.

### `build_register_file_tx()`

Constructs a `TxPayload::StorageMetadata` transaction (variant index 18) with:
- `StorageMetadataOperation::RegisterFile` (variant index 0)
- Specified merkle_root, total_size_bytes, access_list, fee_deposit

**Test:** `tx_builder::tests::build_and_verify_register_file_tx` — builds tx, round-trips through bincode v1, verifies all fields.

---

## Test Count Update

| Phase | Unit Tests | Integration Tests | Total |
|-------|-----------|-------------------|-------|
| Phase 1 | 47 | 0 | 47 |
| Phase 2 | 16 | 0 | 63 |
| Integration | 2 | 2 scripts | 65 + 2 scripts |
| **Current** | **65** | **2 scripts** | **65 unit + 2 integration** |
