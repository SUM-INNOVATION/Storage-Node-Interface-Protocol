# sum-store

The storage crate. It owns everything a node does with chunks locally: the
on-disk chunk store, Merkle tree construction and proofs, the deterministic
assignment algorithms, garbage collection, the manifest index, and proof
verification. It depends on `sum-net` and `sum-types`. Source:
[`crates/sum-store`](../../crates/sum-store).

Several pieces here are **consensus-critical**: the assignment algorithm, the
Merkle tree shape, and proof verification must be bit-identical to the chain's
implementation, or a node will disagree with consensus about where chunks live
and whether a proof is valid.

## Module map

| Module | Responsibility |
|--------|----------------|
| `lib.rs` (`SumStore`) | top-level API tying the store, manifest index, and fetch together |
| `store.rs` (`ChunkStore`) | filesystem-backed chunk storage |
| `merkle.rs` (`MerkleTree`) | BLAKE3 binary Merkle tree, proof generation |
| `verify.rs` | BLAKE3 integrity and Merkle proof validation |
| `assignment_v2.rs` | V2 rendezvous-hash chunk assignment |
| `assignment.rs` | V1 hash-and-probe chunk assignment |
| `gc.rs` (`GarbageCollector`) | delete chunks no longer assigned to this node |
| `manifest.rs` | CBOR (de)serialization of `DataManifest` |
| `manifest_index.rs` (`ManifestIndex`) | `cid → merkle_root` reverse lookup |
| `fetch.rs` | pull chunks from peers |
| `announce.rs` | gossipsub chunk-availability announcements |

## The chunk store

[`store.rs`](../../crates/sum-store/src/store.rs) stores each chunk as a
content-addressed file, `<cid>.chunk`, in the chunk directory. The serving path
memory-maps the file (`ChunkStore::mmap`) so a 1 MiB chunk is served directly
from the mmap'd buffer with no heap allocation for the payload. The chunk size is
fixed at `CHUNK_SIZE = 1_048_576` (1 MiB), defined in `sum-types` and identical
everywhere, so every participant slices a file the same way.

## Merkle trees

[`merkle.rs`](../../crates/sum-store/src/merkle.rs) builds a binary tree over the
chunk hashes, bottom-up, pairing adjacent nodes and hashing the concatenation.
The critical detail is the **odd-node rule**: when a level has an odd number of
nodes, the last node is duplicated (hashed with itself). The chain uses the same
rule, so both sides must agree, and this is exactly the kind of thing that would
silently break proofs if it drifted.

`generate_proof(chunk_index)` walks from the leaf to the root collecting the
sibling hash at each level; the proof is `O(log2(chunk_count))` hashes. For 10
chunks that is 4 hashes (128 bytes) instead of shipping the 1 MiB chunk.

## Proof verification

[`verify.rs`](../../crates/sum-store/src/verify.rs) is what the V2 push validator
calls before writing an inbound chunk.
`verify_merkle_proof_bytes_for_tree(chunk_hash, chunk_index, merkle_path, expected_root, total_leaves)`
rejects three shapes up front, an empty tree, an out-of-range index, and a proof
whose length does not equal the canonical depth for `total_leaves`, before
walking the hash chain. The walk uses the bit rule `(chunk_index >> level) & 1`
to decide, at each level, whether the running hash is the left or right child.
That bit rule must match the chain's verifier exactly.

## Deterministic assignment

Both algorithms answer the same question, which R archives out of the active set
should hold a given chunk, and both are computed identically by uploader,
archives, and validators from public on-chain inputs. They differ in method.

**V2 (rendezvous hash)**, in
[`assignment_v2.rs`](../../crates/sum-store/src/assignment_v2.rs), is the current
scheme. For each `(chunk_index, archive)` pair it computes a score:

```
context = "sumchain SNIP-V2 chunk-assignment v1"
input   = merkle_root(32) || chunk_index.to_be_bytes()(4) || archive_l1_address(20)
key     = blake3::derive_key(context, input)
score   = u64::from_be_bytes(key[0..8])
```

The R archives with the lowest scores for a chunk are its replicas, with ties
broken by ascending L1 address. The context string, the big-endian byte order,
the `blake3::derive_key` variant, and the tie-break are all consensus-critical:
any divergence breaks chain conformance. `chunks_for_archive_v2(...)` returns the
`BTreeSet<u32>` of chunk indices a given archive owns, which is how a node learns
what to hold.

**V1 (hash and linear probe)**, in
[`assignment.rs`](../../crates/sum-store/src/assignment.rs), is the legacy scheme:
it hashes `merkle_root || chunk_index || replica`, maps the result modulo the
node count to a position in the sorted node list, and linear-probes forward on
collision. It is retained for V1-registered files. The difference from V2 is
structural: V1 picks one position per (chunk, replica) and probes; V2 scores
every archive independently and takes the top R. See
[`V1-VS-V2.md`](V1-VS-V2.md).

## Garbage collection

[`gc.rs`](../../crates/sum-store/src/gc.rs) runs `mark_and_sweep` after each
MarketSync cycle. It compares the local chunks against the set currently assigned
to this node; a chunk that is no longer assigned enters a grace period (default 1
hour, `--gc-grace-secs`) and is deleted only after the grace elapses. Two safety
rails: it never deletes a currently-assigned chunk, and it pauses entirely if the
chain has not been polled within 5 minutes, so a stale view of assignments cannot
trigger deletion. The grace tracking is in-memory, so a restart conservatively
resets the timers.

## Manifest index

[`manifest_index.rs`](../../crates/sum-store/src/manifest_index.rs) maintains the
`cid → merkle_root` reverse map (`merkle_root_for_cid`), with separate tables for
public and private files. The ACL gate uses it: given a chunk-pull request for a
CID, it maps the CID back to its file root to look up the access list. A CID in
neither table is denied.

## Manifest format

[`manifest.rs`](../../crates/sum-store/src/manifest.rs) serializes the
`DataManifest` (defined in `sum-types`) as CBOR, a compact binary format. Public
manifests are stored as `<hex_root>.cbor`; private manifests are stored as opaque
encrypted blobs (`<hex_root>.opaque`). The manifest carries the file name, the
whole-file hash, total size, chunk count, merkle root, and an ordered list of
chunk descriptors (index, offset, size, BLAKE3 hash, CID, and an optional
plaintext hash for private chunks). The CBOR encoding is consensus-relevant: the
receiver of a `ManifestPush` recomputes the merkle root from the descriptors and
rejects on mismatch.

## See also

- [`SUM-NET.md`](SUM-NET.md): the codec that carries chunks to and from this store
- [`V1-VS-V2.md`](V1-VS-V2.md): the two assignment schemes compared
- [`CMPLT-PROC.md`](../reference/CMPLT-PROC.md): assignment and proofs in the full flow
