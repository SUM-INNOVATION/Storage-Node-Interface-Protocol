# CLI reference

Complete reference for the `sum-node` binary: every global flag, every
subcommand, and the arguments each one takes. This is the source-of-truth
companion to the operator-facing [`OPERATOR-RUNBOOK.md`](../operations/OPERATOR-RUNBOOK.md)
(how to run a node day-to-day) and the client-facing walkthrough in
[`CMPLT-PROC.md`](CMPLT-PROC.md) (what each command does to chain and mesh
state). Flag defaults here are pinned to the clap definitions in
[`crates/sum-node/src/main.rs`](../../crates/sum-node/src/main.rs); if the two
ever disagree, the code wins and this doc is the bug.

## Invocation

```
sum-node [GLOBAL FLAGS] <COMMAND> [COMMAND ARGS]
```

Global flags precede the subcommand. Every global flag also reads from an
environment variable, so a long-running node is usually configured through the
environment (or a systemd unit) and invoked with just the subcommand:

```bash
export SUM_KEY_FILE=/etc/sum/node.hex
export SUM_RPC_URL=https://rpc.sumchain.io
export SUM_PROFILE=production
sum-node listen
```

When running from source, insert `cargo run --bin sum-node --` in place of the
binary name: `cargo run --bin sum-node -- listen`.

## Global flags

| Flag | Env var | Default | Meaning |
|------|---------|---------|---------|
| `--key-file <PATH>` | `SUM_KEY_FILE` | *(none)* | Ed25519 seed file: 32 bytes, hex-encoded. This one key is both the L1 wallet and the P2P identity. Omitting it generates a throwaway random keypair (dev mode, PoR disabled). |
| `--rpc-url <URL>` | `SUM_RPC_URL` | `http://127.0.0.1:9944` | SUM Chain L1 JSON-RPC endpoint. |
| `--por-poll-secs <N>` | `SUM_POR_INTERVAL` | `10` | How often the PoR worker polls the chain for challenges targeting this node. |
| `--market-sync-secs <N>` | `SUM_MARKET_SYNC_INTERVAL` | `30` | How often the V1-legacy MarketSync worker polls for funded files. |
| `--gc-grace-secs <N>` | `SUM_GC_GRACE` | `3600` | How long an unassigned chunk is retained before garbage collection deletes it. |
| `--chain-id <N>` | `SUM_CHAIN_ID` | `1337` | Chain ID stamped into V2 transactions. Mainnet is `1`; the local mirror is `1337`. Signing against the wrong ID burns the fee (see [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md)). |
| `--attest-fee <N>` | `SUM_ATTEST_FEE` | `1000000` | Per-transaction fee for V2 assignment attestation (`AcceptAssignmentV2`). |
| `--client` | `SUM_CLIENT_MODE` | off | Client mode: upload and download only. No PorWorker, no MarketSync, no GC, no chunk serving. `listen` is unavailable. |
| `--enable-wan` | `SUM_ENABLE_WAN` | off | Enable WAN discovery (Kademlia DHT + TCP). When off, only mDNS LAN discovery runs. |
| `--bootstrap-peer <MULTIADDR>` | `SUM_BOOTSTRAP_PEERS` | *(none)* | Kademlia bootstrap peer. Repeatable, or comma-separated in the env var. Example: `/ip4/1.2.3.4/tcp/4001/p2p/12D3KooW...` |
| `--tcp-port <PORT>` | `SUM_TCP_PORT` | `0` | TCP listen port for WAN. `0` lets the OS assign one. |
| `--udp-port <PORT>` | `SUM_UDP_PORT` | `0` | UDP listen port for the QUIC transport. Pin a stable value if the node must be reliably dialable over QUIC (UPnP forward, fixed DCUtR hole-punch target); `0` picks a fresh ephemeral port every restart. |
| `--relay-server` | `SUM_RELAY_SERVER` | off | Volunteer this node as a Circuit Relay v2 server. Only meaningful on a publicly-reachable host, and only with `--enable-wan`. |
| `--profile <PROFILE>` | `SUM_PROFILE` | `production` | `production` fails closed on every uncertain ACL path (RPC errors, unregistered files, unknown CIDs all deny). `dev` relaxes those for local testing without an L1. Never use `dev` in a real deployment. |

## Client mode vs node mode

The `--client` flag is the fork between the two audiences the binary serves:

- **Node mode** (default) is for archive operators. It runs the background
  workers (PorWorker, MarketSync, GC) and serves chunks to the mesh. The
  `listen` command lives here; passing `--client listen` is rejected.
- **Client mode** (`--client`) is for file users (Alice and Bob in
  [`CMPLT-PROC.md`](CMPLT-PROC.md)). It runs one publish or retrieve operation
  and exits, with none of the long-running node services.

## Commands

The commands below are grouped by task. Names are shown as typed on the command
line (clap lowercases and hyphenates the internal variant, so `IngestV2` is
invoked as `ingest-v2`).

### Node operation

#### `listen`

Run indefinitely: serve chunk requests, enforce ACLs, and answer PoR
challenges. Node mode only. This is the command an archive operator runs under
systemd. Takes no arguments; behavior is entirely governed by the global flags
and the node's on-chain assignments.

```bash
sum-node --key-file node.hex --rpc-url https://rpc.sumchain.io --enable-wan listen
```

#### `register-node`

Register this account as an `ArchiveNode` in the on-chain `NodeRegistry`, then
wait for finality and print the transaction hash and the block it landed in.
The chain ID is read live from RPC so the transaction cannot be mis-flagged
against the wrong network. Run this once before the first `listen`.

| Argument | Default | Meaning |
|----------|---------|---------|
| `--stake <N>` | `1000000000` | Stake committed with registration, in base units (1 Koppa). Override only if your genesis pins a different minimum. |

```bash
sum-node --key-file node.hex register-node --stake 1000000000
```

#### `register-encryption-key`

Register an X25519 encryption public key on chain so other accounts can wrap
private-file keys (`K_file`) for this account. The X25519 keypair is derived
deterministically from the Ed25519 seed via HKDF (domain
`snip-x25519-encryption-key-v1`). Idempotent: re-running overwrites the slot.
A recipient must have run this before anyone can `share` a private file with
them. Takes no arguments.

```bash
sum-node --key-file node.hex register-encryption-key
```

### Publishing files

#### `ingest-v2`

Publish a file through the chain-plan-v3.2 lifecycle: `RegisterFilePendingV2` →
push chunks to the R=3 assigned archives → push the manifest → poll coverage →
`ActivateFileV2`. On success the file is `Active` on chain. On a post-register
failure the file is left `Pending`, and you can `resume` to retry or `abandon`
to release the deposit. Requires `--key-file`.

| Argument | Default | Meaning |
|----------|---------|---------|
| `<PATH>` | *(required)* | File to ingest. |
| `--push-wait-secs <N>` | `120` | Timeout for the chunk-push wave. |
| `--manifest-push-wait-secs <N>` | `60` | Timeout for the manifest-push wave. |
| `--activation-wait-secs <N>` | `300` | Timeout for coverage polling before activation. Not the chain-side `activation_grace_blocks`. |
| `--visibility <public\|private>` | `public` | `public` leaves chunks and manifest in the clear. `private` generates a fresh `K_file`, encrypts every chunk and the manifest, and wraps `K_file` for each recipient (owner auto-added). |
| `--recipient <SPEC>` | *(none)* | Private only. `<base58 L1 addr>` or `<addr>:<expires_at_height>`. Repeatable. Each recipient's X25519 pubkey is fetched from chain; a recipient with no registered key aborts ingest before any chain state is created. Passing `--recipient` on a public ingest is rejected. |

```bash
# Public file
sum-node --client --key-file alice.hex ingest-v2 ./report.pdf

# Private file shared with one recipient until block 6_000_000
sum-node --client --key-file alice.hex ingest-v2 ./report.pdf \
    --visibility private \
    --recipient 4PanYCk...:6000000
```

#### `ingest` (V1, legacy)

The pre-v3.2 ingest path: chunk, push to R=3 assigned nodes, and announce on
the mesh. Retained for V1-registered files; new work should use `ingest-v2`.

| Argument | Default | Meaning |
|----------|---------|---------|
| `<PATH>` | *(required)* | File to ingest. |
| `--upload-timeout-secs <N>` | `120` | Timeout waiting for R=3 push confirmations. |
| `--manifest-push-timeout-secs <N>` | `60` | Timeout waiting for every chunk recipient to ACK the manifest push (needed so they can resolve `cid → merkle_root` for ACLs). |

#### `resume`

Re-run the post-register portion of the V2 lifecycle against a `Pending` file.
You must pass both the merkle root recorded from the prior `ingest-v2` outcome
and the original file path (it is re-chunked locally to rebuild Merkle proofs
for any missing pushes). A path that does not match the root surfaces as a
typed `RootMismatch`.

| Argument | Default | Meaning |
|----------|---------|---------|
| `<MERKLE_ROOT>` | *(required)* | Hex-encoded 32-byte root of the pending file. |
| `<PATH>` | *(required)* | The original file. |
| `--push-wait-secs <N>` | `120` | Timeout for the partial push wave. |
| `--manifest-push-wait-secs <N>` | `60` | Timeout for the manifest re-push. |
| `--activation-wait-secs <N>` | `300` | Timeout for the coverage poll. |

```bash
sum-node --client --key-file alice.hex resume 34a749...1b66 ./report.pdf
```

#### `abandon`

Submit `AbandonFileV2` for a `Pending` file you own, releasing the deposit. The
command pre-checks chain state to give a clean "wait until height N" message
before burning a fee against the chain's strict-`>` grace rule.

| Argument | Meaning |
|----------|---------|
| `<MERKLE_ROOT>` | Hex-encoded 32-byte root of the pending file. |

### Private-file access control

All three are owner-only and operate on a `private` V2 file. None of them ever
put `K_file` on chain.

#### `share`

Grant a new recipient access. Recovers `K_file` locally from the owner's own
access bundle, wraps it for the recipient's registered X25519 key, and submits
`AddAccessV2`.

| Argument | Meaning |
|----------|---------|
| `<MERKLE_ROOT>` | Hex-encoded 32-byte root of the file. |
| `--recipient <SPEC>` | `<base58 L1 addr>`, `<addr>:<expires_at_height>`, or `<addr>:none`. The recipient's X25519 pubkey is fetched from chain; a missing key aborts before any transaction is submitted. |

```bash
sum-node --client --key-file alice.hex share 34a749...1b66 --recipient 2Ehi3aB...:6500000
```

#### `revoke`

Remove a recipient's access entry. Denies them on their next pull. Does **not**
rotate `K_file`: a revoked recipient still holds their old bundle locally, so
for forward secrecy you must revoke and re-ingest under a fresh key (see
[`PRIVACY-AUDIT.md`](PRIVACY-AUDIT.md), threat 14).

| Argument | Meaning |
|----------|---------|
| `<MERKLE_ROOT>` | Hex-encoded 32-byte root of the file. |
| `--recipient <ADDR>` | Address to revoke. Any expiry segment is ignored. |

#### `update-access`

Change a recipient's expiry without touching their encrypted key bundle (it is
preserved byte-for-byte). Requires an explicit directive: `<addr>:<height>` to
set an expiry, `<addr>:none` to clear it. A bare `<addr>` is rejected so intent
is never ambiguous.

| Argument | Meaning |
|----------|---------|
| `<MERKLE_ROOT>` | Hex-encoded 32-byte root of the file. |
| `--recipient <ADDR:DIRECTIVE>` | `<addr>:<height>` or `<addr>:none`. |

### Retrieval

#### `download`

Download and reassemble a complete file by merkle root. Routes automatically
between the V1-legacy, V2-public, and V2-private pipelines based on the file's
chain row, verifies every chunk against the manifest, and rebuilds the Merkle
tree to confirm the reconstructed file matches the chain's root.

| Argument | Default | Meaning |
|----------|---------|---------|
| `<MERKLE_ROOT>` | *(required)* | Hex-encoded root of the file. |
| `--output <PATH>` | *(required)* | Where to write the reassembled file. |
| `--max-concurrent <N>` | `10` | Maximum concurrent chunk fetches. |
| `--download-timeout-secs <N>` | `300` | Overall download timeout. |

```bash
sum-node --client --key-file bob.hex download 34a749...1b66 --output ./report.pdf
```

For a private file, the downloader additionally recovers `K_file` from the
caller's own access bundle and decrypts each chunk; the caller must be a current,
unexpired entry in the file's access list.

#### `fetch`

Fetch a single chunk by CID from a LAN peer. A low-level diagnostic, not a
file-retrieval command (use `download` for that).

| Argument | Meaning |
|----------|---------|
| `<CID>` | CIDv1 string of the chunk. |

### Diagnostics

#### `send`

Discover a peer on the LAN, publish a UTF-8 test message on the `sum/test/v1`
Gossipsub topic, then exit. Used to confirm mesh connectivity during
bring-up.

| Argument | Meaning |
|----------|---------|
| `<MESSAGE>` | UTF-8 message to broadcast. |

## See also

- [`OPERATOR-RUNBOOK.md`](../operations/OPERATOR-RUNBOOK.md): running and recovering a node
- [`RPC-API.md`](RPC-API.md): the L1 methods these commands call
- [`CMPLT-PROC.md`](CMPLT-PROC.md): end-to-end walkthrough of what each command does
- [`CHAIN-COMPAT.md`](CHAIN-COMPAT.md): chain IDs, wire format, mainnet pin
