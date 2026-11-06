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
| `--fee-deposit` (env `SUM_FEE_DEPOSIT`) | `0` | Koppa base units locked in the file's fee pool by `RegisterFilePendingV2`. With `0` the file has no fee pool: archives earn nothing for it and the chain never challenges it (only Active files with a positive pool are challenged). `abandon` refunds 90% of it. |
| `--exit-status` | off | Exit with a code per outcome (table below) instead of 0 for every outcome. The contract lines on stdout are as without the flag, except that code `8` also prints `merkle_root:`. No environment variable. |

**Outcome and exit status.** Without `--exit-status`, `ingest-v2` exits 0
whenever it reaches an outcome, including a file left `Pending`; read the
last `lifecycle:` line on stdout. `lifecycle: Active` means
`ActivateFileV2` was submitted and finalized; `lifecycle: Pending` means
the file is registered but still `Pending` (run `resume` or `abandon`). A
registration that did not finalize during the run prints no
`merkle_root:` or `lifecycle:` line, only a warning. That is not always
"nothing on chain": a refusal before the registration was sent left
nothing, but a registration that was sent and not confirmed (a finality
timeout, a failed status query, an error from the submission call) can
still finalize and leave the file `Pending`. Codes `6`, `7` and `8` below
tell these cases apart and say what to do. A non-zero exit status means a
setup or argument error (for example an unsupported `--visibility`).
Unlike `resume` (below), `ingest-v2` does not encode the outcome in its
exit status unless `--exit-status` is given.

**With `--exit-status`**, every outcome exits with its own code. The
codes shared with `resume` have the same values and meanings.

*Contract lines* are the stdout lines that begin with `merkle_root:` or
`lifecycle:`. Log lines are also written to stdout and are not part of
the contract; errors (code `1`) and usage errors (code `2`) go to stderr.
With the flag the contract lines are those printed without it, except
that code `8` also prints `merkle_root:`:

| Exit | Outcome | Contract lines on stdout, with the flag | Without the flag |
|---|---|---|---|
| `0` | The file is `Active`: `ActivateFileV2` was submitted **and finalized** in this run | `merkle_root: 0x<root>`, then `lifecycle: Active` | the same; exit 0 |
| `3` | The file is registered and still `Pending`; run `resume` or `abandon` | `merkle_root: 0x<root>`, then `lifecycle: Pending` | the same; exit 0 |
| `6` | Refused before chain registration: the registration was not sent, or the chain reported it failed or dropped, and the root has neither a V1 nor a V2 row. This run registered nothing; retry once the cause is fixed. **It may still have paid a fee:** a registration the chain reported `Failed` was included in a block and paid its transaction fee; one never sent, or dropped, paid nothing | none (a warning is logged) | none; exit 0 |
| `7` | The root is already registered on chain, as a V1 file (`storage_getAccessList`) or a V2 file (`storage_getFileInfoV2`), and not by this run (the chain refuses to register a root twice). The row may belong to another owner: read it before treating the file as stored. A rejected registration paid its fee, as for `6` | none (a warning is logged) | none; exit 0 |
| `8` | The registration was sent, or may have been, and is not confirmed: the submission call failed, the finality wait timed out, or a status query failed. It can still finalize and leave the file `Pending`, with its deposit locked. Look the root up before retrying | `merkle_root: 0x<root>` only; no `lifecycle:` line (a warning is logged) | none; exit 0 |
| `1` | An error before an outcome (arguments, missing `--key-file`, RPC, chain parameters, network start-up), or a root lookup that could not be done after a registration that was not sent or was rejected (see below) | none; the error is on stderr | the same for errors before an outcome; a failed lookup cannot occur (no lookup is made) |
| `2` | Usage error from the argument parser | none; usage on stderr | the same |

`4` keeps its `resume` meaning (`Abandoned`); `ingest-v2` does not produce
that outcome. `5` (root mismatch) is not used: `ingest-v2` is given no
root. `6` and `7` are told apart by read-only lookups of the root, made
only with the flag and only after a registration that was not sent or was
rejected: `storage_getFileInfoV2` (V2), then, if it finds no row,
`storage_getAccessList` (V1). A row in either is `7`; no row in both is
`6`. If a lookup fails (an RPC error or a malformed row) and the other
found no row, the code is `1`, with the error on stderr: `6` is never
reported without both lookups answering. `8` is reported without a
lookup, because a row for the root may be this registration's.

Codes `6` and `7` print no `merkle_root:` line, since this run did not
register the root; the root, when one was computed, is in the warning.
Code `8` prints `merkle_root:` because the registration may still
finalize and the caller needs the root to find it (a Private file's root
is that of a fresh ciphertext, which the caller cannot compute), and no
`lifecycle:` line, because nothing is known to be registered. A Private
file's root is that of a fresh ciphertext, so `7` does not occur for it
in practice.

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

### sum-node repush

Re-push every chunk of a registered V2 file, and its manifest, to the
archives its latest assignment epoch assigns them to. Run by the file's
owner with `--key-file`. It repairs archives that hold a file's chunks
without the leaf and Merkle path a V2 push carries (chunks pushed before
archives recorded them): an archive acknowledges the repeat push of a chunk
it holds and records the proof. `resume` is unchanged.

| Flag | Default | Purpose |
|---|---|---|
| `<merkle_root>` | — | (positional) Hex-encoded 32-byte merkle root of the registered file. |
| `<path>` | — | (positional) Path to the original file (plaintext). A Private file is re-encrypted under its recovered `K_file`. |
| `--push-wait-secs` | `120` | Wall-clock timeout for the chunk pushes' acknowledgements. |
| `--manifest-push-wait-secs` | `60` | Wall-clock timeout for the manifest pushes' acknowledgements. |

What it does:
- **Lifecycle:** it works on a `Pending` or `Active` file.
- **Identity:** the operator must be the registered owner. The file at
  `<path>` must rebuild to the registered root, chunk count, plaintext size
  and stored size; a Private file is rebuilt with the `K_file` recovered
  from the owner's own access entry. Any mismatch is refused before
  anything is pushed.
- **Chunks:** it pushes **every** chunk to **every** archive the file's
  latest assignment epoch assigns it to (the epoch archives validate a push
  against), retrying a refused push twice.
- **Manifest:** it pushes the manifest to each of those archives, in the
  form an archive accepts for the file: the Public manifest as CBOR, or the
  Private manifest encrypted under `K_file`. The manifest's file name is
  taken from `<path>`; an archive that already holds a Private manifest
  keeps the one it has.
- **Epoch:** it re-reads the assignment epoch after the pushes. If a
  reassignment moved the file meanwhile, the run is incomplete and the
  pushes the new epoch still needs are logged. If the second read fails,
  the run is an error: it cannot be confirmed.
- **Scope:** a complete run is complete for the epoch it reports, and only
  that epoch. A later reassignment needs a new `repush` and a new
  `repush-audit`.

What it does not do:
- It sends no transaction and changes no chain state.
- An acknowledgement does not show that an archive recorded the proof (an
  archive on a version without push-proof records acknowledges too). Use
  `repush-audit` on the archive.

| Exit | Outcome | stdout |
|---|---|---|
| `0` | Every chunk and manifest push was acknowledged and the epoch read again afterwards is the same | `merkle_root:`, `repush_epoch: <n>`, one `repush_archive: <address> chunks_acknowledged=<n> of=<m> manifest=<result>` line per archive, `repush: complete for epoch <n>` |
| `3` | A push was refused, unanswered, or not sent (no known peer for the archive), or the epoch changed during the run (`repush_epoch_changed: <from> -> <to> not_pushed=<n>`). Each is logged; run it again. | as for `0`, ending `repush: incomplete (epoch <n>)` |
| `4` | The chain shows the file `Abandoned`; nothing was pushed | none |
| `5` | Root mismatch; nothing was pushed | none (a warning is logged) |
| `6` | The operator is not the registered owner, or the rebuilt file's chunk count or sizes differ from the registration; nothing was pushed | none (a warning is logged) |
| `1` | An error (arguments, RPC, rebuilding the file, missing `--key-file`), or the epoch could not be read again after the pushes | none; or, for the final read, as for `0` with `repush_epoch_unconfirmed: <error>` and `repush: error (epoch <n> could not be re-read after the pushes)` |

### sum-node repush-audit

Read-only, on an archive: check that every chunk the chain assigns this
archive (`--key-file`) for each file of an explicit inventory can be
answered if challenged. See `docs/operator/repush-repair.md`.

| Flag | Default | Purpose |
|---|---|---|
| `--root` | — | A merkle root to audit (hex, `0x` optional). Repeatable. |
| `--roots-file` | — | A file listing merkle roots, one per line; `#` comments and blank lines are ignored. |

For each file it reads the file's latest assignment epoch and that epoch's
snapshot, takes the chunks assigned to this archive, and for each builds
the answer the PoR responder would submit, from the local store only (the
file's Public manifest if held here, otherwise the chunk's recorded push
proof), then checks it with the chain's verifier. The answer is built from
the stored chunk's bytes, so a missing or altered chunk is not usable.

- **Inventory:** the roots given, never the local index. With none the
  verdict is `no_inventory`: an empty or partial index does not show that
  no repair is needed.
- **Read-only:** it reads `--store-dir` without writing and without taking
  the store's lease, so it can run beside the archive, and sends nothing to
  the chain. A push that lands while it runs may not be seen.
- **Epochs:** each file's latest assignment epoch is read before the file
  is evaluated and again once every file is. An epoch that moved in
  between makes the verdict incomplete; a second read that fails makes it
  an error. A complete verdict is complete for the epochs it reports, and
  only those: a later reassignment needs a new `repush` and a new audit.

| Exit | Outcome | stdout |
|---|---|---|
| `0` | Every assigned chunk of every inventory file is usable, and every file's epoch is unchanged | `repush_audit_archive:`, `repush_audit_inventory: <n>`, one `repush_audit_file: <root> epoch=<e> epoch_recheck=unchanged assigned=<n> usable_public_manifest=<a> usable_pushed_proof=<b> not_usable=<m>` line per file (`abandoned` for an Abandoned file), `repush_audit: complete for epochs {<e>, ...}` |
| `3` | At least one assigned chunk is not usable, or a file's epoch moved during the audit (`epoch_recheck=moved_to=<n>`) | as for `0`, with one `repush_audit_not_usable: <root> chunk=<i> reason=<...>` line per chunk, ending `repush_audit: incomplete` |
| `7` | No inventory was given | `repush_audit: no_inventory` |
| `1` | A chain read failed (including a file's final epoch read, `epoch_recheck=failed (...)`), or an inventory root is not registered (`repush_audit_file: <root> not_registered` / `error=...`) | as for `0`, ending `repush_audit: error` |

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
