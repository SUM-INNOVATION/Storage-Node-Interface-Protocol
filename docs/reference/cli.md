# CLI reference

Canonical reference for the `sum-node` and `e2e-helper` binaries.
Every subcommand and every flag is listed here; if a flag exists in
code but is missing from this file, `scripts/check-cli-doc.sh`
fails inside `make release-check`.

Global flags apply to every subcommand. Per-subcommand flags are
listed under each subcommand's section.

See [`config-flags.md`](config-flags.md) for a longer-form
description of the safety-critical global flags (in particular the
`--chain-id` note).

## sum-node

Two binaries share the workspace; `sum-node` is the primary. All
flags accept an environment-variable override (`SUM_*`) except
where noted.

### Global flags

Applies to every subcommand.

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--key-file` | `SUM_KEY_FILE` | (none) | Ed25519 seed hex. Without it: ephemeral keypair, dev mode, PoR + V2 write commands disabled. |
| `--store-dir` | `SUM_STORE_DIR` | `$HOME/.sumnode/store` | Directory holding this node's chunks and manifests. The working directory is never consulted, so two nodes started by one user in different directories share a root unless this is set. Give every node on a host its own. With no flag, no `SUM_STORE_DIR` and no `HOME`, the binary exits with an error rather than picking a fallback path. |
| `--rpc-url` | `SUM_RPC_URL` | `http://127.0.0.1:9944` | SUM Chain JSON-RPC endpoint. |
| `--por-poll-secs` | `SUM_POR_INTERVAL` | `10` | PoR challenge polling interval. |
| `--market-sync-secs` | `SUM_MARKET_SYNC_INTERVAL` | `30` | Market-sync polling interval. |
| `--gc-grace-secs` | `SUM_GC_GRACE` | `3600` | Grace period before garbage-collecting unassigned chunks. |
| `--epoch-watch-secs` | `SUM_EPOCH_WATCH_INTERVAL` | `60` | Listen mode: how often to check held V2 files for a new assignment epoch and attest held chunks again under it. `0` disables. See [assignment epochs](../operator/monitoring.md#assignment-epochs-and-re-attestation). |
| `--refetch-secs` | `SUM_REFETCH_INTERVAL` | `0` | Listen mode: how often to re-fetch Public chunks a reassignment newly assigned to this archive from other archives. `0` (default) disables. Private files are never re-fetched. See [re-fetch](../operator/monitoring.md#re-fetch-of-newly-assigned-public-chunks). |
| `--chain-id` | `SUM_CHAIN_ID` | `1337` | See [`config-flags.md`](config-flags.md) "Chain ID safety." |
| `--attest-fee` | `SUM_ATTEST_FEE` | `1000000` | Per-tx fee for V2 attestation. Must be ≥ chain `min_fee`. |
| `--client` | `SUM_CLIENT_MODE` | `false` | Client mode: upload / download only. `listen` refused. |
| `--enable-wan` | `SUM_ENABLE_WAN` | `false` | Enable Kademlia DHT + TCP transport. |
| `--bootstrap-peer` | `SUM_BOOTSTRAP_PEERS` | (empty) | Kademlia bootstrap multiaddrs. Repeatable or comma-separated. |
| `--tcp-port` | `SUM_TCP_PORT` | `0` | TCP listen port (0 = OS-assigned). |
| `--udp-port` | `SUM_UDP_PORT` | `0` | QUIC UDP port. Pin for reliable dialability. |
| `--relay-server` | `SUM_RELAY_SERVER` | `false` | Advertise as Circuit Relay v2 server. Requires `--enable-wan`. |
| `--mdns` | `SUM_MDNS` | `on` | mDNS LAN discovery, `on` or `off` (`true`/`false`, `1`/`0`, `yes`/`no` also accepted). `off` builds no mDNS: no announcement, no discovery. See [controlled networks](../architecture/networking.md#controlled-networks). |
| `--outbound-allow` | `SUM_OUTBOUND_ALLOW` | (unset: no restriction) | Outbound address allow-list; repeatable or comma-separated. Entries are IPs/CIDRs (`127.0.0.1`, `10.0.0.0/8`, `::1`) or multiaddr prefixes (`/ip4/127.0.0.1/tcp/4001`, `/ip4/127.0.0.1/udp/4001/quic-v1`, optionally ending in `/p2p/<peer-id>`). When set, every dial must match and direct inbound connections are kept only from allowed addresses; refusals are logged and counted. Malformed entries are a usage error (exit 2). An application-level routing control, not an OS security boundary. See [controlled networks](../architecture/networking.md#controlled-networks). |
| `--profile` | `SUM_PROFILE` | `production` | `production` fails closed on uncertain ACL paths; `dev` relaxes them. |

### sum-node listen

Serve chunks, enforce ACLs, respond to Proof of Retrievability
challenges, run market-sync + GC, dispatch V2 inbound when a
signing key is present.

No local flags. Refused when `--client` is set.

### sum-node ingest

Legacy V1 upload. Chunk locally, push to R = 3 assigned nodes,
announce on the mesh.

| Flag | Default | Purpose |
|---|---|---|
| `<path>` | — | (positional) Path to the file to ingest. |
| `--upload-timeout-secs` | `120` | Time to wait for R = 3 push confirmations. |
| `--manifest-push-timeout-secs` | `60` | Time to wait for manifest replication ACKs. |

**Behavior depends on `--client`.** In node mode (`--client` unset)
the command stays running after upload and serves chunks. In
client mode it cleans up the local store and exits after
confirmations.

### sum-node ingest-v2

V2 upload via chain plan v3.2: `RegisterFilePendingV2` → push
chunks → push manifest → poll coverage → `ActivateFileV2`.

| Flag | Default | Purpose |
|---|---|---|
| `<path>` | — | (positional) Path to the file to ingest. |
| `--push-wait-secs` | `120` | S2 push-wave wall-clock timeout. |
| `--manifest-push-wait-secs` | `60` | S3 manifest-push wall-clock timeout. |
| `--activation-wait-secs` | `300` | S4 coverage-poll wall-clock timeout. |
| `--visibility` | `public` | `public` or `private`. `private` generates a fresh `K_file`, encrypts chunks + manifest, wraps `K_file` for each recipient. |
| `--recipient` | — | Recipient for a Private file: `<base58 L1 addr>` or `<addr>:<expires_at_height>`. Repeatable. Rejected with `--visibility public`. |

**Outcome and exit status.** `ingest-v2` exits 0 whenever it reaches an
outcome, including a file left `Pending`; read the last `lifecycle:` line
on stdout. `lifecycle: Active` means `ActivateFileV2` was submitted and
finalized; `lifecycle: Pending` means the file is registered but still
`Pending` (run `resume` or `abandon`). An error before registration prints
no `lifecycle:` line, only a warning. A non-zero exit status means a setup
or argument error (for example an unsupported `--visibility`). Unlike
`resume` (below), `ingest-v2` does not encode the outcome in its exit
status.

### sum-node resume

V2 resume: replay only the residual portion of the pipeline for a
`Pending` file.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the pending file. |
| `<path>` | — | (positional) Path to the original file. Re-chunked to rebuild Merkle proofs. |
| `--push-wait-secs` | `120` | S2 partial push wall-clock timeout. |
| `--manifest-push-wait-secs` | `60` | S3 manifest re-push wall-clock timeout. |
| `--activation-wait-secs` | `300` | S4 coverage-poll wall-clock timeout. |

**Outcome and exit status.** `resume` exits 0 only when activation is
complete. Every other outcome exits non-zero with its own code, and the
`lifecycle:` line on stdout is printed as before:

| Exit | Outcome | stdout |
|---|---|---|
| `0` | The file is `Active`: `ActivateFileV2` was submitted **and finalized** in this run, or the chain already showed the file `Active` | `lifecycle: Active` |
| `3` | The file is still `Pending`; run `resume` again or `abandon` | `lifecycle: Pending` |
| `4` | The chain shows the file `Abandoned`; resume cannot activate it | `lifecycle: Abandoned` |
| `5` | Root mismatch: the file at `<path>` does not produce `<merkle_root>` | none (a warning is logged) |
| `1` | An error: a failure during the run, or before an outcome (arguments, RPC, chain parameters, missing `--key-file`) | none (the error is logged) |

`1` is the code every `sum-node` error already exits with, and `2` is
the argument parser's usage-error code, so outcome codes start at `3`.
Before this change `resume` exited 0 on every outcome; scripts that
treated exit 0 as "resume ran" must now check for `0` (Active) or `3`
(still Pending).

### sum-node abandon

V2 abandon: submit `AbandonFileV2` for a `Pending` file owned by
this key. Pre-checks the chain-plan v3.2 `activation_grace_blocks`
strict-`>` rule before submitting.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the pending file. |

### sum-node reassign

V2 reassign: submit an owner-triggered `ReassignChunksV2` for a file
owned by this key, only when the chain reports `reassignment_needed`
and an archive that has left the active set holds a chunk of the file
under the latest epoch (otherwise: `no departed assignee; not
submitting`). The chain's no-key builder assembles the unsigned transaction; the
command verifies it against the request, signs it with this key and
waits for finality. Details:
[`../client/upload-and-download.md`](../client/upload-and-download.md#reassignment).

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the file. |
| `--dry-run` | off | Verify the built transaction and print it; sign and send nothing. |

### sum-node register-encryption-key

Register the X25519 encryption pubkey derived from the archive's
Ed25519 seed. Required to receive Private V2 file shares.

No local flags.

### sum-node register-node

Register this account as an `ArchiveNode` on chain, with a stake
commitment. Reads `chain_id` live from RPC — safe against the
`--chain-id` default.

| Flag | Default | Purpose |
|---|---|---|
| `--stake` | `1000000000` | Stake commitment in base units. Override per chain team's published minimum. |

### sum-node share

Owner-only. Wrap the file's `K_file` for a new recipient and submit
`AddAccessV2`.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the file. |
| `--recipient` | — | `<base58 L1 addr>`, `<addr>:<expires_at_height>`, or `<addr>:none`. |

### sum-node revoke

Owner-only. Remove a recipient's chain-side access entry. Does not
rotate `K_file`.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the file. |
| `--recipient` | — | L1 address to revoke. Expiry segment (if any) is ignored. |

### sum-node update-access

Owner-only. Update a recipient's expiry on a Private V2 file. The
encrypted key bundle is preserved byte-for-byte.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the file. |
| `--recipient` | — | `<addr>:<expires_at_height>` to set, `<addr>:none` to clear. A bare `<addr>` is rejected. |

### sum-node fetch

Fetch a single chunk by CID from a LAN peer.

| Flag | Default | Purpose |
|---|---|---|
| `<cid>` | — | (positional) CIDv1 string of the chunk. |

### sum-node download

Download a complete file by merkle root. Auto-routes Public vs
Private based on the chain row's visibility.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded merkle root of the file. |
| `--output` | — | Path to write the reassembled file. |
| `--max-concurrent` | `10` | Maximum concurrent chunk fetches. |
| `--download-timeout-secs` | `300` | Overall download timeout. |

`--key-file` is required only for Private V2 files (the local seed
is needed to unwrap the on-chain `K_file` bundle). Public files
download without it.

### sum-node send

Discover a peer on the LAN, publish a test gossipsub message, then
exit. Useful for connectivity smoke checks.

| Flag | Default | Purpose |
|---|---|---|
| `<message>` | — | (positional) UTF-8 message to broadcast on `sum/test/v1`. |

## e2e-helper

Diagnostic + write-testing helper. Every write-capable subcommand
guards against non-local RPC URLs with `--allow-live-chain-write`.

Global flag: `--rpc-url` — most subcommands default to
`http://127.0.0.1:8545` (the local-mirror port). `smoke` and
`active-nodes-at-height` require it explicitly.

### e2e-helper health

Call the RPC's `health` method. Read-only.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |

### e2e-helper l1-address

Derive the base58 L1 address from a seed hex. Local computation only,
no RPC.

| Flag | Default | Purpose |
|---|---|---|
| `--seed-hex` | — | Hex-encoded Ed25519 seed. |

### e2e-helper balance

Read the account balance for an address. Read-only.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |
| `--address` | — | Base58 L1 address to query. |

### e2e-helper block-number

Read `sum_blockNumber`. Read-only.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |

### e2e-helper node-record

Read the on-chain `NodeRecord` for an address. Read-only.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |
| `--address` | — | Base58 L1 address. |

### e2e-helper active-challenges

Read the pending PoR challenges for an address. Read-only.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |
| `--address` | — | Base58 L1 address. |

### e2e-helper register-node

Submit `NodeRegistry::Register(ArchiveNode)`. Reads `chain_id` live
from RPC. Refused against a non-local RPC URL without
`--allow-live-chain-write`.

| Flag | Default | Purpose |
|---|---|---|
| `--seed-hex` | — | Hex-encoded Ed25519 seed. |
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |
| `--stake` | `1000000000` | Stake commitment. |
| `--allow-live-chain-write` | `false` | Required when `--rpc-url` is not localhost. |

### e2e-helper register-file

Submit a V1 file-registration tx. Diagnostic only. Refused against
a non-local RPC URL without `--allow-live-chain-write`.

| Flag | Default | Purpose |
|---|---|---|
| `--seed-hex` | — | Hex-encoded Ed25519 seed. |
| `--rpc-url` | `http://127.0.0.1:8545` | RPC endpoint. |
| `--merkle-root` | — | Hex-encoded 32-byte merkle root. |
| `--total-size` | — | File size in bytes. |
| `--fee-deposit` | `100000000` | Fee deposit committed on registration. |
| `--allow-live-chain-write` | `false` | Required when `--rpc-url` is not localhost. |

### e2e-helper smoke

Read-only chain-liveness check: chain parameters + block-height +
optional V2-enabled gate. Returns non-zero on failure.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | — | RPC endpoint (required). |
| `--known-address` | — | Optional address to spot-check. Overridable via `SNIP_SMOKE_KNOWN_ADDRESS`. |
| `--known-root` | — | Optional merkle root to spot-check. Overridable via `SNIP_SMOKE_KNOWN_ROOT`. |
| `--known-tx` | — | Optional tx hash to spot-check. Overridable via `SNIP_SMOKE_KNOWN_TX`. |
| `--json` | `false` | Emit JSON instead of human-readable. |
| `--require-v2` | `false` | Fail if V2 is not enabled. |

### e2e-helper active-nodes-at-height

Snapshot of active archives at a chain height, gated by a minimum
required count.

| Flag | Default | Purpose |
|---|---|---|
| `--rpc-url` | — | RPC endpoint (required). |
| `--height` | `finalized` | Chain height or the string `finalized`. |
| `--require-archives` | — | Minimum active-archive count. Exit 2 if unmet. |
| `--json` | `false` | Emit JSON instead of human-readable. |

### e2e-helper generate-e2e-keys

Local computation — generate a set of seed files at 0o600 for the
WS2b harness. Refuses to write into a non-empty directory.

| Flag | Default | Purpose |
|---|---|---|
| `--out` | — | Output directory. |
| `--balance` | `1000000000000` | Starting balance to pre-fund in the harness. |
