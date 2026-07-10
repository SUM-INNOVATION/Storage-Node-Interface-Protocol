# Documentation archive

This directory preserves historical planning documents. **Nothing
here is current-day authoritative.** Every file carries an
`ARCHIVED — historical planning document` preamble; if a preamble
is missing, treat the file as normative and file an issue.

Documents move here — rather than being deleted — because they
provide useful context on *why* the code is shaped the way it is.
Anyone tracing a design decision back through history should find
the reasoning intact.

## Archived documents

| File | Superseded by | Notes |
|---|---|---|
| [`PLAN.md`](PLAN.md) | [`../status/implementation-status.md`](../status/implementation-status.md), [`../reference/chain-compat.md`](../reference/chain-compat.md) | Uses V1 transaction shapes throughout; Phase 5 encryption listed as "not started" — the Private V2 encryption implementation ships. |
| [`PHASE4-EXECUTION-PLAN.md`](PHASE4-EXECUTION-PLAN.md) | [`../status/implementation-status.md`](../status/implementation-status.md), [`../roadmap/roadmap.md`](../roadmap/roadmap.md) | Objectives 1/2/4/5 shipped; Objective 3 (Reed-Solomon) remains in the roadmap. |
| [`PHASE4-IMPLEMENTATION-PLAN.md`](PHASE4-IMPLEMENTATION-PLAN.md) | [`../status/implementation-status.md`](../status/implementation-status.md) | Download command, GC, resilient upload have all shipped. |
| [`CLIENT-MODE-GAP.md`](CLIENT-MODE-GAP.md) | [`../client/upload-and-download.md`](../client/upload-and-download.md), [`../status/implementation-status.md`](../status/implementation-status.md) | Phases 1 and 2 shipped; Phase 3 (lightweight-swarm client) remains future work. |
| [`WAN-DISCOVERY-AND-HARDENING.md`](WAN-DISCOVERY-AND-HARDENING.md) | [`../architecture/networking.md`](../architecture/networking.md) | Networking + hardening shipped; current-day networking material extracted. |
| [`SECURITY-ANALYSIS.md`](SECURITY-ANALYSIS.md) | [`../security/threat-model.md`](../security/threat-model.md), [`../security/privacy-audit.md`](../security/privacy-audit.md) | Proposed a symmetric-only ChaCha20-Poly1305 design with out-of-band key sharing; the shipped Private V2 design uses X25519 hybrid encryption with chain-side wrapped bundles. |

## Reading rules

- Statements about "NOT STARTED" / "IN PROGRESS" / "DONE" / phase
  labels in archived documents describe the state at the time each
  document was written. Do not treat them as current.
- Line ranges that point into `crates/` may be stale; the source
  tree has moved on since these documents were last edited.
- Cross-references may point to paths that no longer exist. When
  they do, the current-day equivalent is listed in the table above.

## What is not archived

Documents that describe current-day behavior remain outside this
directory:

- Operator + mainnet material: [`../operator/`](../operator/)
- Chain wire compatibility: [`../reference/chain-compat.md`](../reference/chain-compat.md)
- Privacy pinning guards: [`../security/privacy-audit.md`](../security/privacy-audit.md)
- Platform support matrix: [`../compatibility/platform-support.md`](../compatibility/platform-support.md)
- Release checklist: [`../release/release-checklist.md`](../release/release-checklist.md)

If any of those were archived by mistake, please open an issue —
they should remain in the normative tree.
