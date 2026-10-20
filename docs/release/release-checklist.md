# Release checklist

Run these checks in order before tagging and shipping a SNIP release.
Anything that fails blocks the release; anything that warns goes in
[`CHANGELOG.md`](../../CHANGELOG.md) under "Known issues."

> Examples below use placeholders for environment-specific values:
> `<internal-chain-release-tag>`, `<chain-rpc-host>`,
> `<known-v2-root>`, `<known-tx-hash>`. Substitute for your release
> environment. Real RPC URLs, validator hostnames, funded addresses,
> and internal chain commits MUST NOT appear in this file.

## 0. CI gate (one-time setup, then automated)

Every PR runs `make release-check` on `ubuntu-24.04` via
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml). The job
is named exactly `release-check (linux)` so a GitHub branch-
protection rule can require it by stable name.

One-time setup:

- [ ] In GitHub: **Settings → Branches → main** (Branch protection
      rule). Require status check `release-check (linux)` to pass
      before merging. Enable "Require branches to be up to date
      before merging" so the gate runs against the merge result,
      not a stale base.
- [ ] Confirm a freshly opened PR shows the `release-check (linux)`
      check as required (not "expected") — the rule only applies
      after at least one run completes on `main`.

What CI runs:

- `make release-check` — `fmt` + `lint-strict` + `test` + `build` +
  `audit-logs`. Same chain operators run locally.

What CI does NOT run:

- **Live-chain smoke** (`make smoke RPC=…`) — manual only. CI has
  no chain RPC URL and no chain secrets. Run before tagging
  (see § 4 below).
- Release artifact builds, coverage, macOS matrix — explicitly out
  of scope for the merge gate. Add later in separate workflows
  once the linux gate is stable in production.

## 1. Workspace hygiene

- [ ] `git status` — no uncommitted changes, no untracked files
      except `target/` and editor scratch.
- [ ] On the release branch (typically `feature/...` merged into
      `main`).
- [ ] Workspace lives on a non-cloud-synced filesystem.

## 2. Local checks

```bash
make release-check
```

Equivalent to:

- `cargo fmt --check` — code is formatted.
- `cargo clippy --workspace --all-targets -- -D warnings` —
  zero clippy warnings under the workspace lint policy.
- `cargo test --workspace` — all tests green.
- `cargo build --release -p sum-node` — release binary builds.

If `lint-strict` fails on a warning class that can't be fixed in
this release, do NOT add a blanket `#[allow]`. Document the
exemption in the commit message and in [`Cargo.toml`](../../Cargo.toml)
`[workspace.lints.clippy]` with rationale.

## 3. Chain compatibility

- [ ] [`CHAIN-COMPAT.md`](../reference/chain-compat.md) "Pinned chain version"
      row holds an actual SHA. The SHA MUST be from a chain
      history that contains no committed signing material; if
      chain ops rewrote history to scrub secrets, confirm the
      rewrite landed before pinning.
- [ ] The fixture tests pass without modification on the pinned
      SHA's regenerated mirror types:
      ```bash
      cargo test -p sum-node tx_builder rpc_client
      cargo test -p sum-types
      ```
      Any diff in fixture bytes is a wire-format change and MUST be
      a separate commit referencing the new chain SHA (chain-team
      coordination required).
- [ ] Chain team has reported a green run of their own V2 wire-
      fixture suite on the pinned SHA (e.g.
      `cargo test -p sumchain-primitives --test v2_wire_fixtures`).
      Both sides green means the V2 wire shape is confirmed-stable
      on this commit.
- [ ] V2-gate semantics tests still pass (regression guard for
      `Some(0)` ≠ `None`):
      ```bash
      cargo test -p sum-types chain_params_v2_enabled_from_height
      ```
- [ ] No stale `REPLICATION_FACTOR` constant references, no
      hard-coded `3` in eligibility / assignment call sites, no
      unfiltered raw `NodeRecordInfo` slices reaching an assignment
      consumer. Run the audit:
      ```bash
      rg -n --glob '!target' --glob '!docs' --glob '!CHANGELOG.md' \
        --glob '!tests/**' \
        -e '\bREPLICATION_FACTOR\b' \
        -e 'assignment_replication_factor\s*:\s*3\b' \
        -e 'assigned_archives\([^)]*,\s*3\s*\)' \
        -e 'compute_chunk_assignment\([^)]*,\s*3\s*\)' \
        -e 'chunks_for_archive_v2\([^)]*,\s*3\s*[,)]'
      ```
      Allowlist (rg hits below are load-bearing and safe):
        * `crates/sum-types/src/storage.rs` — `pub const DEFAULT_REPLICATION_FACTOR`
          plus the single-line `#[deprecated]` alias
          `REPLICATION_FACTOR` (kept to avoid breaking downstream
          constants; new call sites use `DEFAULT_REPLICATION_FACTOR`).
        * `crates/sum-node/src/runtime_params.rs` — one reference to
          the deprecated alias inside a `#[allow(deprecated)]` test
          that verifies the alias still resolves to the same value.
        * `crates/sum-store/src/assignment*.rs` — inline unit-test
          modules (`#[cfg(test)] mod tests`) drive the pure
          assignment kernel with deterministic replication factors;
          these are not production callers.
        * `crates/sum-node/src/*/tests` — deliberate boundary-test
          fixtures (`assignment_replication_factor: 5` / `: 2` /
          `: 7`, plus `assigned_archives(..., 3)` /
          `chunks_for_archive_v2(..., 3, ...)` for R>N and R<N
          coverage).
      Any hit outside the allowlist is a regression: replace with
      the runtime `R` threaded from
      `RuntimeChainParams::assignment_replication_factor` before
      merging.
- [ ] The four post-#62 `AssignmentCoverageV2` fields deserialize
      from a legacy (pre-#62) chain response — no panics, defaults
      to the empty-epoch shape:
      ```bash
      cargo test -p sum-types resolved_latest_epoch
      cargo test -p sum-types coverage_v2_forward_compat
      ```
- [ ] R=0 preflight and pinned-empty-snapshot recovery paths
      still fire — regression coverage lives at
      `crates/sum-node/src/ingest_v2.rs`:
      ```bash
      cargo test -p sum-node v2_ingest_r_0 v2_resume_r_0 v2_ingest_post_s1_empty v2_resume_reuses
      ```

## 4. Live-chain smoke (manual, read-only)

Live-chain smoke is **read-only by default**. Destructive /
write-path lifecycle tests require explicit operator approval and
double opt-in (env + flag) — never run them on a value-bearing
chain without that approval.

```bash
make smoke RPC=https://<chain-rpc-host>
```

Or via `e2e_helper` directly:

```bash
cargo run --release -p sum-node --bin e2e-helper -- \
    smoke --rpc-url https://<chain-rpc-host>
```

Asserts read-only:

- `chain_getChainParams` decodes; reports V2 state per
  `v2_enabled_from_height`:
  - `Some(0)` → "V2 enabled from genesis."
  - `Some(N)` for N>0 → "V2 enabled at finalized height N."
  - `None` → "V2 disabled on this chain."
- `chain_getBlockHeight(["finalized"])` returns finality.
- Optional, when env vars are set:
  - `account_getEncryptionPublicKey` for a known address.
  - `storage_getFileInfoV2` for `SNIP_SMOKE_KNOWN_ROOT=<known-v2-root>`.
  - `chain_getTransactionStatus` for `SNIP_SMOKE_KNOWN_TX=<known-tx-hash>`.

## 5. Local-mirror full E2E

**First path: GitHub-hosted.** The `release local-mirror E2E`
workflow
([`.github/workflows/release-local-mirror-e2e.yml`](../../.github/workflows/release-local-mirror-e2e.yml))
runs on every pull request whose head branch starts with `release/`,
on a fresh GitHub-hosted runner. It runs
[`scripts/local-mirror-e2e.sh`](../../scripts/local-mirror-e2e.sh),
which:

1. checks out the public chain repository at the workflow's
   `SUMCHAIN_COMMIT` and refuses any other commit;
2. provisions the chain's synthetic five-validator devnet
   (`setup-local-testnet --mode topology --validator-count 5
   --output-dir devnet`: chain id 1337, V2 from genesis, throwaway
   keys generated on the runner, never committed, printed or
   uploaded);
3. builds the images from that commit and starts the chain's
   `deploy/snip-local-mirror.yaml`;
4. writes SNIP's six role seed files from the devnet's funded roles
   into a temporary directory outside both repositories;
5. runs `make smoke RPC=http://localhost:8545 SMOKE_ARGS=--require-v2`,
   then `make e2e-mirror`;
6. tears the devnet down and deletes its keys, on success or failure.

It uses a read-only token and no secrets, and it is not a required
check. A failure is a hard release blocker. Record the run URL and
the chain commit with the release evidence.

The reassignment and re-fetch scenario
(`reassignment_refetch_restores_public_coverage`) does not run in the
release workflow: the synthetic devnet keeps
`archive_reassignment_enabled_from_height` and
`archive_unbonding_enabled_from_height` unset, so it skips unless
`SNIP_E2E_REASSIGNMENT=1` is set. With that set, `scripts/local-mirror-e2e.sh`
opens exactly those two gates from genesis in this run's throwaway devnet
genesis (PoR assignment targeting and the PoR scheduler stay unset), runs the
shared suite with the opt-in unset, and then runs this scenario alone. It
covers: an archive leaving by its own BeginUnstake, the owner's reassignment,
re-fetch by a fourth throwaway archive with corrupt copies refused and the
next source used, a restart while one chunk is missing, latest-epoch
attestation with `reassignment_needed` cleared, and a Private file that is
never fetched.

`SUMCHAIN_COMMIT` is the chain commit the end-to-end run uses. It is
not the compatibility pin in
[`chain-compat.md`](../reference/chain-compat.md): at that pin the
chain's mirror preset mounts files the repository does not track, so
it cannot be brought up from the repository alone.

**Fallback: a Docker-capable Linux host the owner names.** The same
script runs there unchanged; no other setup is assumed:

```bash
git clone https://github.com/SUM-INNOVATION/sum-chain.git /tmp/sum-chain
git -C /tmp/sum-chain checkout <SUMCHAIN_COMMIT from the workflow>
SUMCHAIN_DIR=/tmp/sum-chain SUMCHAIN_COMMIT=<same sha> \
    scripts/local-mirror-e2e.sh
```

The suite (`crates/sum-node/tests/e2e_mirror.rs`) covers: mirror
health; node and encryption-key registration; Public ingest and
download; Private owner-only; Private shared; share, revoke and
update-access across the finality boundary; and resuming a Pending
file to Active. Archive-restart recovery is not covered yet.

## 6. Privacy audit

- [ ] `scripts/audit-logs.sh` clean (lands in WS4). The script greps
      for forbidden tokens (`k_file`, `seed`, `x25519_secret`,
      `bundle_hex`, `encrypted_key_bundle`, raw `plaintext`) inside
      log-macro format strings.
- [ ] Each row in [`PRIVACY-AUDIT.md`](../security/privacy-audit.md) still has a
      pinning test reference; new threats added since last release
      have new rows.

## 7. Pre-final-release gates (release-candidate → final)

A release-candidate (`vX.Y.Z-rcN`) is **not** final production until
every gate below clears against that tag's commit. These gates exist
to catch hidden local-environment assumptions and chain-state
prerequisites that only surface on a fresh machine or against live
mainnet.

Step-by-step operator instructions for satisfying gates 2–4 against
live mainnet are in [`MAINNET-BRINGUP.md`](../operator/mainnet-bringup.md). This
section defines *what* must be true to promote; that guide describes
*how* to make it true.

- [ ] **Fresh-machine local-mirror E2E.** A different operator on a
      different machine clones the repo at the rc tag, generates
      keys via `e2e-helper generate-e2e-keys`, brings up the chain
      mirror per [`OPERATOR-RUNBOOK.md`](../operator/runbook.md), and
      runs the full WS2b suite:

      ```bash
      cargo test -p sum-node --test e2e_mirror -- \
          --ignored --test-threads=1 --nocapture
      # target: 11 passed; 0 failed
      ```

      The fresh-machine reproduction catches path / dependency /
      mirror-bring-up assumptions baked into the original developer's
      environment. A green run on the original machine is necessary
      but not sufficient.

- [ ] **Mainnet read-only smoke.** Against the live mainnet RPC,
      compared with the values in [`CHAIN-COMPAT.md`](../reference/chain-compat.md)
      "Mainnet pin / deployed chain" (the RPC does not report which
      chain commit mainnet runs):

      ```bash
      make smoke RPC=https://rpc.sumchain.io SMOKE_ARGS=--require-v2
      ```

      Confirms `chain_id`, V2 enablement state, and finality
      cadence match the values in CHAIN-COMPAT. A drift here means
      the chain advanced past what the rc was built against — bump
      the chain pin, re-run all gates.

- [ ] **At least 3 archive nodes registered + listening on mainnet.**
      Confirm via the `e2e-helper active-nodes-at-height` gate
      (reads finalized head + the active-nodes snapshot in one
      step; exit 2 when below threshold):

      ```bash
      cargo run --release -p sum-node --bin e2e-helper -- \
          active-nodes-at-height \
          --rpc-url https://rpc.sumchain.io \
          --height finalized \
          --require-archives 3
      # exit 0 ⇒ pre-flight cleared
      ```

      The chain plan's `assignment_replication_factor = 3` makes
      this a hard pre-flight: no V2 ingest can activate without
      three resolvable archive peers. Below 3, ingest registers on
      chain but stalls in `Pending` indefinitely.

- [ ] **First mainnet Public V2 ingest + download round-trip.** Use
      a throwaway file (operator personal data, NOT customer data)
      and verify the round-trip succeeds through the full
      `RegisterFilePendingV2` → push chunks → push manifest →
      `ActivateFileV2` → download → byte-identical reassembly path.
      This is the canonical "first real bytes" milestone; it is
      out-of-band today (no automated mainnet test). Record the
      tx hash and the merkle root in the release notes.

Only after **all four** gates clear does an `rcN` advance to a
final `vX.Y.Z` tag.

## 8. Tag and ship

- [ ] Bump version in [`Cargo.toml`](../../Cargo.toml)
      `[workspace.package].version`.
- [ ] Update [`CHANGELOG.md`](../../CHANGELOG.md):
  - Move "Unreleased" entries under a new
    `## [vX.Y.Z] — YYYY-MM-DD`.
  - Restate Phase / chain-version compatibility against
    `<internal-chain-release-tag>`.
- [ ] `git commit -m "chore: release vX.Y.Z"`.
- [ ] `git tag -a vX.Y.Z -m "vX.Y.Z"`.
- [ ] `git push && git push --tags`.

### 8a. Verify the prebuilt-binary draft release

Pushing the tag triggers
[`.github/workflows/release.yml`](../../.github/workflows/release.yml),
which builds the Linux x86_64 release binaries, packages them as
`snip-vX.Y.Z-linux-x86_64.tar.gz`, computes `SHA256SUMS`, and
uploads those plus `scripts/install.sh` to a **draft** GitHub
Release. The workflow never auto-publishes.

**First path: GitHub-hosted verification.** The same workflow's
`verify-draft` job then runs on two fresh GitHub-hosted runners that
did not build the assets, `ubuntu-22.04` and `ubuntu-24.04`. Each
one:

- confirms the release is a draft for this tag, flagged pre-release
  exactly when the tag is an rc, with exactly three assets:
  `snip-vX.Y.Z-linux-x86_64.tar.gz`, `SHA256SUMS`, `install.sh`;
- downloads them with `gh release download` and runs
  [`scripts/verify-release-assets.sh`](../../scripts/verify-release-assets.sh),
  which checks:
  - the asset set is exactly those three regular files;
  - `SHA256SUMS` has one entry, for the tarball, and it matches;
  - every tarball entry is confined to `snip-vX.Y.Z-linux-x86_64/`,
    with no `..` component and no link or special file;
  - the layout is exactly `bin/sum-node`, `bin/e2e-helper`,
    `README.md`, `CHANGELOG.md`, `LICENSE-MIT` and `LICENSE-APACHE`,
    with both binaries executable;
  - the bundled `CHANGELOG.md` has a `## [X.Y.Z]` section;
  - `bin/sum-node --version` prints exactly `sum-node X.Y.Z` on that
    runner;
  - the `install.sh` asset is byte-identical to the tag's
    `scripts/install.sh`.

`make release-check` self-tests the verifier against a synthetic good
release and 16 broken ones
([`scripts/test-verify-release-assets.sh`](../../scripts/test-verify-release-assets.sh)).

- [ ] Wait for the `release` workflow on the tag to finish green,
      including both `verify draft assets` jobs. Keep the run URL as
      evidence.
- [ ] Open the draft release in the GitHub UI and confirm the asset
      list is the one the job checked.

**Fallback: a clean host.** If the GitHub-hosted job cannot run, or
as an additional check, verify by hand on a clean Linux x86_64 host
(or an Ubuntu 22.04 VM / container). From a checkout of the tag:

```bash
# Authenticated download from the draft release.
# Draft assets are NOT reachable via the unauthenticated
# releases URL; use `gh release download` from a logged-in
# gh CLI for the preflight.
mkdir assets
gh release download vX.Y.Z \
    --repo SUM-INNOVATION/Storage-Node-Interface-Protocol \
    --dir assets
scripts/verify-release-assets.sh assets vX.Y.Z scripts/install.sh
```

Without a checkout, the minimal manual steps are:

```bash
gh release download vX.Y.Z \
    --repo SUM-INNOVATION/Storage-Node-Interface-Protocol \
    --pattern 'snip-*-linux-x86_64.tar.gz' \
    --pattern 'SHA256SUMS'

sha256sum --check --ignore-missing SHA256SUMS
tar xzf snip-vX.Y.Z-linux-x86_64.tar.gz
./snip-vX.Y.Z-linux-x86_64/bin/sum-node --version
```

Either way this confirms tarball integrity, layout, and that the
binary runs on a clean host. After publishing the release, the
curl-pipe install line from the README/INSTALL becomes reachable;
that is the next gate.

- [ ] Replace the auto-generated draft body with the matching
      `CHANGELOG.md` section + the install command.
- [ ] Manually publish the draft. For rc tags, keep the
      "pre-release" flag checked.
- [ ] After publishing, run the post-publish install smoke.
      **First path:** dispatch
      [`.github/workflows/release-install-smoke.yml`](../../.github/workflows/release-install-smoke.yml)
      with the tag. It runs the line below on fresh `ubuntu-22.04`
      and `ubuntu-24.04` runners and checks `sum-node --version`.
      It runs only on manual dispatch and reads only the public
      release URLs. **Fallback:** run the line on a clean host:

      ```bash
      curl -fsSL https://github.com/SUM-INNOVATION/Storage-Node-Interface-Protocol/releases/download/vX.Y.Z/install.sh \
          | sh -s -- --version vX.Y.Z
      sum-node --version
      ```

      This is the line copy-pasted into the README; if it does
      not work after publish, no user can install vX.Y.Z.

## 9. Post-release

- [ ] Watch the first-deploy logs for any new warning/error patterns.
- [ ] Verify operator runbook still describes the shipped binary
      (CLI flag drift is the most common doc rot).
- [ ] If any fixture changed: ping chain team to confirm their CI
      sees the same bytes against the same internal release tag.
