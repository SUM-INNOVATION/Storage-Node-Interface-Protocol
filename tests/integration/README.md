# Integration Tests

## Overview

Two integration test scripts that exercise the full SUM Storage Node pipeline beyond what unit tests can cover.

| Script | What it tests | L1 required? | Time |
|--------|--------------|--------------|------|
| `test_p2p.sh` | mDNS discovery, gossipsub announcements, chunk transfer between two nodes | No | ~30s |
| `test_e2e_l1.sh` | ArchiveNode registration, file registration, PoR challenge response, proof submission | Yes | ~3-5 min |

---

## test_p2p.sh — P2P Chunk Transfer

Tests the complete chunk lifecycle on localhost without any blockchain.

### What it does

1. Builds `sum-node`
2. Creates a 2.5 MB test file (3 chunks: 1MB + 1MB + 0.5MB)
3. **Node A** ingests the file, announces chunks via gossipsub, and serves them
4. **Node B** discovers Node A via mDNS, fetches the first chunk, verifies the CID, and writes to disk
5. Verifies the chunk file exists on Node B's local store

### How to run

```bash
./tests/integration/test_p2p.sh
```

### Expected output

```
========================================
 SUM Storage Node — P2P Integration Test
========================================

[1/7] Building sum-node...
  OK: .../target/debug/sum-node
[2/7] Temp directory: /tmp/tmp.XXXXXX
[3/7] Generating keys and test file...
  OK: Test file 2621440 bytes (expecting 2621440)
[4/7] Starting Node A (ingest)...
  OK: Node A ingested and serving
[5/7] Extracting CID from manifest...
  OK: CID = bafkr4i...
[6/7] Starting Node B (fetch bafkr4i...)...
  OK: Node B fetched chunk
[7/7] Verifying chunk on disk...
  OK: Chunk file exists (1048576 bytes)

========================================
 PASS: P2P Integration Test
========================================
```

### Troubleshooting

- **"Node A did not finish ingesting"** — Build error? Check `cargo build --bin sum-node` runs cleanly.
- **"Node B did not fetch chunk"** — mDNS blocked? Some VPNs/firewalls block mDNS multicast (224.0.0.251:5353). Try disabling VPN.
- **"Could not extract CID"** — Log format changed? Check the manifest JSON in Node A's stderr log.

---

## test_e2e_l1.sh — Full PoR Loop with L1

Tests the complete Proof of Retrievability pipeline against a running SUM Chain.

### Prerequisites

- Two SUM Chain validator nodes running with block production active
- L1 RPC accessible from this machine (not just localhost on the validator)
- A funded account with at least 1.2 Koppa (1,200,000,000 base units)

### How to run

```bash
# With a specific seed and RPC URL
./tests/integration/test_e2e_l1.sh \
    --rpc-url http://<validator-ip>:8545 \
    --seed-hex <64_hex_chars_of_funded_account>

# With defaults (generates random seed — you must fund the account first)
./tests/integration/test_e2e_l1.sh --rpc-url http://<validator-ip>:8545
```

### Parameters

| Flag | Default | Description |
|------|---------|-------------|
| `--rpc-url` | `http://127.0.0.1:8545` | L1 JSON-RPC endpoint |
| `--seed-hex` | (random) | 64-char hex Ed25519 seed for the test account |
| `--timeout` | `360` | Max seconds to wait for a PoR challenge |

### What it does

1. Builds `sum-node` and `e2e-helper`
2. Health-checks the L1 RPC
3. Derives the L1 address from the seed, checks balance
4. Registers as an ArchiveNode (stakes 1 Koppa)
5. Ingests a 1.5 MB test file, extracts the merkle root
6. Registers the file on the L1 (deposits 0.1 Koppa to fee pool)
7. Starts the storage node in listen mode with PoR worker
8. Waits for the L1 to issue a challenge (every 100 blocks, ~200 seconds)
9. Verifies the node submitted a proof and remains Active (not slashed)

### Expected output

```
================================================
 SUM Storage Node — E2E L1 Integration Test
================================================

[1/9] Building binaries...
[2/9] Temp directory: /tmp/tmp.XXXXXX
[3/9] Checking L1 health...
  OK: L1 reachable
[4/9] Checking account...
  OK: Account funded
[5/9] Registering as ArchiveNode...
  OK: Registered as ArchiveNode
[6/9] Ingesting test file...
  OK: Ingested 1572864 bytes, merkle_root=abcd1234...
[7/9] Registering file on L1...
  OK: File registered on L1
[8/9] Starting storage node (waiting for PoR challenge)...
  ... waiting (block 1200, elapsed 30s)
  ... waiting (block 1215, elapsed 60s)
[9/9] Verifying...
  OK: Node still Active (not slashed)

================================================
 PASS: E2E L1 Integration Test
================================================
```

### Making validators accessible from this machine

If your validators listen on `127.0.0.1:8545`, they're only reachable from their own machine. Options:

1. **Change RPC bind address** — edit the validator's `config.toml`:
   ```toml
   [rpc]
   addr = "0.0.0.0:8545"
   ```
   Then use `--rpc-url http://<validator-ip>:8545`

2. **SSH tunnel** — from this machine:
   ```bash
   ssh -L 8545:127.0.0.1:8545 <validator-host>
   ```
   Then use `--rpc-url http://127.0.0.1:8545`

### Troubleshooting

- **"L1 RPC is not reachable"** — Validator not running, wrong URL, or firewall blocking. Try `curl` manually.
- **"Account has zero balance"** — Fund the account via genesis allocation or transfer from a funded account.
- **"No PoR proof submitted within timeout"** — Challenges are issued every 100 blocks. Increase `--timeout`. Also verify the file was registered (check L1 state).
- **"ArchiveNode registration not confirmed"** — Insufficient balance for the 1 Koppa stake, or transaction fee issue.

---

## e2e-helper binary

A CLI tool for L1 interactions, used by the E2E test script.

```bash
# Check L1 health
e2e-helper health --rpc-url http://127.0.0.1:8545

# Get L1 address for a seed
e2e-helper l1-address --seed-hex <64_hex>

# Query balance
e2e-helper balance --rpc-url http://127.0.0.1:8545 --address <base58>

# Query block number
e2e-helper block-number --rpc-url http://127.0.0.1:8545

# Query node record
e2e-helper node-record --rpc-url http://127.0.0.1:8545 --address <base58>

# Register as ArchiveNode
e2e-helper register-node --seed-hex <64_hex> --rpc-url http://127.0.0.1:8545 --stake 1000000000

# Register a file
e2e-helper register-file --seed-hex <64_hex> --rpc-url http://127.0.0.1:8545 \
    --merkle-root <64_hex> --total-size <bytes> --fee-deposit 100000000
```
