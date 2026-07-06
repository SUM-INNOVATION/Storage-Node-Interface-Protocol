# Monitoring

What to watch on a running archive node, and what "healthy" looks like. Pairs
with [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md) (run and recover) and
[`MAINNET-BRINGUP.md`](MAINNET-BRINGUP.md) (first bring-up).

## Current state: monitoring is log-based

There is no metrics HTTP endpoint today. The node does **not** expose a
Prometheus `/metrics` route, and there is no built-in health-check port. A
`NodeMetrics` counter struct exists in the source
([`crates/sum-node/src/metrics.rs`](../../crates/sum-node/src/metrics.rs)) but is
skeleton code: it is not yet instantiated or threaded through the workers, so it
does not collect anything at runtime. Do not build alerting against a
`/metrics` scrape; it is not there yet.

Until that lands, operational monitoring is done by watching the node's
structured logs (`tracing` at `RUST_LOG=info`) and by querying the chain for the
node's own state. Both are described below.

## What to watch in the logs

Run the node with `RUST_LOG=info`. The two background workers emit the signals
that matter.

### PorWorker (the earning heartbeat)

| Log line | Meaning | What it tells you |
|----------|---------|-------------------|
| `PoR worker started` | Boot | Worker is up; logs your address and poll interval |
| `active challenges found` | A challenge targets you | The chain is exercising you; count is logged |
| `PoR proof submitted` | You answered a challenge | The heartbeat: node holds data and is earning. Logs challenge_id, chunk_index, result |
| `PoR poll failed — backing off` | RPC trouble | Occasional is fine; sustained means the node cannot reach the chain |

The single most important signal is `PoR proof submitted` with a success result.
A node that stops emitting it while challenges are being issued is failing to
prove retrievability and is on a path to being slashed.

### MarketSyncWorker (V1-legacy self-heal)

| Log line | Meaning |
|----------|---------|
| `MarketSync worker started` | Boot; logs address and poll interval |
| `fetching assigned chunk via FetchManager` | Pulling a chunk it should hold; logs root, chunk_index, cid, peer |
| `GC completed after sync cycle` | Garbage collection ran; logs chunks_deleted, bytes_freed |
| `failed to sync file` | A file could not be synced; logs merkle_root and error |
| `MarketSync cycle failed — backing off` | RPC trouble on the sync loop |

## What to watch on chain

The authoritative view of a node's standing is its on-chain record. Query it
with the RPC methods in [`RPC-API.md`](../reference/RPC-API.md):

- **`storage_getNodeRecord(<addr>)`**: your `status` must be `Active`. If it
  reads `Slashed`, you missed a challenge and were penalized and ejected. Your
  `staked_balance` shows the stake remaining after any slashing.
- **`storage_getActiveChallenges(<addr>)`**: the open challenges targeting you
  right now, each with an `expires_at_height`. If one is approaching its
  deadline and you have not proven it, that is an imminent slash.
- **`chain_getBlockHeight(["finalized"])`**: confirm the chain is advancing and
  your view of it is current.

## Alerting priorities

In order of severity, the conditions worth paging on:

1. **`status` flipped to `Slashed`** on chain. Terminal for this registration;
   you are out of service. Highest priority.
2. **An open challenge nearing `expires_at_height` with no proof submitted.**
   This is a slash about to happen. The window is the challenge TTL (50 blocks).
3. **Sustained `PoR poll failed` / `MarketSync cycle failed`.** The node cannot
   reach the chain, so it cannot see or answer challenges. Leads to (2) then (1).
4. **The process is not running** (no logs, supervisor restart loop).

## A note on secrets in logs

The node is built so that key material never reaches a log line, and this is
enforced by the `audit-logs` guardrail in the release gate
([`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md)). When you ship logs to a
central collector, the seed, `K_file`, X25519 secret, and key bundles are not in
them by construction. What is in them: your peer ID, your L1 address, merkle
roots, chunk indices, and counts. Treat merkle roots and file metadata as you
would any operational data.

## See also

- [`OPERATOR-RUNBOOK.md`](OPERATOR-RUNBOOK.md): run, recover, and the exact queries
- [`RPC-API.md`](../reference/RPC-API.md): the chain methods to poll
- [`PRIVACY-AUDIT.md`](../reference/PRIVACY-AUDIT.md): what the logs deliberately never contain
