# Upgrading

How to move a node from one SNIP release to the next, and what to check when the
chain it talks to changes underneath it. Pairs with
[`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md) (cutting a release) and
[`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md) (the chain-version contract).

## The compatibility contract

SNIP is a client of SUM Chain. The binding constraint on any upgrade is
wire-format compatibility: bincode-v1 transaction payloads and JSON-RPC response
shapes must stay byte-aligned with the chain the node talks to. That contract is
pinned by contract tests in the repo and documented in
[`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md), which records the exact chain
commit, genesis SHA-256, and the `v2_enabled_from_height` the release was
validated against. Read it before upgrading.

Two independent things can change, and they upgrade differently:

- **The SNIP binary** (a new `sum-node` release). This is a local operation.
- **The chain** (a new chain version, or a V2 activation height passing). SNIP
  reacts to this; you do not upgrade the chain from here.

## Upgrading the SNIP binary

An archive node holds no local state that a new binary cannot rebuild from the
chunk store and the chain, so the upgrade is a stop, swap, start:

1. **Read the CHANGELOG.** Check [`CHANGELOG.md`](../../CHANGELOG.md) for the
   target version's entry, especially any "Known issues" and any note that the
   release requires a minimum chain version.
2. **Confirm chain compatibility.** Verify the running chain matches what the new
   release was validated against ([`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md)).
   A SNIP release that expects a newer wire shape against an older chain (or vice
   versa) will fail contract-level, and transactions may be rejected with the fee
   burned.
3. **Install the new binary** alongside the old one
   ([`INSTALL.md`](../reference/INSTALL.md)). Keep the old binary so you can roll
   back.
4. **Stop the node**, swap the binary, **start it** again with the same key file,
   RPC URL, and profile. The chunk store on disk is reused as-is.
5. **Verify** with the read-only smoke check and confirm the node re-registers
   its presence and resumes answering challenges: watch for `PoR proof submitted`
   in the logs and confirm `status: Active` on chain
   ([`MONITORING.md`](MONITORING.md)).

Rolling back is the same sequence with the previous binary, as long as no
one-way chain-side change (below) has happened in between.

## When the chain changes: V2 activation

The one chain-side transition an operator must plan around is the V2 storage
lifecycle going live. The chain advertises `v2_enabled_from_height` in
`chain_getChainParams`. SNIP gates every V2 transaction (`RegisterFilePendingV2`,
`AcceptAssignmentV2`, `ActivateFileV2`, `AbandonFileV2`, `RegisterEncryptionKey`)
on the finalized height having reached that value. Behavior around the boundary:

- While `v2_enabled_from_height` is `null`, SNIP refuses to submit V2
  transactions at all. This is deliberate: submitting against a chain that has
  not activated V2 would burn fees.
- Before the height is reached, V2 submissions are held back; after it is
  reached (and finalized), they proceed.
- A value of `0` means "V2 from genesis" and is distinct from `null`.

There is nothing to do to "upgrade" a node across this boundary beyond running a
binary that supports V2 (any current release). The gate flips automatically when
the finalized height crosses the threshold. Confirm the parameter with a smoke
check ([`MONITORING.md`](MONITORING.md)) if you are unsure whether V2 is live on
your chain.

## Release-candidate to stable

Release tags move `rc` to `rc` and finally to a stable `vX.Y.Z`. Per the
bring-up guide, a stable tag is cut at the latest release-candidate commit that
has passed all gates, which is not necessarily the last `rc` you ran. When
promoting, always check out the tag named in
[`MAINNET-BRINGUP.md`](MAINNET-BRINGUP.md) and
[`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md) rather than assuming the newest
`rc` is current, and re-run the smoke check after swapping.

## See also

- [`CHAIN-COMPAT.md`](../reference/CHAIN-COMPAT.md): the version pin and wire contract
- [`RELEASE-CHECKLIST.md`](RELEASE-CHECKLIST.md): cutting and validating a release
- [`MONITORING.md`](MONITORING.md): confirming a node is healthy after the swap
- [`CHANGELOG.md`](../../CHANGELOG.md): per-version changes and known issues
