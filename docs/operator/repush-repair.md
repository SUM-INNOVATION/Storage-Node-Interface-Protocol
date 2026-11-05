# Repairing push-proof records: `repush` and `repush-audit`

An archive answers a proof-of-retrievability challenge on a chunk of a
Private V2 file (and of a Public V2 file whose manifest it does not hold)
from the leaf and Merkle path the chunk's V2 push carried, which it
records when it accepts the push (see
[`../protocol/proof-of-retrievability.md`](../protocol/proof-of-retrievability.md),
"Responder side"). Chunks pushed to an archive before it kept that record
have none, and a challenge on one is not answered.

Two commands repair and check this. Neither sends a transaction.

## The owner: `sum-node repush`

The file's owner re-pushes every chunk of the file, and its manifest, to
the archives its latest assignment epoch assigns them to. An archive that
already holds a chunk acknowledges the repeat push and records the proof
the push carries; an archive that never received the chunk stores it.

```bash
sum-node --key-file <owner key> --rpc-url <rpc> \
    repush <merkle_root> <path to the original file>
```

- The operator must be the registered owner; the file must rebuild to the
  registered root, chunk count and sizes. A Private file is re-encrypted
  under the `K_file` recovered from the owner's own access entry, so the
  owner needs only the plaintext and the key.
- Name the file as it was named at ingest to push identical manifest bytes
  (the manifest records the file name). An archive that already holds a
  Private manifest keeps it either way.
- `repush: complete` (exit `0`) means every push was acknowledged and the
  assignment epoch did not change during the run. Anything else is
  `repush: incomplete` (exit `3`); run it again. A reassignment during the
  run is reported as `repush_epoch_changed:`, with the pushes the new epoch
  still needs logged.

**An acknowledgement is not the proof.** An archive running a version
without push-proof records acknowledges a push too. Whether a chunk can
be answered is shown on the archive, by `repush-audit`.

**Order.** Repair archives that run a version recording push proofs. A
repush to an archive that does not yet record them has to be repeated
after it is upgraded.

**Limit.** An owner who no longer has a file's plaintext (or, for a
Private file, its key) cannot rebuild it, so that file's existing chunks
cannot be repaired this way.

## The archive: `sum-node repush-audit`

On each archive, check the files of an explicit inventory:

```bash
sum-node --key-file <archive key> --store-dir <store> --rpc-url <rpc> \
    repush-audit --roots-file inventory.txt
```

`inventory.txt` lists merkle roots, one per line (`#` comments allowed);
`--root <hex>` adds one. For each file the audit takes the chunks the
file's latest assignment epoch assigns to this archive, builds the answer
the PoR responder would submit from the local store, and checks it with the
chain's verifier.

- **Usable:** the answer verifies, through the file's Public manifest (a
  Public file whose manifest this archive holds) or through the chunk's
  recorded push proof. Either way it is built from the stored chunk's
  bytes, so a missing or altered chunk is not usable.
- `repush_audit: complete` (exit `0`): every assigned chunk of every
  inventory file is usable. `incomplete` (exit `3`) lists each chunk that
  is not, with the reason. `error` (exit `1`): a chain read failed, or a
  root is not registered.

**The inventory decides, not the index.** A file this archive never
received is absent from its store, so an empty or partial index cannot
show that nothing needs repair. With no inventory the audit reports
`no_inventory` (exit `7`), never `complete`. The inventory has to come from
the files' owners: no chain RPC lists the files an archive is assigned.

**Read-only.** The audit opens the store without writing and without its
lease, so it can run beside a running archive. A push that lands while it
runs may not be seen; run it again.

## What a complete audit does not show

- That the chain will challenge or reward any chunk.
- Anything about files outside the inventory.
- That other archives hold their chunks: run the audit on each archive.
- That the answer reaches the chain: the responder still has to be running
  and able to submit the proof.
