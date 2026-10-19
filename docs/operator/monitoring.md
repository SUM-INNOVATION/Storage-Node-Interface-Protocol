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

The health check is not exposed on an HTTP endpoint today. Observe
it in the archive's own logs at startup.

**Do not instantiate a second `SumStore` against a running
archive's store root.** A `SumStore` takes an exclusive lease on
its root — `flock(LOCK_EX | LOCK_NB)` on `<store_dir>/.store.lease`
— and holds it for the life of the process, so a smoke script that
opens one against a live root now fails with `store root ... is
already in use` rather than reporting health. Earlier revisions of
this page recommended exactly that; it was never safe. A second
`SumStore` on a live root shares one chunk namespace and one
`manifests/` directory with the archive, and `ChunkStore::mmap`'s
safety argument rests on the root having a single manager. The
lease refuses the procedure instead of letting it corrupt state
quietly.

To inspect a root without the archive's cooperation, read it
directly — `ls <store_dir>/*.chunk | wc -l` for the chunk count,
`du -sh <store_dir>` for disk usage. These are read-only and take
no lease. `<store_dir>/.store.lease` is the lease file itself: it
is zero bytes, carries no state, is recreated on demand, and is
invisible to the garbage collector (which only ever considers
entries ending in `.chunk`). Its presence says nothing about
whether an archive is running — only a successful `flock` does —
so do not treat it as a liveness signal, and do not delete it while
an archive is up.

To take over a root, stop the archive holding it. The lease lives
in the open file description, so the kernel releases it when the
process exits, including on a crash or `kill -9`; there is no stale
lease to clear by hand.

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
  assignment recomputed and this archive is no longer assigned the
  chunk of a funded V1 file. V2 chunks are never deleted; see
  [Garbage collection and the protected-chunk list](#garbage-collection-and-the-protected-chunk-list).
  If deletions are unexpectedly frequent, the active-node set may
  be churning; check `storage_getActiveNodesAtHeight` output vs
  what the archive expects.

## Garbage collection and the protected-chunk list

**What the collector deletes.** It deletes a chunk only if:
- it belongs to a funded V1 file listed by the chain (`storage_getFundedFiles`) whose manifest this archive holds;
- it is not assigned to this archive;
- no other locally held manifest, Private chunk mapping or protected-list entry references it.

Everything else is retained, including chunks whose ownership cannot be determined.

**The protected-chunk list.**
- `<store_dir>/manifests/gc_protected.list` names every chunk the collector must never delete.
- **Each V2 push** appends one line, synced to disk *before* the chunk is written. If that write fails, the push is refused and the chunk is not stored.
- **First start.** The first start of a version that keeps the list records every chunk already on disk, because nothing says which of them arrived by V2 push. It then writes `gc_protected.initialized`. Seeding happens only on that first start.
- **Do not edit or delete either file while the archive is running.**

**Storage growth.**
- About 128 bytes per V2 chunk, plus about 69 bytes per chunk present at the first start.
- Chunks present at the first start are kept permanently. This includes V1 chunks that would otherwise have been collected later.
- V2 chunks are not collected at all. There is no V2-aware collection yet.
- Chunks of files that left the funded set, and orphans, are also retained.

**Fail-closed states.** The collector deletes nothing, and V2 pushes are refused, while any of these holds:
- the list is missing after `gc_protected.initialized` exists;
- the list is unreadable;
- the list contains a malformed line;
- a write to the list has failed.

Each is logged at `error` level. A torn final line left by a crash in the middle of a write is not an error: it is dropped on the next start.

**Recovery.** Stop the archive, then either:
- restore `gc_protected.list` from a backup; or
- remove both `gc_protected.list` and `gc_protected.initialized`. The next start then protects every chunk on disk again. This preserves data, but makes every chunk currently on disk permanent.

**Limits.**
- This does not recover chunks that earlier versions already deleted. They must be pushed again by the file's owner or another replica, and re-attested.
- `SUM_GC_GRACE` delays deletion of collectable chunks only. A restart starts the grace clock again.

## Assignment epochs and re-attestation

A file's owner can submit `ReassignChunksV2` after an archive assigned to the file leaves the active set. The chain then adds an assignment epoch: from that height it accepts this archive's `AcceptAssignmentV2` only for chunks assigned to it over the new epoch's snapshot.

**What the archive does.**
- **Push validation and attestation use the file's latest assignment epoch**, read from `storage_getAssignmentCoverageV2`. A chunk the latest epoch no longer assigns here is refused; a chunk it newly assigns here is accepted.
- **The epoch watcher** checks every V2 file this archive holds chunks of, every `--epoch-watch-secs` (default 60). When a file's latest epoch has changed, it attests the held chunks the new epoch assigns here. At startup it also attests any file whose latest epoch shows fewer attested chunks from this archive than it holds and is assigned. It retries a failed run at most 3 times per epoch.
- It attests only chunks already on disk. An archive newly assigned chunks it does not hold attests nothing for them until they are pushed to it, or until re-fetch (below, off by default) has pulled them.

**Log lines.**
- `V2 held set rebuilt from disk` (startup, with the number of chunk indices found);
- `epoch watcher: assignment epoch changed; attesting again`;
- `epoch watcher: attestation did not finish; waiting for the next epoch change or a restart` (`warn`, after 3 failed runs for one epoch).

**The held-index records.**
- `<store_dir>/manifests/<root>.v2_held` records the chunk index of each chunk accepted by V2 push, one `<index> <cid>` line per push, synced before the push is acknowledged. If the write fails the push is refused.
- At startup the archive rebuilds its held set from these records and from Public manifests, keeping only chunks still on disk.
- **Private chunks pushed by a version without these records** cannot be indexed after a restart. They stay stored and are served, but are attested again only after the owner pushes them again.
- A lost or damaged record file means the affected chunks are not attested until they are pushed again; it never makes the archive attest a chunk it does not hold. Unusable lines are dropped (and the file rewritten) at startup with a `warn`.
- **Do not edit or delete these files while the archive is running.**

## Re-fetch of newly assigned Public chunks

**Off by default.** Set `--refetch-secs` (`SUM_REFETCH_INTERVAL`) to a number of seconds to enable it, for example `300`. `0` disables it.

After a `ReassignChunksV2`, the latest epoch can assign this archive chunks of a file that nobody will push to it: the owner's `ingest`/`resume` still route to the file's original archives. With re-fetch enabled, every interval the archive:

1. reads the chain's list of Pending and Active V2 files (`storage_getPushableFilesV2`) and, for each file with more than one assignment epoch, finds the chunks the latest epoch assigns here that the previous epoch did not and that are not held;
2. pulls the file's manifest from another archive if it has none, and accepts it only if it validates exactly as a ManifestPush would;
3. pulls each chunk from the other archives assigned it (latest epoch first, then earlier epochs), refuses bytes that do not match the manifest, and stores accepted chunks through the same validation as a push: chain registration, latest-epoch assignment to this archive, Merkle proof;
4. leaves attestation to the epoch watcher, which attests the new chunks at its next check.

**Bounds per run:** 256 files, 1,024 chunks, 3 sources per chunk (and per manifest), 4 epochs, 30 s per pull. A source is used only if it is connected and has identified itself (`peer L1 identity mapped`); otherwise the next source is tried. Nothing is ever deleted.

**Private files are never re-fetched.** Their manifests and ciphertext are served only to the file's access list, and re-fetch of Private data awaits an authorization policy. Nothing is requested, stored or attested for them, and a re-fetched chunk that turns out to belong to a Private file is refused before it is stored. The skip is logged once per file per run of the node: `Private file not re-fetched: …`.

**What it does not do.**
- No promise about how soon a newly assigned archive holds or attests its chunks, or about storage-proof challenges before it does; that protocol decision is open.
- It does not complete an original placement: chunks this archive was already assigned before the reassignment are not re-fetched.
- Owner-side push routing and download routing still use the file's first epoch.

**Log lines.**
- `V2 re-fetch started (Public files only)` / `V2 re-fetch disabled (--refetch-secs 0)` at startup;
- `re-fetch: run complete` (when a run planned anything), with files checked, chunks planned, fetched and failed, and corrupt answers refused;
- `re-fetch: bytes do not match the manifest; refused` and `re-fetch: manifest refused` (`warn`, with the source);
- `re-fetch: inconsistent assignment epochs; skipped` (`warn`).

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
