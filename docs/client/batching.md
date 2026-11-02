# Client: small files, packing and the per-file cost

Every file SNIP stores is registered on chain under its own merkle
root, and the chain accepts one registration per root. There is no
transaction that registers several files at once. A caller that
produces many small files batches them **on the client side**: pack
them into one archive file and upload that file. This page gives
what one file costs, why a file smaller than one chunk costs as much
as a full chunk, and what the files in a pack share.

Nothing on this page changes the chain or the client. It describes
what the code does today (SNIP #57 item 3).

## What one file costs

The owner submits two transactions for every file, and each waits
for `Finalized` before the pipeline goes on
([`upload-and-download.md`](upload-and-download.md), steps 3 and 8):

1. `RegisterFilePendingV2`: registers the root, sizes, chunk count,
   visibility and initial access list, and locks the fee deposit.
2. `ActivateFileV2`: moves the file from `Pending` to `Active` once
   every chunk is attested.

What the chain charges for them:

- **Transaction fee.** Each transaction carries a signed `fee`, and
  the chain refuses a fee below `min_fee`. The chain deducts the fee
  from the sender before it checks the operation, so an operation it
  refuses still costs its fee (for example, `ActivateFileV2` before
  coverage is complete). SNIP signs both transactions with
  `fee = min_fee`, read from `chain_getChainParams`
  (`IngestParams::fee_per_tx`, filled by
  `IngestParams::from_chain_params`).
- **Fee deposit.** `RegisterFilePendingV2` moves `fee_deposit` from
  the owner's balance into the file's fee pool
  (`IngestParams::fee_deposit`, CLI `--fee-deposit`, default `0`).
  The pool pays the archives' challenge rewards. While the file is
  still `Pending`, `abandon` returns `(100 - abandonment_fee_percent)`
  percent of the pool, once `activation_grace_blocks` have passed
  since registration.
- **Nothing per byte.** Neither transaction has a size-dependent
  charge. `chain_getChainParams` also reports `storage_fee_per_byte`.
  It is the chain's fee for NFT metadata and is **not** charged on
  these two operations.

So, for one file uploaded without retries:

```text
fees spent per file     = fee(RegisterFilePendingV2) + fee(ActivateFileV2)
                        = 2 × min_fee                (as SNIP signs them)
deposit locked per file = fee_deposit                (caller's choice; default 0)

for N files packed into one file:
fees spent per member     = 2 × min_fee / N
deposit locked per member = fee_deposit / N
```

Each further transaction on the file costs one more fee: for example
`abandon`, `share`, `revoke` or `update-access`, or an
`ActivateFileV2` that has to be sent again.

The archives also pay per file. Each archive that holds a chunk
assigned to it submits at least one `AcceptAssignmentV2` for that
file, at its own `--attest-fee`, with up to `max_chunk_indices_per_tx`
chunk indices each. A one-chunk file is assigned to
`R = assignment_replication_factor` archives, so up to `R` such
transactions. Packing reduces these as well.

The time also counts per file, not per byte: two finality waits and
the coverage wait between them. Run one operation at a time per
network node ([`library.md`](library.md), "Using one network node").

### Reading the parameters

All of these come from one read-only call, which the CLI and
`IngestParams::from_chain_params` already make:

```bash
curl -s -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"chain_getChainParams","params":[]}' \
    https://rpc.sumchain.io
```

Amounts are in base units: 1 Koppa is `1_000_000_000` base units.
These are chain parameters and can change, so read them when you
compute a cost rather than copying them from here.

**Example: mainnet values read on 2026-11-02 (23:22 UTC)** from
`https://rpc.sumchain.io`:

| Field | Value |
|---|---|
| `min_fee` | `1000` |
| `storage_fee_per_byte` | `100` (NFT metadata only, not charged here) |
| `assignment_replication_factor` | `3` |
| `abandonment_fee_percent` | `10` |
| `activation_grace_blocks` | `50` |
| `max_chunk_count_per_file` | `1048576` |
| `max_chunk_indices_per_tx` | `65536` |

With these values, one file costs its owner `2 × 1000 = 2000` base
units in fees (0.000002 Koppa), whatever its size, plus the
`fee_deposit` the caller chooses. A pack of 100 files costs the same
2000 base units, so 20 base units per member.

## Why a small file costs a full chunk

The chain requires
`chunk_count = ceil(stored_size_bytes / CHUNK_SIZE)`, with
`CHUNK_SIZE = 1_048_576` bytes (1 MiB, from `sumchain-wire`), and
refuses `stored_size_bytes = 0` and `chunk_count = 0`.

- A Public file stores its plaintext, so one chunk holds up to
  1,048,576 bytes of the file.
- A Private file stores ciphertext with a 16-byte tag per chunk, so
  one chunk holds up to 1,048,560 bytes of plaintext.
- A file of one byte, and a file of one full chunk, are each one
  chunk. Each has one registration, one activation, `R` assigned
  archives, up to `R` attestations and one row on chain. They cost
  the same.
- An empty file cannot be registered. Put empty files in a pack.

**SNIP sets no minimum file size** beyond the chain's one byte.
Since the costs above are per file, the cost per stored byte is:

```text
fees per byte = 2 × min_fee / file_size
```

Below `CHUNK_SIZE`, bytes added to the same file are free on chain.
A file smaller than one chunk is the case where packing helps most:
fill the pack to at least one chunk before uploading it. Above one
chunk, the owner's fees stay at `2 × min_fee`, up to
`max_chunk_count_per_file × CHUNK_SIZE` stored bytes. The limits on
pack size come from the trade-offs below, not from fees.

## Packing several files into one

1. Pack the files into one archive file, for example a tar file. SNIP
   does not read the contents; any format your readers can unpack
   will do.
2. Upload the archive as one file: `sum-node ingest-v2 <archive>`, or
   `IngestPipeline::ingest` / `ingest_private` with an
   `IngestInput::Path` or `IngestInput::Bytes`
   ([`library.md`](library.md)).
3. Keep your own index from each member to the pack's root. Neither
   the chain nor the manifest lists the members: the manifest
   describes the pack's chunks, under the pack's file name.
4. To read a member, download the pack and unpack it. `sum-node
   download`, `download_to_path` and `download_to_bytes` fetch a file
   whole, by root, and verify it against the root. Neither the CLI nor
   the library has a call that fetches one member or a byte range.

**Computing the root before the upload.** A Public file's root
depends only on its bytes. If you commit to the root before any
network contact, build the pack reproducibly: a fixed member order,
and fixed timestamps, owners, permissions and compression settings.
A Private file's root is over the ciphertext under a fresh key, so
it is known only after `ingest_private` returns.

## What the members of a pack share

Everything the chain tracks is per root, so a pack behaves as one
file:

- **Lifecycle.** One `Pending`, `Active` or `Abandoned` state. The
  upload succeeds or stalls as a whole, and `resume` or `abandon`
  acts on the whole pack.
- **Visibility and access.** Public or Private, and the access list
  (`share`, `revoke`, `update-access`), apply to every member. Put
  files that need different readers in different packs.
- **Replication and coverage.** Chunks are assigned per root and
  chunk index, each to `R` archives. Activation needs every chunk of
  the pack attested. A member that spans several chunks depends on
  the archives of all of them.
- **Proof of retrievability.** A challenge selects a file (a root),
  then a chunk in it
  ([`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md)).
  A pack is one candidate with one fee pool. N files registered
  separately, each with a deposit, are N candidates with N pools. A
  file with no deposit is never challenged.
- **Retrieval cost.** Reading one member downloads the whole pack.
  The bigger the pack, the more is fetched for one member.

## Not provided

- Several roots in one registration, or a member-level index on
  chain. Either would need a chain change.
- Preparing a file offline and publishing it later (SNIP #57 item 2);
  see [`library.md`](library.md), "Not covered".
