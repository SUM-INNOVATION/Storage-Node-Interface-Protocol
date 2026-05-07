# Changelog

Notable changes to SNIP. The format follows Keep a Changelog
([keepachangelog.com](https://keepachangelog.com/en/1.1.0/)) and
loosely Semantic Versioning. Chain-version compatibility is tracked
in [`docs/CHAIN-COMPAT.md`](docs/CHAIN-COMPAT.md).

## [Unreleased]

## [0.4.0-rc1] - 2026-05-07

Release candidate; not final production. Operators on a fresh
machine should follow [`docs/OPERATOR-RUNBOOK.md`](docs/OPERATOR-RUNBOOK.md)
end-to-end and reproduce the local-mirror E2E suite at least once
before this is promoted to `v0.4.0`.

### Added (WS2b — chain compatibility + local-mirror E2E)
- **V2 download protocol fix.** SNIP's download paths now use the
  V2 request/response helpers (`pull_manifest_v2`, `pull_chunk_v2`,
  `ShardReceivedV2::{ManifestData, Data}`) for any file that has a
  V2 chain row. With V2 advertised first in the request_response
  codec list, libp2p picks `/sum/storage/v2` for every new outbound
  substream between V2-capable peers, so V1 helpers were
  unreachable on V2-enabled chains and every V2 download timed out
  with "V1 request written to non-V1 stream". V1 helpers + the V1
  `FetchManager` are retained for `DownloadPath::V1Legacy` only.
  The fix preserves the existing decrypt / plaintext-hash / ACL
  logic; only the byte transport changed.
- **`DownloadOrchestrator::run_v2_public`.** New V2-aware Public
  download path that mirrors the V1 four-phase flow on V2 helpers
  end-to-end. Hard-fails with a typed
  "exhausted all V2-assigned archives" message when assignment is
  unresolvable — no "any connected peer" fallback. For V2 rows the
  chain's assignment is the truth.
- **Strict single-shot V2 Pull validation.** Both Private and
  Public V2 chunk-fetch paths reject windowed responses
  (`offset != 0`, `total_bytes != expected`, `data.len() !=
  expected`) and try the next assigned archive rather than
  silently assembling partial ciphertext.
- **`crates/sum-node/src/download_v2_routing.rs`.** Shared module
  for V2 routing helpers — `V2AssignmentView` /
  `build_v2_assignment_view` (snapshot + per-chunk archive list)
  and `decode_v2_manifest_bytes` (CBOR + root-equality check).
  Pure code motion that lets Public and Private V2 paths share the
  assignment build.
- **`sum-node register-node` operator CLI.** Production submit-and-
  wait flow for `NodeRegistry::Register(ArchiveNode { stake })`.
  Reads `chain_id` live from RPC (no `--chain-id` flag drift
  burning a fee against the wrong network), waits for finality via
  `wait_for_finalized`, and prints a stable
  `tx_hash: 0x<hex>` + `finalized_height: <N>` stdout contract.
  The dev-only `e2e-helper register-node` (RPC fire-and-forget) is
  retained for legacy callers.
- **Stable `merkle_root:` / `lifecycle:` stdout from `ingest-v2`
  and `resume`.** Every IngestOutcome variant that carries a
  recorded merkle root prints a parseable
  `merkle_root: 0x<hex>` line + a `lifecycle:
  <Active|Pending|Abandoned>` line on stdout (tracing log lines
  preserved for humans). Operators get a machine-readable handle
  on the root regardless of whether S2/S3/S4 succeeded.
- **`archive_3` role in `e2e-helper generate-e2e-keys`.** The
  chain plan fixes `assignment_replication_factor = 3`; the e2e
  role set previously shipped only `archive_1` and `archive_2`,
  leaving the harness stuck in S2 under-replication. The
  generator now emits seeds and an alloc snippet for all three
  archives. Operators bringing up a new mirror after this release
  must `down -v` so the genesis allocation overlay can fund the
  third address.
- **Local-mirror E2E harness.** 11 ignored scenarios under
  `crates/sum-node/tests/e2e_mirror.rs`, gated behind
  `make e2e-mirror`, never part of `release-check` or PR CI.
  Order-independent — each scenario stands up its own preconditions
  via idempotent helpers (`ensure_archive_registered`,
  `ensure_encryption_key_registered`, `spawn_archive_fleet` that
  waits for `storage_getActiveNodesAtHeight` to show ≥ R archives).
  `unique_plaintext(scenario, bytes)` generates fresh bytes per
  invocation so two runs of the same scenario never collide on
  merkle root. Scenario 12 (archive restart) deferred to a
  follow-up.

### Fixed (WS2b)
- **`L1RpcClient::send_raw_transaction`** now returns
  `Result<String>` and tolerates two chain wire shapes: bare hex
  string (`"0xabc..."`) and wrapped object
  (`{"tx_hash": "0xabc..."}`). Previously the wrapped form silently
  passed a JSON blob into `chain_getTransactionStatus` and the
  finality lookup failed with a non-actionable error. 6 contract
  tests pin both wire shapes plus 4 typed-error rejection paths.

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
- **Scenario 12 (archive restart).** Process-lifecycle / port-reuse
  / store-reload coverage is deferred from the WS2b harness; it
  needs an isolated review pass before landing.
- **Windowed V2 Pull responses.** The V2 download orchestrator
  rejects partial responses with "partial V2 pull unsupported"
  and tries the next assigned archive. Acceptable for now because
  every V2 ingest path stores chunks ≤ `CHUNK_SIZE` (1 MiB) whole;
  revisit when a future ingest format breaks that assumption.
- **Live testnet validation.** The 11/11 local-mirror E2E suite
  exercises the chain-compat surface end-to-end against a
  fresh-genesis mirror, but a live testnet replay against an
  upstream chain release remains a Phase 5+ workstream.

### Resolved since `[Unreleased]` was opened
- **Local-mirror E2E suite** — landed in WS2b; see
  `crates/sum-node/tests/e2e_mirror.rs` and `make e2e-mirror`.
- **CI** — `.github/workflows/ci.yml` shipped earlier and is now
  the canonical PR gate; the new branch protection on `main`
  requires `release-check (linux)` to pass.
