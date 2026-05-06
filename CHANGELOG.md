# Changelog

Notable changes to SNIP. The format follows Keep a Changelog
([keepachangelog.com](https://keepachangelog.com/en/1.1.0/)) and
loosely Semantic Versioning. Chain-version compatibility is tracked
in [`docs/CHAIN-COMPAT.md`](docs/CHAIN-COMPAT.md).

## [Unreleased]

### Added (Phase 4 — Private V2 file lifecycle)
- Private V2 file ingest, including encrypted manifest and
  per-chunk encryption under a per-file `K_file`. Owners receive
  their own access bundle automatically; recipients can be added
  at ingest time.
- Private V2 file download with V2-aware serving ACL. Caller
  resolves their access entry via paginated chain RPC, derives
  `K_file` via X25519 unwrap, fetches and decrypts the manifest,
  and fetches V2-assignment-aware ciphertext chunks. Non-authorized
  peers are refused at the serving boundary.
- Owner-side `share` / `revoke` / `update-access` CLI for Private
  V2 files. Signs `AddAccessV2` / `RemoveAccessV2` /
  `UpdateAccessV2`; the chain never sees `K_file`. Revoke does NOT
  rotate `K_file` (forward secrecy is a known Phase 5+ gap; see
  "Known gaps" below).
- Private V2 ingest **resume**. Recovers `K_file` from the on-chain
  access bundle (sync sibling of the paginated helper, safe under
  the access-list byte cap) and re-derives Private artifacts
  deterministically. Chain probe runs before re-derivation so
  abandoned/activated rows short-circuit.
- **Bounded per-chunk Private V2 download concurrency**. Wires the
  existing `--max-concurrent` CLI flag through to the chunk-fetch
  loop; assignment-aware routing preserved; pure
  `select_chunks_to_dispatch` selector keeps concurrency
  invariants testable without mocking the network layer.
- **Multi-peer Private V2 manifest fetch**. Dispatches up to
  `max_concurrent.clamp(1, 3).min(|distinct_assigned|)` requests
  to the V2-assigned archive set; first valid response wins;
  wrong-root / undecryptable / malformed responses fail only that
  archive. New typed `ManifestFetchAllArchivesFailed` error.

### Production-readiness scaffolding
- Top-level [`Makefile`](Makefile) with discoverable operator
  commands: `test`, `fmt`, `lint`, `lint-strict`, `build`,
  `release-check`.
- **Strict lint gate.** Workspace-level lint policy in
  [`Cargo.toml`](Cargo.toml) `[workspace.lints.clippy]` with three
  documented exemptions (orchestration entry-point arg counts,
  type-alias-not-warranted test fixtures, hanging-indent doc
  comments). All other clippy warnings cleared.
  `make lint-strict` enforces zero-warning workspace.
- Operator runbook ([`docs/OPERATOR-RUNBOOK.md`](docs/OPERATOR-RUNBOOK.md)),
  release checklist
  ([`docs/RELEASE-CHECKLIST.md`](docs/RELEASE-CHECKLIST.md)),
  privacy audit
  ([`docs/PRIVACY-AUDIT.md`](docs/PRIVACY-AUDIT.md)),
  chain-compatibility policy
  ([`docs/CHAIN-COMPAT.md`](docs/CHAIN-COMPAT.md)).

### Chain compatibility
- Targets internal chain release tag `<internal-chain-release-tag>`
  (private chain repository; exact commit verified out-of-band by
  chain ops).
- Local-mirror chain emits `v2_enabled_from_height: 0` (V2 enabled
  from genesis); SNIP deserializes as `Some(0)`, distinct from
  `None` (V2 disabled). Three deserialization tests in
  [`rpc_types.rs`](crates/sum-types/src/rpc_types.rs) pin this
  distinction.
- 14 bincode-v1 transaction fixtures and 7 RPC contract tests pin
  the V2 wire surface.

### Known gaps (deferred to Phase 5+)
- **Forward secrecy on revoke.** `revoke` removes the chain access
  entry but does NOT rotate `K_file`; a revoked recipient who
  cached ciphertext + bundle can still decrypt past content.
  Operators wanting forward secrecy must revoke + re-ingest under
  a fresh `K_file`.
- **Local-mirror E2E suite.** Chain ops shipped a runnable
  self-bootstrapping mirror at the pinned commit; operators can
  bring it up via the compose preset documented in
  [`docs/OPERATOR-RUNBOOK.md`](docs/OPERATOR-RUNBOOK.md). The
  SNIP-side WS2 suite that drives the mirror end-to-end through
  the full Phase 4 lifecycle is the next workstream. In the
  meantime, the in-tree fixture + contract tests + `make smoke`
  against the running mirror are the operator gate.
- **CI.** `.github/workflows/ci.yml` ships in WS8 of the
  production-readiness workstream; until then, `make release-check`
  is the single-command equivalent operators run locally.
