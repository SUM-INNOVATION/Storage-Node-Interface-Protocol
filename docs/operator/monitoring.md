# Monitoring and operational health

What to watch on a running archive, what the observable failure
modes look like, and where to look when they surface.

The setup + first-run material lives in [`runbook.md`](runbook.md).
This document assumes the archive is running and focuses on the
post-install steady state.

## Metrics

`sum-node listen` maintains an in-process metrics surface
(`NodeMetrics`) with atomic counters. There is no built-in
Prometheus endpoint in `v0.4.x` — the metrics can be observed via
tracing at startup and inspected in-process. Key counters:

| Counter | Meaning |
|---|---|
| `chunks_served` | Successful outbound `ShardResponse` deliveries. |
| `por_proofs_submitted` | `SubmitStorageProof` transactions the archive submitted. |
| `por_proofs_failed` | Proofs the chain rejected or the archive could not build. |
| `gc_chunks_deleted` | Chunks the local `GarbageCollector` removed. |
| `peers_connected` | Currently-connected libp2p peers. |

Structured metrics scraping is a candidate for a future release;
see [`../roadmap/roadmap.md`](../roadmap/roadmap.md).

## Health check

`SumStore::health_check` returns:

- `chunk_count` — chunks on disk.
- `manifest_count` — manifests indexed by `manifest_index`.
- `disk_usage_bytes` — total on-disk bytes across `<cid>.chunk`
  files.
- `store_dir_writable` — true if a probe write succeeded.

The health check is not exposed on an HTTP endpoint today. Inspect
it via a smoke script that instantiates a `SumStore` against the
running archive's chunk directory, or observe it in the logs at
startup.

## Observable failure modes

The runbook's operational-troubleshooting table covers the full
matrix; the highlights:

- **`Failed(N)` on first mainnet write.** Common causes: wrong
  `--chain-id`, insufficient balance, RPC drift. See
  [`../reference/config-flags.md`](../reference/config-flags.md)
  "Chain ID safety" — following the mainnet examples in
  [`mainnet-bringup.md`](mainnet-bringup.md) exactly is the
  prescriptive path.
- **Ingest lands on chain but stays `Pending` forever.** The
  three-archive quorum has not been reached — see
  [`mainnet-bringup.md`](mainnet-bringup.md) §5. If quorum is
  reached but coverage still fails, an archive on the assigned set
  is not currently `listen`-ing, or its `AssignmentAttestor` is
  submitting transactions the chain rejects (the CLI's `--chain-id`
  default is `1337` and is used by the attestor — pass
  `--chain-id 1` explicitly on mainnet).
- **Sudden slash of an archive's stake.** A PoR challenge landed
  during a period the archive was offline or unable to fetch the
  challenged chunk within `CHALLENGE_TTL_BLOCKS`. Check the
  archive's `--por-poll-secs` (default 10 s) and confirm the chain
  RPC is reachable. See
  [`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md)
  for the eligibility rules governing which chunks the archive can
  be asked to prove.
- **`gc: deleting unassigned chunk` in the logs.** Normal — the
  assignment recomputed and this archive no longer holds the chunk.
  If deletions are unexpectedly frequent, the active-node set may
  be churning; check `storage_getActiveNodesAtHeight` output vs
  what the archive expects.

## Log guards

Sensitive material (Ed25519 seed, X25519 secret, `K_file`, wrapped
key bundles, chunk plaintext) is not permitted in `info!` /
`warn!` / `debug!` / `println!` / `eprintln!`. The guard is
[`scripts/audit-logs.sh`](../../scripts/audit-logs.sh) which runs
as part of `make release-check`. Rows in
[`../security/privacy-audit.md`](../security/privacy-audit.md)
list each pinned guardrail and the token it defends.

If an operator adds a log line that trips the audit, they can add
an inline `// audit-allow: <reason>` marker; each allow must be
reviewed at release time.

## Cross-references

- Runbook (setup, first-run, mainnet bring-up): [`runbook.md`](runbook.md).
- Mainnet bring-up specifics: [`mainnet-bringup.md`](mainnet-bringup.md).
- Config surface: [`../reference/config-flags.md`](../reference/config-flags.md).
- Privacy pinning guardrails: [`../security/privacy-audit.md`](../security/privacy-audit.md).
