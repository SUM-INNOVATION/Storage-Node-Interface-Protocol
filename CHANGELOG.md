# Changelog

Notable changes to SNIP. The format follows Keep a Changelog
([keepachangelog.com](https://keepachangelog.com/en/1.1.0/)) and
loosely Semantic Versioning. Chain-version compatibility is tracked
in [`docs/reference/chain-compat.md`](docs/reference/chain-compat.md).

## [Unreleased]

### Added
- **AssignmentCoverageV2 epoch fields** (closes #34). Adds four
  `#[serde(default)]` fields (`assignment_epochs`,
  `latest_assignment_epoch`, `reassignment_needed`, `per_epoch`) to
  `AssignmentCoverageV2` and a new `AssignmentEpochCoverageV2` type
  (upstream `sum-chain` issue #62). Pre-#62 chain responses (which
  omit the fields entirely) deserialize cleanly at the default
  values. `AssignmentCoverageV2::resolved_latest_epoch() ->
  Option<u64>` disambiguates legacy responses (empty
  `assignment_epochs`) from post-#62 responses. Aggregate
  `can_activate_now` and `missing_indices` semantics are preserved
  per upstream (verified against
  `sum-chain/crates/state/src/storage_metadata.rs:2181-2200`), so
  existing consumers (`s4_wait_coverage`,
  `collect_missing_indices`) require no behavior change.
  Consumer response to `reassignment_needed` and client-side
  `ReassignChunksV2` remain follow-ups; this change is
  observability + wire-compat only. `#[serde(default)]` cannot
  detect malformed partial metadata — SNIP trusts the upstream
  invariant that `epochs.last() == latest_assignment_epoch` when
  the field is present; this trade-off is documented in the type's
  docstring.

### Fixed
- **Runtime replication factor plumbing** (closes #33). SNIP no
  longer hardcodes `R = 3` in receive-side and V1 code paths. Every
  production runtime consumer receives the live
  `ChainParamsInfo::assignment_replication_factor` via a new
  `sum_node::runtime_params::RuntimeChainParams` struct, cached once
  at process startup (`run_listen`) or operation entry
  (`run_ingest`, `run_ingest_v2`, `run_download`, `run_resume_v2`).
  Production profile hard-fails when `chain_getChainParams` fails
  (no silent fallback to `R = 3`); dev profile falls back to
  `RuntimeChainParams::dev_fallback` with a WARN naming the R value
  in use. `PushValidator` / `AssignmentAttestor` constructed via
  `V2Params::from_runtime`; `MarketSyncWorker`, `DownloadOrchestrator`,
  and `UploadOrchestrator` accept the runtime R directly.
- **Plan-derived upload success semantics** (part of #33). The V1
  upload orchestrator now stores `per_chunk_expected: Vec<u32>`
  computed at planning time from
  `assigned_archives(..., min(configured_r, N))`. `UploadResult::check_success`
  compares per-chunk ACKs to that plan target — bare configured R
  is never re-consulted, so R > N deployments correctly require
  only N ACKs per chunk (matching upstream chain aggregate
  coverage). A new `UploadFailure::NoEligibleTargets` variant
  surfaces the "R = 0 or empty snapshot" state to the CLI mapper.
- **V2 R = 0 preflight before S1** (part of #33). Fresh
  `ingest-v2` refuses to submit `RegisterFilePendingV2` when
  `assignment_replication_factor == 0`; no chain state created,
  no fees spent. Message is truthful (registration refused).
  `resume` under R = 0 reports the existing Pending lifecycle
  and suggests abandon / external `ReassignChunksV2` — never
  claims registration was prevented (which would be wrong because
  S1 already finalized on the prior invocation).
- **Constant rename with deprecated alias** (part of #33).
  `sum_types::storage::REPLICATION_FACTOR` renamed to
  `DEFAULT_REPLICATION_FACTOR`; the old name kept as a
  `#[deprecated(note = "...")]` alias for one release cycle to
  avoid breaking downstream compilations. **No production code
  path consumes either name.** The `since` field is intentionally
  omitted so the release-cutting PR can populate it with the
  actual tag. Non-consumption invariant is enforced by the
  release-checklist audit command
  `rg -n '\b(REPLICATION_FACTOR|DEFAULT_REPLICATION_FACTOR)\b' crates -g '*.rs'`
  — the only allowed matches are the constant definition site in
  `crates/sum-types/src/storage.rs`,
  `RuntimeChainParams::dev_fallback`, and test code under
  `#[cfg(test)]` or `tests/`.
- **NodeRecord eligibility contract enforcement** (closes #32).
  SNIP now defensively filters `storage_getActiveNodes` /
  `storage_getActiveNodesAtHeight` responses by the
  eligibility contract `role == "ArchiveNode" && status ==
  "Active"` at every assignment consumer. Adds
  `NodeRecordInfo::is_active_archive`,
  `NodeRecordInfo::known_status`, `KnownNodeStatus`, and the
  shared `filter_active_archives` helper in
  `crates/sum-types/src/rpc_types.rs`; wire representation
  stays `String` for both fields so future unknown chain
  variants (e.g. after the archive-unbonding gate #20 activates
  at height 8,900,000) deserialize without a SNIP release,
  remain observable via the raw string, and are treated as
  ineligible. Applied at V1 upload/download, MarketSync + GC,
  V2 ingest / resume snapshot, V2 push-validator admission
  cache, V2 public + private routing, V2 attest trigger, and
  the operator `--require-archives` gate. The consumer audit is
  documented in `docs/architecture/chain-integration.md`.

### Changed (docs — audit)
- **Documentation restructure and audit** (closes #29, #31). The
  `docs/` tree is reorganised into subject-area subdirectories
  (`getting-started/`, `protocol/`, `architecture/`, `operator/`,
  `client/`, `reference/`, `security/`, `compatibility/`,
  `release/`, `status/`, `roadmap/`, `archive/`), with historical
  planning documents moved to `docs/archive/` under an ARCHIVED
  preamble and normative material either in place (edited) or
  extracted into new topic pages. The root README is now a concise
  landing page pointing into the new tree.
- **V2 Proof-of-Retrievability targeting is documented correctly.**
  The README previously described V2 PoR challenge targeting as
  unimplemented, citing a stale `sum-chain` source range. That
  claim is removed. The new canonical page
  [`docs/protocol/proof-of-retrievability.md`](docs/protocol/proof-of-retrievability.md)
  distinguishes shipped V2 assignment-aware challenge targeting
  (upstream `sum-chain` issue #97, gated by `assignment_targeting`
  on `chain_getChainParams`) from the still-planned bounded
  coverage scheduler (upstream `sum-chain` issue #81,
  design-only). The gate value on any given deployment is described
  as "verify at runtime" — SNIP documentation does not restate the
  current mainnet value as a constant. Cross-repository chain
  references cite symbols rather than line ranges to degrade more
  gracefully.
- **Chain-ID safety callouts.** Every mainnet-directed example in
  [`docs/operator/mainnet-bringup.md`](docs/operator/mainnet-bringup.md)
  and [`docs/operator/runbook.md`](docs/operator/runbook.md) now
  passes `--chain-id 1` explicitly on the write commands that
  consume the CLI-supplied `chain_id` directly
  (`register-encryption-key`, `share`, `revoke`, `update-access`,
  `abandon`, and `listen`'s background `AssignmentAttestor`).
  [`docs/reference/config-flags.md`](docs/reference/config-flags.md)
  "Chain ID safety" enumerates which code paths consume the CLI
  value vs. read `chain_id` from RPC.
- **Feature-status matrix.** New
  [`docs/status/implementation-status.md`](docs/status/implementation-status.md)
  lists every user-visible feature with shipped/planned status and
  entry-point references; new
  [`docs/roadmap/roadmap.md`](docs/roadmap/roadmap.md) collects
  forward-looking items including a listener-side runtime fix that
  should read `chain_id` from RPC in every V2 tx-signing path.
- **Terminology.** SNIP uses "Proof of Retrievability" (PoR)
  consistently. Upstream issue titles that use "Proof-of-Retention"
  are preserved verbatim when cited, with a one-time terminology
  note at the top of the PoR page.
- **CLI reference.** [`docs/reference/cli.md`](docs/reference/cli.md)
  is now the sole source of truth for `sum-node` and `e2e-helper`
  subcommands and flags. Adds previously-undocumented
  `--download-timeout-secs` and `--manifest-push-timeout-secs`;
  fixes the V1 `ingest` row's `--client` branch behavior.
- Every operator-runbook `e2e-helper register-node` example now
  includes `--allow-live-chain-write`.
- Sum-chain-repo path references in
  [`docs/reference/chain-compat.md`](docs/reference/chain-compat.md)
  and [`docs/release/release-checklist.md`](docs/release/release-checklist.md)
  are prefixed `sum-chain:` (or accompanied by an inline
  "in the chain repo" clarifier) to disambiguate from local paths.
- Root-level `docs/*.md` compatibility stubs left at every moved
  authoritative path so external links continue to resolve for one
  release cycle.
- Compressed workspace `Cargo.toml` `repository` URL from
  `SUM-Storage-Node-Protocol` to `Storage-Node-Interface-Protocol`
  (actual GitHub name). All five workspace crates inherit and
  now advertise the correct URL.

### Added (docs — audit)
- `scripts/check-docs-links.sh`. Mechanical link checker: every
  relative Markdown link inside `docs/**/*.md`, `README.md`, and
  `CHANGELOG.md` must resolve to an existing file. Ignores
  external URLs, mailto, pure fragment links, and code-formatted
  cross-repository paths prefixed `sum-chain:`. Reports source
  file, line, and unresolved target on failure. Wired into
  `make release-check`.
- `scripts/check-cli-doc.sh`. Verifies every clap flag on
  `sum-node` and `e2e-helper` has a row under its subcommand
  section in `docs/reference/cli.md`. Wired into
  `make release-check`.

### Added
- Prebuilt Linux x86_64 binary tarball published as a GitHub
  Release asset. Tarball layout:
  `snip-vX.Y.Z-linux-x86_64/{bin/{sum-node,e2e-helper},README.md,CHANGELOG.md,LICENSE-MIT,LICENSE-APACHE}`.
  Sibling `SHA256SUMS` asset published alongside. Linux x86_64
  is the only platform with prebuilts in `v0.4.x`; every other
  cell builds from source per `docs/PLATFORM-SUPPORT.md`.
- `.github/workflows/release.yml`. Tag-triggered (`v*`) workflow
  that builds release binaries from the tag checkout, validates
  the tag format (`vX.Y.Z` or `vX.Y.Z-rcN`), runs
  `make release-check`, packages the tarball, computes
  `SHA256SUMS`, and uploads tarball + checksums +
  `scripts/install.sh` to a **draft** GitHub Release. The
  workflow never auto-publishes. rc tags additionally carry the
  GitHub `--prerelease` flag.
- `scripts/install.sh`. Idempotent installer that pins the
  release version (no implicit "latest"), refuses to run on
  anything other than Linux x86_64, downloads tarball +
  checksums into `mktemp -d`, verifies the SHA256, and installs
  `sum-node` + `e2e-helper` into `$HOME/.local/bin` by default
  (or `$PREFIX/bin` if `--prefix` is supplied). The installer
  does NOT invoke `sudo`; system installs run the curl-pipe
  under user-supplied sudo. Released as a versioned release
  asset alongside the tarball.

### Added (docs)
- `docs/INSTALL.md`. Documents both the manual-verify install
  path (download → check SHA256 → extract → move binaries) and
  the curl-pipe convenience script. Manual-verify is documented
  first; curl-pipe references the release-asset URL (never
  `main`). Includes the build-from-source fallback for cells
  without a prebuilt and the v0.4.x security-model notes (no
  code signing, no notarization, no signed checksums — these
  are deferred packaging items).
- `docs/PLATFORM-SUPPORT.md`. Initial cross-platform support
  matrix establishing the `v0.4.x` contract: client mode runs on
  any Linux-compatible environment (Linux native, macOS native,
  Windows via WSL2, ChromeOS via Crostini); archive mode is
  Linux-first with macOS as experimental pending operator
  long-run validation; Windows and ChromeOS archive modes are
  not supported. Records per-environment setup recipes (WSL2
  install + Crostini quickstart), promotion criteria for
  Experimental → Supported, items not planned for `v0.4.x`
  (native Windows tooling, launchd templates, signed packaging),
  and permanent non-goals (ChromeOS native, iOS / Android
  client).

### Changed (docs)
- `README.md` adds a compact platform support matrix near the
  top with a pointer to `docs/PLATFORM-SUPPORT.md` for the full
  rationale, plus a top-level `## Install` section pointing at
  `docs/INSTALL.md` and showing the curl-pipe convenience line.
- `docs/PLATFORM-SUPPORT.md` Linux x86_64 / aarch64 section
  records that x86_64 has a prebuilt tarball; aarch64 remains
  build-from-source in `v0.4.x` and is a `v0.4.1+` candidate
  pending operator validation. "Not planned for v0.4.x" section
  splits the old "Pre-built binary distribution, code signing,
  notarization" bullet into two: code signing / notarization /
  signed checksums (still deferred to `v0.5.x`) and prebuilts
  beyond Linux x86_64 (aarch64 / macOS arm64 in `v0.4.1+`;
  Homebrew / winget / scoop / deb / rpm in `v0.5.x+`).
- `docs/RELEASE-CHECKLIST.md` § 8 adds a new "8a. Verify the
  prebuilt-binary draft release" subsection covering the
  manual-verify preflight against draft assets via
  `gh release download`, the manual publish step (no
  auto-publish), and a post-publish curl-pipe smoke gate.

## [0.4.0-rc4] - 2026-05-12

Docs-only release candidate. Same binary surface as `v0.4.0-rc1` /
`rc2` / `rc3`. Lands the mainnet bring-up field guide previously
tracked in GitHub issue #20, fixes one stale string in the operator
runbook, corrects the README's client-vs-archive workflow language,
and records the first verified mainnet Public V2 round-trip
artifacts.

### Added (docs)
- `docs/MAINNET-BRINGUP.md`. Standalone field guide for mainnet
  archive bring-up. Covers host prerequisites (including the
  recommended separate owner/client identity), install at the
  current release-candidate tag, read-only smoke + balance +
  active-archive preflight, per-archive registration, the
  three-archive bootstrap gate, the canonical throwaway Public V2
  round-trip (success + below-quorum Pending paths), Private file
  readiness, operational notes, the four-gate promotion criteria
  from `RELEASE-CHECKLIST.md` § 7, and mainnet-specific failure
  triage.

### Changed (docs)
- `docs/OPERATOR-RUNBOOK.md` § Mainnet bring-up step 3.1 smoke
  sample updated to match the actual `format_smoke_human` output
  (`ENABLED_FROM_HEIGHT`, two-line `[1/2]` + `[2/2]` check format).
  Adds a pointer to `MAINNET-BRINGUP.md` at the end of the section.
- `docs/RELEASE-CHECKLIST.md` § 7 adds a pointer to
  `MAINNET-BRINGUP.md` for step-by-step gate-satisfaction
  instructions.
- `README.md` CLI Reference workflow language corrected to clearly
  distinguish two SNIP operator roles: client (file user, pays
  fees, no stake, no `listen`) and archive operator (registers,
  stakes, runs `listen`). Client examples updated to use V2
  (`ingest-v2 --visibility public`) since V2 is the chain-canonical
  path on mainnet. Archive-operator examples now show
  `register-encryption-key` + `register-node --stake 1000000000`
  before `listen --profile production`. Cross-links to
  `docs/MAINNET-BRINGUP.md` and `docs/CHAIN-COMPAT.md` added.

### Verified (informational; no code changes)
First mainnet Public V2 round-trip executed against `v0.4.0-rc3`
with a three-archive fleet:

- merkle_root: `0x617538f7ecd0d36f65e82802227d4fb4081af7ec5ed5d8ea2fb8d9ae07f53ba0`
- RegisterFilePendingV2 tx: `0x8ba095ac329170ad610ad714f29566cfb446bc4f369dbb2695ca731057138117` (height 5,647,745)
- ActivateFileV2 tx: `0x6d2cabd10cfef15cdfbaf23983d37ffa2411432dfbb48c835c802d805e88538f` (height 5,647,755)
- file size / SHA-256: 4,096 B / `036536268a41f495e13ca8d6f52b9f3c9118280848339f2b6eee266cd67ee812`
- archive fleet (L1 base58): `EbhtpDt6DUgviJfPcQvQoiett8GtS5s71`, `79oSi1TAfTjv76pPQhhgEpgqJFiBDKcdZ`, `8c7viqt3gKjwgEw9BbNVmoJDxcqSvjToe`
- owner (L1 base58): `2udfFGPbxMnfEJ8sdrK9uvnJ1zmPbwL69`
- outcome: `lifecycle: Active` on single-pass ingest; recovered file
  byte-identical to source.

All four `RELEASE-CHECKLIST.md` § 7 pre-final-release gates green
against the rc3 binary: fresh-machine local-mirror E2E (11/11),
mainnet read-only smoke, ≥ 3 archives registered + listening,
single-pass byte-identical Public V2 round-trip.

### Constraints honored
- No code changes; no tests changed.
- `v0.4.0-rc1`, `rc2`, `rc3` tags unchanged. `v0.4.0-rc4` is a
  separate annotated tag at the new main tip.
- No private keys, validator hostnames, or chain-team-private
  facts. No validator binary SHA reproducibility claim — chain
  commit remains the canonical reference.
- Stable stdout claim for ingest/resume limited to `merkle_root:`
  and `lifecycle:` lines; tx hashes documented as best-effort log
  output, not as pinned stdout.

## [0.4.0-rc3] - 2026-05-08

Operator-safety follow-up to `v0.4.0-rc2`. Adds a single
`e2e-helper` subcommand that lets bring-up scripts gate the first
mainnet ingest on the chain plan's `assignment_replication_factor = 3`
quorum. No production `sum-node` logic is touched.

### Added (e2e-helper)
- **`e2e-helper active-nodes-at-height`.** Read-only chain
  snapshot: resolves the finalized head (or an explicit u64),
  calls `storage_getActiveNodesAtHeight`, and prints the role ×
  status breakdown. Optional `--require-archives N` converts the
  helper into a script-friendly pre-flight gate — exit `2` if
  the ArchiveNode/Active count is below `N`, exit `0` if met or
  no requirement, exit `1` on RPC/wire failure. JSON output via
  `--json`.

### Changed (docs)
- `docs/OPERATOR-RUNBOOK.md` "Mainnet bring-up" hard pre-flight
  replaces the inline `curl + jq` snippet with the new helper.
- `docs/RELEASE-CHECKLIST.md` § 7 "Pre-final-release gates" likewise
  uses the helper for the `≥ 3 archives` check.

### Verification
- 7 new unit tests in `e2e_helper.rs` pin the role × status tally,
  the exit-code decision (no requirement, threshold met exactly,
  below threshold), and the human-output formatting (key:value
  shape + empty-snapshot hint).
- `make release-check` clean.
- Live smoke against the local mirror exercises both branches
  (`exit 0` without threshold, `exit 2` when threshold unmet).

### Constraints honored
- Production `sum-node` logic untouched. WS2 tests untouched.
- No new mainnet writes; helper is read-only.
- `v0.4.0-rc1` and `v0.4.0-rc2` tags unchanged. `v0.4.0-rc3` is a
  separate annotated tag at the new main tip.

## [0.4.0-rc2] - 2026-05-07

Docs-only release candidate. No code changes; same binary as
`v0.4.0-rc1`. Promotes the operator-facing docs to mainnet-ready
state by recording the chain team's confirmed-public mainnet pin
and the bring-up flow that goes with it.

### Added (docs)
- **`docs/CHAIN-COMPAT.md` "Mainnet pin / deployed chain"
  section.** Records the publicly observable mainnet facts at the
  pinned chain commit — `chain_id = 1`, public RPC, genesis SHA-256,
  `v2_enabled_from_height = 5200000`, `finality_depth = 6`,
  `block_time_ms = 3000`. Documents the canonical TxPayload shapes
  SNIP submits (V1 archive registration at tag 17, V2 encryption-key
  registration at tag 19) and the JSON-RPC method set SNIP
  intentionally uses, with explicit notes that aliases like
  `sum_sendRawTransaction` are NOT used by SNIP. New
  "Mainnet vs local-mirror" comparison table catches mis-configured
  RPC URLs.
- **`docs/OPERATOR-RUNBOOK.md` "Mainnet bring-up" section.**
  Step-by-step flow: read-only smoke → balance check → smallest
  write canary (`register-encryption-key`) → archive registration
  (`register-node`) → verify node record → start `listen` with
  `--profile production`. Includes a hard pre-flight that bars the
  first ingest until `storage_getActiveNodesAtHeight` shows ≥ 3
  active archives — single-archive registration is structurally
  insufficient under chain plan `R = 3`. Operational shape options
  documented: coordinated ramp (3 independent operators) or
  single-org bootstrap (3 identities on 3 hosts).
- **`docs/RELEASE-CHECKLIST.md` "Pre-final-release gates" section
  (new § 7).** Codifies the four gates an `rcN` must clear before
  promoting to a final `vX.Y.Z` tag: fresh-machine local-mirror
  E2E reproduction, mainnet read-only smoke, ≥ 3 mainnet archive
  nodes registered + listening, and a first Public V2 ingest +
  download round-trip against mainnet using a throwaway file.

### Constraints honored
- Validator binary SHA is intentionally NOT documented as a
  reproducibility requirement — release builds are not
  byte-reproducible across hosts (toolchain, build flags, system
  libraries vary). The chain commit is the canonical reference.
- Validator hostnames, RPC admin endpoints, and any other chain-
  team-private facts are excluded. Only operator-visible mainnet
  facts the chain team has confirmed for public docs are recorded.
- No code changes. Same binary surface as `v0.4.0-rc1`. The
  release-candidate jump from rc1 to rc2 reflects docs-readiness,
  not a behavioral change.

### Verification
- `make release-check` clean (fmt + lint + tests + release build).
- `v0.4.0-rc1` tag is unchanged; this release-candidate is a
  separate annotated tag at the new main tip.

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
