# Proof of Retrievability (PoR)

Storage archives on SNIP are held accountable for keeping the data
they agreed to hold. The SUM Chain periodically challenges an archive
to produce a Merkle proof for a specific chunk of a specific file;
failure to submit a valid proof within a bounded window slashes the
archive's stake. This document is the authoritative source of truth
for how challenge targeting, challenge scheduling, and the SNIP-side
responder work today.

> **A note on terminology.** Some upstream `sum-chain` issues use the
> phrase *"Proof-of-Retention"* in their titles. SNIP itself uses
> *"Proof of Retrievability"* (abbreviated **PoR**) throughout — the
> chain-side function is `SubmitStorageProof`, and the property being
> proven is that the archive can retrieve the requested chunk on
> demand, not simply that it stored the chunk once. Upstream titles
> are preserved verbatim when cited; SNIP prose consistently uses
> "Proof of Retrievability" / "PoR".

## Two mechanisms, one system

PoR has two orthogonal parts. Confusing them has produced concrete
documentation drift in the past; keep them separate.

| Mechanism | What it decides | Status |
|---|---|---|
| **Challenge targeting** | For a given challenge, *which archive* is asked to prove *which chunk* of *which file* | Implemented in `sum-chain` (see `sum-chain` issue #97). Gated by a `chain_getChainParams` feature flag — verify at runtime. |
| **Bounded coverage scheduling** | Over time, *how often* each chunk of each funded file is challenged, so every chunk gets audited within a bounded window | Design-only. Tracked upstream as `sum-chain` issue #81; specification in `sum-chain:docs/specs/snip-assignment-aware-por-scheduling.md`. |

Targeting answers "*given* a challenge, who is it for?" Scheduling
answers "*when* does the next challenge fire, and against what?"
These are separate axes of the design. A change to one does not
imply a change to the other.

## V2 assignment-aware challenge targeting (`sum-chain` issue #97)

**Status**: implemented in `sum-chain`; gated by a
`chain_getChainParams` feature flag (**`assignment_targeting`**)
whose current value on any given deployment must be read at runtime
via `chain_getChainParams`. Do not assume the gate is on or off in
prose; document behavior for both cases.

**Chain-side entry points** (external repository; cited by symbol
rather than line number so minor code movement does not break the
reference):

- `generate_challenge` — challenge generator run by `execute_block`
  every `CHALLENGE_INTERVAL_BLOCKS` — `sum-chain:crates/state/src/storage_metadata.rs`.
- `funded_active_v2_candidates()` — helper that returns the V2
  funded-and-active candidate set consulted by `generate_challenge`
  when the gate is on — `sum-chain:crates/state/src/storage_metadata.rs`.
- Assignment-targeting test coverage —
  `sum-chain:crates/state/tests/por_assignment_targeting.rs`.

**Cross-repository evidence.** The line ranges and behavior of these
functions are taken from the SNIP-side issue #31 report, which cites
the `sum-chain` source at the pinned commit recorded in
[`../reference/chain-compat.md`](../reference/chain-compat.md)
"Pinned chain version." SNIP does not independently verify against
`sum-chain` source at the time this document was written; the pinned
chain commit is the load-bearing reference.

### When `assignment_targeting` is enabled

- The challenge generator restricts eligibility to V2 files with a
  positive `fee_deposit` and a non-empty active-accepting archive
  set (`funded_active_v2_candidates()`).
- The seed selects a file and a chunk index.
- The `target_node` is drawn from **that chunk's assigned-active
  archive set** — not from the full pool of active archives.
- Archives never assigned to the chunk are not challenged for it.

### When `assignment_targeting` is disabled

- The generator falls back to the pre-#97 selection: the target is
  drawn uniformly from all active archives, regardless of assignment.
- Consequence for archives: an active archive may be challenged for a
  chunk it was never assigned to hold. To answer the challenge in
  time, the archive would need to fetch the chunk from a peer (via
  the discovery mechanisms described in
  [`../architecture/networking.md`](../architecture/networking.md))
  and submit a valid Merkle proof within `CHALLENGE_TTL_BLOCKS`.

### V1 legacy path

Files registered on the V1 path (`sum-node --client ingest`,
without the `V2` suffix) continue to use the pre-#97 selection
regardless of gate state. This path exists for backwards
compatibility with V1 files that predate chain-plan v3.2 and is not
the mainnet write path today.

## Bounded coverage scheduling (`sum-chain` issue #81) — planned

**Status**: design-only. Specified in
`sum-chain:docs/specs/snip-assignment-aware-por-scheduling.md`.

Targeting decides *who* is challenged when a challenge fires.
Scheduling would additionally guarantee that *every chunk of every
funded file* is challenged within a bounded window — closing the gap
where a large file with many chunks could go uncontested for long
stretches purely by seed luck. The scheduler is a policy that would
run above the challenge generator; it is a separate mechanism from
`assignment_targeting`.

SNIP does not implement or emulate a bounded coverage scheduler on
the archive side. When `sum-chain` #81 lands, archives will observe
a different distribution of challenges over time — the on-wire
`SubmitStorageProof` shape and the SNIP-side responder do not
change.

## Responder side (SNIP)

SNIP archives learn about challenges targeting them via the
[`PorWorker`](../../crates/sum-node/src/por_worker.rs) background
task, which polls `storage_getActiveChallenges` on the configured
poll interval. When a challenge fires:

1. `PorWorker` receives the pending challenge from RPC (challenge
   ID, `merkle_root`, `chunk_index`, `expires_at_height`).
2. It looks up the chunk locally, computes a BLAKE3 Merkle proof
   from the persisted manifest, and constructs a
   `SubmitStorageProof` transaction via
   [`tx_builder`](../../crates/sum-node/src/tx_builder.rs).
3. The transaction is signed with the archive's Ed25519 seed and
   submitted through the RPC layer.
4. The chain-side `execute_submit_proof` validates that
   `challenge.target_node == sender`, that the current height is
   less than or equal to `expires_at_height`, and that the Merkle
   proof reconstructs the file's on-chain `merkle_root`. On success
   the challenge is cleared and the archive earns the payout from
   the file's `fee_pool`; on failure or expiry, the chain's
   `process_expired_challenges` slashes a percentage of the
   archive's stake and flips its `NodeStatus` to `Slashed`.

SNIP's responder logic is protocol-version-agnostic: the same
`PorWorker` and `tx_builder` machinery answers challenges regardless
of whether the file was registered via the V1 or V2 path, and
regardless of the state of the `assignment_targeting` gate. The gate
governs *which* archives receive challenges, not how those archives
respond.

Operational tuning surface — CLI: `--por-poll-secs` (default 10 s;
env `SUM_POR_INTERVAL`). See [`../reference/cli.md`](../reference/cli.md).

## Operator implications

Provisioning an archive depends on the current gate state on the
chain the archive is joining. Both behaviors are supported by SNIP;
the difference lies in what workload the archive must handle.

- **Gate on (assignment-targeting active).** An archive is asked to
  prove only chunks it was deterministically assigned to hold. Disk
  provisioning maps to the assignment algorithm's expected footprint
  for the archive's L1 address at the current active-node count.
  Off-assignment chunks are not challenged for this archive.
- **Gate off (uniform targeting).** An archive may be asked to prove
  any chunk of any funded V2 file (or V1 file, always). The archive
  must be able to fetch an unassigned chunk from a peer and submit a
  valid Merkle proof within `CHALLENGE_TTL_BLOCKS` blocks, or accept
  the slash. Disk provisioning for the assigned set remains the
  same; incremental network + peer-discovery reliability becomes
  load-bearing.

An operator MUST verify the gate at runtime via
`chain_getChainParams` — the mainnet `assignment_targeting` value is
not restated as a fixed constant in SNIP documentation because that
value is a property of the chain deployment, not of SNIP itself. See
[`../reference/chain-compat.md`](../reference/chain-compat.md)
"Pinned chain version" for how the chain-facing feature-gate
surface is discovered.

## Cross-references

- Feature-gate table: [`../architecture/chain-integration.md`](../architecture/chain-integration.md).
- Current-day feature status: [`../status/implementation-status.md`](../status/implementation-status.md).
- Wire fixtures + RPC contract tests SNIP pins for the surface it
  interacts with: [`../reference/chain-compat.md`](../reference/chain-compat.md).
- V2 file lifecycle SNIP submits before PoR becomes eligible:
  [`v2-state-machine.md`](v2-state-machine.md).
- Upstream open items:
  [`sum-chain` issue #81](https://github.com/SUM-INNOVATION/sum-chain/issues/81)
  (bounded coverage scheduling — planned),
  [`sum-chain` issue #97](https://github.com/SUM-INNOVATION/sum-chain/issues/97)
  (assignment-aware challenge targeting — shipped, gated).
