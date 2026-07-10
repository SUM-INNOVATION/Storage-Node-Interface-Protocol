# Versioning

Notes on how SNIP releases are numbered, what a release-candidate
tag means, and how the CHANGELOG relates to Cargo package versions.

## Semantics

SNIP loosely follows Semantic Versioning:

- **MAJOR** — reserved for a chain-compat break serious enough that
  operators need a heads-up. In practice this is coordinated with
  the SUM Chain release cycle.
- **MINOR** — user-visible feature adds (new subcommand, new
  operational surface, new chain gate SNIP starts reading).
- **PATCH** — bug fixes, doc-only changes, packaging.

Chain-version compatibility is tracked separately in
[`../reference/chain-compat.md`](../reference/chain-compat.md).
A patch bump is safe on the *same* pinned chain commit; a chain
re-pin is a MINOR at minimum.

## Release-candidate tags

Release candidates are tagged as `vX.Y.Z-rcN` (e.g. `v0.4.0-rc4`).
The tag-triggered release workflow publishes an rc tag as a
**draft** GitHub release with the `--prerelease` flag. Only the
final `vX.Y.Z` tag lands as a non-prerelease.

Draft releases require a human to publish. The release workflow
never auto-publishes; see
[`release-checklist.md`](release-checklist.md) §8a.

## CHANGELOG discipline

[`../../CHANGELOG.md`](../../CHANGELOG.md) follows Keep a Changelog:

- **`[Unreleased]`** at the top holds the running edit for the
  next release.
- Each shipped version has its own section stamped with the
  release date.
- Each section groups changes by category: **Added**, **Changed**,
  **Fixed**, **Removed**, **Verified** (informational). Docs-only
  changes are typically tagged **Added (docs)** / **Changed (docs)**.

At release-time, the release-checklist moves the current
`[Unreleased]` block under a new `[vX.Y.Z] — YYYY-MM-DD` heading.

## Package versioning

The `[workspace.package].version` in [`../../Cargo.toml`](../../Cargo.toml)
tracks the SNIP release, but is not always bumped in lockstep with
every doc-only patch. The authoritative human-facing version is the
git tag; the Cargo version is bumped at release-time as part of
the release checklist.

Individual crates (`sum-node`, `sum-net`, `sum-store`, `sum-crypto`,
`sum-types`) all inherit `version.workspace = true` — they are
versioned together.

## Cross-references

- Release checklist and gates: [`release-checklist.md`](release-checklist.md).
- Platform prebuilt tarball status: [`../compatibility/platform-support.md`](../compatibility/platform-support.md).
- Change log: [`../../CHANGELOG.md`](../../CHANGELOG.md).
