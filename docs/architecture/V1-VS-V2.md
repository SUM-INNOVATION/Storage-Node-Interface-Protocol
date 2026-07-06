# V1 vs V2

SNIP has two protocol generations in the tree at once. V2 (chain plan v3.2) is
the current path for all new files; V1 is the legacy path, retained so that
files registered under it keep working. This doc explains what differs, why both
exist, and where the boundary sits, so you know which code path a given file
takes.

## Why both exist

The typed-transaction design versions by addition, not replacement: new
behaviour ships as new payload variants and new wire protocols alongside the old
ones, so historical files and transactions never break. V2 introduced a
different file lifecycle, a different assignment algorithm, an explicit wire
protocol, and private-file encryption. Rather than migrate V1 files in place,
the node runs both and routes per file based on the file's chain row.

## The differences

| Dimension | V1 | V2 |
|-----------|----|----|
| Registration | single-step file registration | staged lifecycle: `RegisterFilePendingV2` → push → attest → `ActivateFileV2` |
| Lifecycle states | funded / active by fee pool | explicit `Pending` / `Active` / `Abandoned` on chain |
| Assignment | hash + linear probe (`assignment.rs`) | rendezvous hash (`assignment_v2.rs`) |
| Wire protocol | `/sum/storage/v1`, push via optional `push_data` | `/sum/storage/v2`, explicit `Push` / `Pull` / `ManifestPush` / `ManifestPull` |
| Push validation | announce-and-fetch | inline Merkle proof validated before write |
| Retention | MarketSync re-fetch loop | chain-side PoR challenges + slashing |
| Privacy | public only | public or private (per-recipient encrypted) |
| Coverage / activation | implicit | `storage_getAssignmentCoverageV2`, owner activates when covered |

## Lifecycle

**V1** registers a file and relies on the `MarketSyncWorker` to keep chunks
placed: nodes poll `storage_getFundedFiles` and `storage_getActiveNodes`, compute
the V1 assignment, and self-heal by fetching chunks they should hold.

**V2** is explicit and staged. The uploader registers the file as `Pending`
(`RegisterFilePendingV2`), pushes each chunk to its assigned archives with an
inline Merkle proof, and pushes the manifest. Each archive attests coverage on
chain (`AcceptAssignmentV2`), OR-merging its assigned chunk indices into a
per-`(file, archive)` bitmap. The uploader polls
`storage_getAssignmentCoverageV2` until `can_activate_now` is true, then submits
`ActivateFileV2` to move the file `Pending → Active`. From there, retention is
enforced by PoR challenges and slashing, not by a re-fetch loop. If activation
never completes, the file can be resumed or abandoned (releasing the deposit
after the grace period).

## Assignment

Both schemes are deterministic and consensus-critical, computed identically by
every participant. They differ in method (full detail in
[`SUM-STORE.md`](SUM-STORE.md)):

- **V1** hashes `merkle_root || chunk_index || replica`, maps modulo the node
  count to a starting position, and linear-probes forward on collision.
- **V2** scores every `(chunk, archive)` pair with
  `blake3::derive_key("sumchain SNIP-V2 chunk-assignment v1", merkle_root || chunk_index_be || archive_addr)`
  and takes the R lowest-scoring archives per chunk, tie-broken by address.

V2's rendezvous approach distributes more evenly and reshuffles more gracefully
when the active-node set changes, because each pair is scored independently
rather than depending on probe order.

## What the node runs today

The node runs both paths, but they are not symmetric in importance:

- For **V2 files**, the ingest, push validator, attestor, and finality waiter
  call the V2 RPC methods and wire protocol exclusively. This is the path all new
  files take.
- The **`MarketSyncWorker`** remains alive as a V1-legacy compatibility worker.
  It self-heals V1-registered files via the older algorithm and does **not**
  drive V2 retention; V2 retention is the chain's PoR-and-slashing job.
- **PoR challenge selection** currently enumerates V1-registered files; V2 PoR is
  on the roadmap (see the scope note in
  [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md), Step 5). Read that note before
  reasoning about challenge coverage for a V2 file.

## The routing boundary

On download, the pure router (`route_download_target`) reads the file's chain row
and dispatches to one of three pipelines: `V1Legacy`, `V2Public`, or `V2Private`.
The decision depends only on the chain row (its visibility byte), never on local
chunk-store or peer state, and an unknown visibility byte fails closed rather than
silently downgrading to public. That single, chain-driven decision point is what
keeps the two generations cleanly separated: a file is exactly one of V1, V2
public, or V2 private, and consensus is the source of truth for which.

## See also

- [`SUM-STORE.md`](SUM-STORE.md): the two assignment algorithms in detail
- [`SUM-NET.md`](SUM-NET.md): the V1 and V2 wire protocols
- [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md): the V2 flow end to end
- [`RPC-API.md`](../reference/RPC-API.md): the V1 and V2 RPC method families
