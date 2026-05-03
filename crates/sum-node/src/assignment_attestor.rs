//! V2 assignment attestor (chain plan v3.2 §3.6 sender-side).
//!
//! Once a SNIP archive holds chunks for a file the chain assigned to it,
//! it must declare those chunks via `AcceptAssignmentV2`. The chain
//! OR-merges the per-`(file, archive)` bitmap; multiple txs with
//! disjoint or overlapping `chunk_indices` slices are equivalent in
//! effect. The popcount of the bitmap (across all `currently_active`
//! archives) is what the owner watches via `storage_getAssignmentCoverageV2`
//! to know when `ActivateFileV2` is admissible.
//!
//! ## What the attestor does
//!
//! 1. Compute `my_assignment = chunks_for_archive_v2(root, …, my_addr)`
//!    — the deterministic V2 set of chunk indices the chain expects us
//!    to attest. Same algorithm the chain runs in
//!    `storage_getAssignmentCoverageV2`.
//! 2. Intersect with `held` — the chunk indices we actually have on
//!    disk (passed in from the local store).
//! 3. If `attest_set = my_assignment ∩ held` is empty, **no-op**: there
//!    is nothing to attest, and submitting an empty
//!    `AcceptAssignmentV2` would burn fee for zero coverage.
//! 4. Otherwise batch the sorted `attest_set` into chunks of
//!    `params.max_chunk_indices_per_tx` (default 65,536) and submit
//!    one `AcceptAssignmentV2` tx per batch, waiting for `Finalized`
//!    between batches. Nonce is monotonic from `starting_nonce`.
//!
//! ## Failure semantics
//!
//! On the first batch that errors at submit (RPC) or hits a terminal
//! `Failed`/`Dropped`/`Timeout` from [`wait_for_finalized`], the
//! attestor STOPS and returns a partial [`AttestSummary`]: every
//! finalized batch up to that point + the failing batch's error.
//! Earlier successful batches are already on chain (OR-merge means
//! their bits are already counted toward coverage), so an external
//! `resume` flow can read `storage_getAssignmentCoverageV2` to
//! discover what still needs attestation and call the attestor again
//! with the remaining `held` slice.
//!
//! ## Why intersection, not "everything held"
//!
//! An archive may have downloaded a chunk it isn't assigned (mistaken
//! push from the owner, leftover from a rotation). Attesting an
//! un-assigned chunk on chain is a wasteful submission: the chain's
//! `accept_assignment_v2` validator rejects indices outside the
//! deterministic set as a validity failure (chain plan v3.2 §3.6),
//! so the whole tx fails. We pre-filter with the same algorithm the
//! chain runs to avoid the wasted submission.
//!
//! ## Why per-batch finality, not fire-and-forget
//!
//! Sequential nonce + per-batch finality keeps the batches sequenced:
//! if batch N's nonce is `k`, batch N+1's nonce is `k+1` and the
//! mempool will reject `k+1` if `k` hasn't been included. Waiting for
//! `Finalized` after each batch also lets us bail early on a `Failed`
//! status (e.g. signature error on batch N would also make batch N+1
//! invalid because `k+1` already presupposes `k` was accepted).

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sum_store::assignment_v2::chunks_for_archive_v2;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::push_validator::V2Params;
use crate::rpc_client::L1RpcClient;
use crate::tx_builder::build_accept_assignment_v2_tx;
use crate::tx_wait::{wait_for_finalized, TxStatusSource, TxWaitError};

/// Subset of the L1 RPC the attestor consumes. Composed with
/// [`TxStatusSource`] (from `tx_wait`) so any production impl is
/// automatically a status source — `wait_for_finalized` then accepts
/// `&self` without an adapter.
#[async_trait]
pub trait AttestorRpc: TxStatusSource {
    /// Submit a hex-encoded signed transaction. Returns the chain's tx
    /// hash on success (typically `"0x..."`).
    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String>;
}

#[async_trait]
impl AttestorRpc for L1RpcClient {
    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
        // The chain's `send_raw_transaction` returns a JSON string carrying
        // the tx hash. Anything else is a chain bug — callers downstream
        // (`wait_for_finalized`) need a hash, so reject other shapes here
        // rather than papering over with a debug-printed Value.
        //
        // UFCS to disambiguate from this very trait method.
        let raw = L1RpcClient::send_raw_transaction(self, signed_tx_hex).await?;
        match raw {
            serde_json::Value::String(s) => Ok(s),
            other => Err(anyhow::anyhow!(
                "send_raw_transaction returned non-string result: {other:?}"
            )),
        }
    }
}

/// Inputs for one `attest()` call.
#[derive(Debug, Clone)]
pub struct AttestRequest {
    /// File's Merkle root (raw 32 bytes — same shape `assignment_v2` consumes).
    pub merkle_root: [u8; 32],
    /// Total chunks in the file (chain `chunk_count`). Drives the
    /// `[0, chunk_count)` walk inside `chunks_for_archive_v2`.
    pub chunk_count: u32,
    /// Canonical (sorted, deduped) snapshot at the file's
    /// `assignment_height`. Caller is expected to source this via
    /// `storage_getActiveNodesAtHeight` and decode addresses to
    /// `[u8; 20]` before calling — same preprocessing the push
    /// validator does.
    pub snapshot: Vec<[u8; 20]>,
    /// Set of chunk indices this archive has on disk for `merkle_root`.
    /// Pre-intersected with the file's chunk_count is fine but not
    /// required (the attestor does its own intersection with the
    /// deterministic V2 assignment).
    pub held: BTreeSet<u32>,
    /// Starting nonce. Each batch consumes one (`+1` per batch).
    pub starting_nonce: u64,
    /// How often to poll `chain_getTransactionStatus` per batch.
    pub poll_interval: Duration,
    /// Hard ceiling on time-to-finality per batch.
    pub batch_timeout: Duration,
}

/// One successful `AcceptAssignmentV2` batch.
#[derive(Debug, Clone)]
pub struct BatchOutcome {
    /// Chunk indices submitted in this batch (sorted ascending).
    pub chunk_indices: Vec<u32>,
    /// Nonce used. Each batch increments this monotonically from
    /// `request.starting_nonce`.
    pub nonce: u64,
    /// Tx hash returned by `send_raw_transaction`.
    pub tx_hash: String,
    /// Block height at which the tx finalized.
    pub finalized_at_height: u64,
}

/// Failure modes for [`AssignmentAttestor::attest`].
#[derive(Debug, Error)]
pub enum AttestError {
    /// Configuration rejected before any work was attempted. Today
    /// emitted only when `params.max_chunk_indices_per_tx == 0`,
    /// which would otherwise panic `Vec::chunks`. No tx submitted; no
    /// nonce consumed. Caller must fix `V2Params` before retrying.
    #[error("invalid attestor params: {reason}")]
    BadParams { reason: String },

    /// `send_raw_transaction` returned an error before the tx ever
    /// reached the mempool. Most often: malformed nonce / fee /
    /// transport failure. The chunks in `chunk_indices` are NOT
    /// attested. **The chain's nonce counter for this account is
    /// unaffected** — caller may reuse `nonce` after refreshing it
    /// (see also [`AttestSummary::last_finalized_nonce`]).
    #[error("submit failed for batch at nonce {nonce}: {source}")]
    Submit {
        nonce: u64,
        chunk_indices: Vec<u32>,
        #[source]
        source: anyhow::Error,
    },

    /// The tx was submitted but `wait_for_finalized` did not see
    /// `Finalized` — see [`TxWaitError`] for the breakdown.
    /// `chunk_indices` and `nonce` are surfaced so the caller can
    /// resume against `storage_getAssignmentCoverageV2`.
    ///
    /// **Nonce-counter side effects depend on the inner variant:**
    ///   * `TxWaitError::Failed` — tx was on chain (executed and
    ///     reverted, OR rejected at validity); the chain DID consume
    ///     the nonce. Caller MUST advance.
    ///   * `TxWaitError::Dropped` — tx never reached inclusion; nonce
    ///     NOT consumed. Caller may reuse.
    ///   * `TxWaitError::Timeout` — ambiguous (could land later);
    ///     caller MUST refresh nonce from chain before any reuse.
    ///   * `TxWaitError::Rpc` — same as Timeout: refresh first.
    ///
    /// Bottom line: do NOT derive next-nonce arithmetically from
    /// [`AttestSummary::last_nonce_attempted`] after this error — go
    /// to chain.
    #[error("waited for finality of batch at nonce {nonce}: {source}")]
    Wait {
        nonce: u64,
        chunk_indices: Vec<u32>,
        tx_hash: String,
        #[source]
        source: TxWaitError,
    },
}

/// Per-call result. `batches` holds every finalized batch up to the
/// stopping point (in order). `error` is `None` if every batch
/// finalized; otherwise `Some(_)` with the failing batch's details.
///
/// Note: batches before a failure are durable on chain (OR-merge), so
/// `error.is_some()` does NOT mean "nothing got attested." The caller
/// should consult `storage_getAssignmentCoverageV2` to discover what
/// still needs attestation, then re-call the attestor with the
/// remaining held set and a fresh `starting_nonce`.
#[derive(Debug)]
pub struct AttestSummary {
    pub batches: Vec<BatchOutcome>,
    pub error: Option<AttestError>,
}

impl AttestSummary {
    /// True if every batch finalized.
    pub fn fully_attested(&self) -> bool {
        self.error.is_none()
    }

    /// Total chunk indices successfully attested across all batches.
    pub fn attested_count(&self) -> usize {
        self.batches.iter().map(|b| b.chunk_indices.len()).sum()
    }

    /// Highest nonce that the attestor *attempted* — finalized OR
    /// failed. **NOT a safe basis for next-nonce arithmetic on its
    /// own.** A failing batch may or may not have consumed the
    /// chain's nonce slot depending on the failure mode (see
    /// [`AttestError::Wait`]'s nonce side-effects table). Use this
    /// only for diagnostics and tracing.
    ///
    /// For chained tx ordering after an error, prefer:
    ///   1. [`Self::last_finalized_nonce`] for the highest nonce we
    ///      know is on chain, then
    ///   2. a fresh `get_nonce` RPC against the account before
    ///      submitting the next tx.
    pub fn last_nonce_attempted(&self) -> Option<u64> {
        let last_finalized = self.batches.last().map(|b| b.nonce);
        let last_failed = self.error.as_ref().and_then(|e| match e {
            AttestError::BadParams { .. } => None,
            AttestError::Submit { nonce, .. } | AttestError::Wait { nonce, .. } => Some(*nonce),
        });
        last_failed.or(last_finalized)
    }

    /// Highest nonce the chain has finalized for this attestor run.
    /// `None` when no batch finalized (or before any tx was submitted).
    /// Safe to use as a "watermark below which we know nonces are
    /// consumed"; caller still must `get_nonce` for the next slot.
    pub fn last_finalized_nonce(&self) -> Option<u64> {
        self.batches.last().map(|b| b.nonce)
    }
}

/// V2 attestation submitter.
pub struct AssignmentAttestor<R: AttestorRpc> {
    rpc: R,
    signing_key_seed: [u8; 32],
    my_addr: [u8; 20],
    chain_id: u64,
    fee_per_tx: u128,
    params: V2Params,
}

impl<R: AttestorRpc> AssignmentAttestor<R> {
    pub fn new(
        rpc: R,
        signing_key_seed: [u8; 32],
        my_addr: [u8; 20],
        chain_id: u64,
        fee_per_tx: u128,
        params: V2Params,
    ) -> Self {
        Self {
            rpc,
            signing_key_seed,
            my_addr,
            chain_id,
            fee_per_tx,
            params,
        }
    }

    /// Compute the assigned set ∩ held set, batch, submit, and wait.
    /// See module-level docs for failure semantics.
    pub async fn attest(&self, req: AttestRequest) -> AttestSummary {
        // Param validation — `chunks(0)` would otherwise panic at runtime
        // even in release builds. A misconfigured `V2Params` should never
        // bring down the node; surface it as a typed error instead.
        if self.params.max_chunk_indices_per_tx == 0 {
            return AttestSummary {
                batches: vec![],
                error: Some(AttestError::BadParams {
                    reason: "max_chunk_indices_per_tx must be > 0 \
                             (chain plan v3.2 default = 65,536)".into(),
                }),
            };
        }

        let assigned = chunks_for_archive_v2(
            &req.merkle_root,
            req.chunk_count,
            &req.snapshot,
            self.params.assignment_replication_factor,
            &self.my_addr,
        );

        // `BTreeSet::intersection` produces a sorted iterator — batches
        // come out in deterministic ascending order, identical across
        // runs against the same input.
        let attest_set: Vec<u32> = assigned.intersection(&req.held).copied().collect();

        if attest_set.is_empty() {
            debug!(
                root = %hex::encode(req.merkle_root),
                assigned_count = assigned.len(),
                held_count = req.held.len(),
                "attest: nothing to do (empty intersection of assignment and held set)"
            );
            return AttestSummary { batches: vec![], error: None };
        }

        info!(
            root = %hex::encode(req.merkle_root),
            attest_count = attest_set.len(),
            cap = self.params.max_chunk_indices_per_tx,
            "attest: starting OR-merge attestation"
        );

        let cap = self.params.max_chunk_indices_per_tx as usize;
        // Validated up-front above; `cap > 0` is an invariant by the time
        // we reach `chunks(cap)`.

        let mut batches: Vec<BatchOutcome> = Vec::new();
        let mut nonce = req.starting_nonce;

        for chunk_batch in attest_set.chunks(cap) {
            let chunk_indices: Vec<u32> = chunk_batch.to_vec();

            // Building the tx is infallible given a 32-byte seed; the
            // builder propagates only bincode serialization errors,
            // which are themselves infallible for our mirror types.
            // We still surface them through Submit::source if they
            // ever occur, so a corrupt seed doesn't panic the node.
            let tx_hex = match build_accept_assignment_v2_tx(
                &self.signing_key_seed,
                self.chain_id,
                nonce,
                self.fee_per_tx,
                req.merkle_root,
                chunk_indices.clone(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    return AttestSummary {
                        batches,
                        error: Some(AttestError::Submit {
                            nonce,
                            chunk_indices,
                            source: e,
                        }),
                    };
                }
            };

            let tx_hash = match self.rpc.send_raw_transaction(&tx_hex).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(nonce, err = %e, "attest: send_raw_transaction failed");
                    return AttestSummary {
                        batches,
                        error: Some(AttestError::Submit {
                            nonce,
                            chunk_indices,
                            source: e,
                        }),
                    };
                }
            };

            let height = match wait_for_finalized(
                &self.rpc,
                &tx_hash,
                req.poll_interval,
                req.batch_timeout,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    warn!(nonce, %tx_hash, err = %e, "attest: wait_for_finalized failed");
                    return AttestSummary {
                        batches,
                        error: Some(AttestError::Wait {
                            nonce,
                            chunk_indices,
                            tx_hash,
                            source: e,
                        }),
                    };
                }
            };

            debug!(
                nonce,
                %tx_hash,
                finalized_at_height = height,
                batch_size = chunk_indices.len(),
                "attest: batch finalized"
            );
            batches.push(BatchOutcome {
                chunk_indices,
                nonce,
                tx_hash,
                finalized_at_height: height,
            });
            nonce += 1;
        }

        info!(
            root = %hex::encode(req.merkle_root),
            batches_finalized = batches.len(),
            "attest: every batch finalized"
        );
        AttestSummary { batches, error: None }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use sum_types::rpc_types::TxStatusV2;

    /// Scripted RPC that records every send + status call. Submit
    /// responses come from a queue; status is also queued, drained per
    /// `wait_for_finalized` poll.
    #[derive(Default)]
    struct MockRpc {
        send_responses: StdMutex<VecDeque<Result<String, String>>>,
        status_responses: StdMutex<VecDeque<Result<TxStatusV2, String>>>,
        sent_txs: StdMutex<Vec<String>>,
    }

    impl MockRpc {
        fn new() -> Self {
            Self::default()
        }
        fn enqueue_send_ok(&self, tx_hash: &str) {
            self.send_responses
                .lock()
                .unwrap()
                .push_back(Ok(tx_hash.into()));
        }
        fn enqueue_send_err(&self, msg: &str) {
            self.send_responses
                .lock()
                .unwrap()
                .push_back(Err(msg.into()));
        }
        fn enqueue_status_ok(&self, st: TxStatusV2) {
            self.status_responses.lock().unwrap().push_back(Ok(st));
        }
        fn sent_count(&self) -> usize {
            self.sent_txs.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TxStatusSource for MockRpc {
        async fn get_transaction_status(&self, _tx_hash: &str) -> anyhow::Result<TxStatusV2> {
            let next = self
                .status_responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("test bug: no scripted status response"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    #[async_trait]
    impl AttestorRpc for MockRpc {
        async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
            self.sent_txs.lock().unwrap().push(signed_tx_hex.to_string());
            let next = self
                .send_responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("test bug: no scripted send response"))?;
            next.map_err(anyhow::Error::msg)
        }
    }

    /// Five-archive snapshot identical to the W4 fixture; address[0] in
    /// `0xAA..0xAE`.
    fn five_archives() -> Vec<[u8; 20]> {
        (0..5)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xAA + i;
                a
            })
            .collect()
    }

    /// Build an attestor with a small per-batch cap so multi-batch tests
    /// fit in test sizes. `R = 5` so every chunk is assigned to every
    /// archive (snapshot.len = 5, R clamps to 5) — caller can then
    /// hand-craft a `held` set of any size up to chunk_count.
    fn make_attestor(rpc: MockRpc, my_addr: [u8; 20], cap: u32) -> AssignmentAttestor<MockRpc> {
        AssignmentAttestor::new(
            rpc,
            [42u8; 32], // signing seed
            my_addr,
            1337,    // chain_id
            1_000_000, // fee
            V2Params {
                assignment_replication_factor: 5,
                max_chunk_indices_per_tx: cap,
            },
        )
    }

    fn req(
        root: [u8; 32],
        chunk_count: u32,
        snapshot: Vec<[u8; 20]>,
        held: BTreeSet<u32>,
        nonce: u64,
    ) -> AttestRequest {
        AttestRequest {
            merkle_root: root,
            chunk_count,
            snapshot,
            held,
            starting_nonce: nonce,
            poll_interval: Duration::from_millis(10),
            batch_timeout: Duration::from_secs(5),
        }
    }

    // ── Required tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn empty_held_is_no_op() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        let attestor = make_attestor(rpc, my_addr, 4);

        let summary = attestor
            .attest(req([0xAA; 32], 16, snapshot, BTreeSet::new(), 100))
            .await;
        assert!(summary.fully_attested());
        assert!(summary.batches.is_empty());
        assert_eq!(attestor.rpc.sent_count(), 0, "no-op: no tx submitted");
    }

    #[tokio::test]
    async fn held_disjoint_from_assignment_is_no_op() {
        // Use my_addr that's NOT in the snapshot — chunks_for_archive_v2
        // returns empty, so even a non-empty held set yields no batches.
        let snapshot = five_archives();
        let stranger = [0xFE; 20]; // not in snapshot
        let rpc = MockRpc::new();
        let attestor = make_attestor(rpc, stranger, 4);

        let held: BTreeSet<u32> = (0..16).collect();
        let summary = attestor
            .attest(req([0xBB; 32], 16, snapshot, held, 200))
            .await;
        assert!(summary.fully_attested());
        assert!(summary.batches.is_empty());
        assert_eq!(attestor.rpc.sent_count(), 0);
    }

    #[tokio::test]
    async fn single_batch_finalizes() {
        // R = 5, snapshot.len = 5 → every chunk assigned to every archive.
        // Held = [0, 1, 2, 3] → 4 chunks ≤ cap = 4 → exactly one batch.
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        rpc.enqueue_send_ok("0xtx1");
        rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: 1000 });
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..4).collect();
        let summary = attestor
            .attest(req([0xCC; 32], 4, snapshot, held, 50))
            .await;

        assert!(summary.fully_attested(), "summary error: {:?}", summary.error);
        assert_eq!(summary.batches.len(), 1);
        let b = &summary.batches[0];
        assert_eq!(b.chunk_indices, vec![0, 1, 2, 3]);
        assert_eq!(b.nonce, 50);
        assert_eq!(b.tx_hash, "0xtx1");
        assert_eq!(b.finalized_at_height, 1000);
        assert_eq!(summary.attested_count(), 4);
        assert_eq!(attestor.rpc.sent_count(), 1);
    }

    #[tokio::test]
    async fn multi_batch_chunks_into_cap_sized_batches() {
        // 10 chunks, cap = 4 → batches of 4, 4, 2.
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        for i in 0..3 {
            rpc.enqueue_send_ok(&format!("0xtx{i}"));
            rpc.enqueue_status_ok(TxStatusV2::Finalized {
                block_height: 1000 + i as u64,
            });
        }
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..10).collect();
        let summary = attestor
            .attest(req([0xDD; 32], 10, snapshot, held, 7))
            .await;

        assert!(summary.fully_attested(), "{:?}", summary.error);
        assert_eq!(summary.batches.len(), 3);
        assert_eq!(summary.batches[0].chunk_indices, vec![0, 1, 2, 3]);
        assert_eq!(summary.batches[1].chunk_indices, vec![4, 5, 6, 7]);
        assert_eq!(summary.batches[2].chunk_indices, vec![8, 9]);
        // Nonces are monotonic from starting_nonce.
        assert_eq!(summary.batches[0].nonce, 7);
        assert_eq!(summary.batches[1].nonce, 8);
        assert_eq!(summary.batches[2].nonce, 9);
        assert_eq!(summary.attested_count(), 10);
    }

    #[tokio::test]
    async fn batch_chunks_are_deterministic_across_runs() {
        // Two runs with identical input — the batched chunk_indices
        // sequences must be byte-identical. Catches a refactor that
        // accidentally orders by HashSet (non-deterministic).
        async fn run_once() -> Vec<Vec<u32>> {
            let snapshot = five_archives();
            let my_addr = snapshot[0];
            let rpc = MockRpc::new();
            for i in 0..3 {
                rpc.enqueue_send_ok(&format!("0xt{i}"));
                rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: 100 + i });
            }
            let attestor = make_attestor(rpc, my_addr, 4);
            // Insert held in deliberately non-sorted order.
            let mut held = BTreeSet::new();
            for v in [9u32, 0, 5, 3, 7, 1, 4, 8, 2, 6] {
                held.insert(v);
            }
            let summary = attestor
                .attest(req([0xEE; 32], 10, snapshot, held, 0))
                .await;
            summary
                .batches
                .into_iter()
                .map(|b| b.chunk_indices)
                .collect()
        }
        let run1 = run_once().await;
        let run2 = run_once().await;
        assert_eq!(run1, run2);
        assert_eq!(run1[0], vec![0, 1, 2, 3], "first batch must be sorted asc");
    }

    #[tokio::test]
    async fn intersection_filters_held_outside_assignment() {
        // Build a 3-archive snapshot with R = 1 so each chunk has
        // exactly one assigned archive. We'll hold every chunk but
        // only ~chunk_count / 3 should be attested (the assignment
        // intersection).
        let snapshot: Vec<[u8; 20]> = (0..3)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xA0 + i;
                a
            })
            .collect();
        let my_addr = snapshot[0];

        let rpc = MockRpc::new();
        // We don't yet know how many batches will result; queue one
        // happy response and one timeout-ish overflow buffer. Query
        // counts asserted below.
        for i in 0..32 {
            rpc.enqueue_send_ok(&format!("0xb{i}"));
            rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: 100 + i });
        }
        let mut attestor = make_attestor(rpc, my_addr, 64);
        attestor.params.assignment_replication_factor = 1; // override

        let chunk_count = 24u32;
        let held: BTreeSet<u32> = (0..chunk_count).collect();

        // Compute the expected attest set externally to check the
        // attestor agrees byte-for-byte.
        let expected: BTreeSet<u32> = chunks_for_archive_v2(
            &[0x12; 32],
            chunk_count,
            &snapshot,
            1,
            &my_addr,
        )
        .into_iter()
        .collect();

        let summary = attestor
            .attest(req([0x12; 32], chunk_count, snapshot, held, 0))
            .await;
        assert!(summary.fully_attested(), "{:?}", summary.error);

        let actual: BTreeSet<u32> = summary
            .batches
            .iter()
            .flat_map(|b| b.chunk_indices.iter().copied())
            .collect();
        assert_eq!(actual, expected);
        // Sanity: a non-trivial subset (R=1 distributes among 3
        // archives roughly evenly, so we expect 24/3 ≈ 8 chunks).
        assert!(
            (4..=16).contains(&actual.len()),
            "expected ~chunk_count/snapshot_size attestations, got {}",
            actual.len()
        );
    }

    #[tokio::test]
    async fn submit_failure_on_first_batch_returns_partial_with_zero_batches() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        rpc.enqueue_send_err("simulated mempool reject");
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..4).collect();
        let summary = attestor
            .attest(req([0x55; 32], 4, snapshot, held, 100))
            .await;
        assert!(!summary.fully_attested());
        assert_eq!(summary.batches.len(), 0);
        assert_eq!(summary.last_nonce_attempted(), Some(100));
        // No batch finalized: the safe nonce helper must return None.
        // W10 must NOT use last_nonce_attempted to derive a next nonce
        // here — the chain's nonce counter is unmoved.
        assert_eq!(summary.last_finalized_nonce(), None);
        match summary.error.expect("error must be set") {
            AttestError::Submit { nonce, chunk_indices, source } => {
                assert_eq!(nonce, 100);
                assert_eq!(chunk_indices, vec![0, 1, 2, 3]);
                assert!(source.to_string().contains("simulated mempool reject"));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_failure_mid_run_keeps_earlier_finalized_batches() {
        // 3 batches; second submit fails after first finalizes.
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        rpc.enqueue_send_ok("0xtx0");
        rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: 200 });
        rpc.enqueue_send_err("transient transport error");
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..10).collect();
        let summary = attestor
            .attest(req([0x66; 32], 10, snapshot, held, 50))
            .await;
        assert!(!summary.fully_attested());
        assert_eq!(summary.batches.len(), 1, "first batch must remain finalized");
        assert_eq!(summary.batches[0].nonce, 50);
        // attempted = 51 (the failing batch); finalized = 50 (the
        // successful one). The two helpers must diverge here so the
        // caller can distinguish "what's on chain" from "what we tried."
        assert_eq!(summary.last_nonce_attempted(), Some(51));
        assert_eq!(summary.last_finalized_nonce(), Some(50));
        match summary.error.expect("error must be set") {
            AttestError::Submit { nonce, chunk_indices, .. } => {
                assert_eq!(nonce, 51);
                assert_eq!(chunk_indices, vec![4, 5, 6, 7]);
            }
            other => panic!("expected Submit on second batch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_failed_status_propagates_terminal_failure() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        // First batch: submit ok, status returns Failed.
        rpc.enqueue_send_ok("0xtx-bad");
        rpc.enqueue_status_ok(TxStatusV2::Failed {
            block_height: Some(310),
            reason: "signature_invalid".into(),
        });
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..4).collect();
        let summary = attestor
            .attest(req([0x77; 32], 4, snapshot, held, 12))
            .await;
        assert!(!summary.fully_attested());
        assert_eq!(summary.batches.len(), 0);
        match summary.error.expect("error must be set") {
            AttestError::Wait { nonce, tx_hash, source: TxWaitError::Failed { reason, .. }, .. } => {
                assert_eq!(nonce, 12);
                assert_eq!(tx_hash, "0xtx-bad");
                assert!(reason.contains("signature_invalid"));
            }
            other => panic!("expected Wait::Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_dropped_status_propagates() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        rpc.enqueue_send_ok("0xtx-dropped");
        rpc.enqueue_status_ok(TxStatusV2::Dropped);
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..4).collect();
        let summary = attestor
            .attest(req([0x88; 32], 4, snapshot, held, 1))
            .await;
        match summary.error.expect("error must be set") {
            AttestError::Wait { source: TxWaitError::Dropped, nonce, .. } => {
                assert_eq!(nonce, 1);
            }
            other => panic!("expected Wait::Dropped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_then_finalized_loops_until_finality() {
        // Stress the wait loop: status returns Pending twice then
        // Finalized. The attestor should still treat the batch as
        // successful and consume all three status responses.
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        rpc.enqueue_send_ok("0xtx-slow");
        rpc.enqueue_status_ok(TxStatusV2::Pending);
        rpc.enqueue_status_ok(TxStatusV2::Pending);
        rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: 999 });
        let attestor = make_attestor(rpc, my_addr, 4);

        let held: BTreeSet<u32> = (0..2).collect();
        let summary = attestor
            .attest(req([0x99; 32], 2, snapshot, held, 30))
            .await;
        assert!(summary.fully_attested(), "{:?}", summary.error);
        assert_eq!(summary.batches[0].finalized_at_height, 999);
    }

    #[tokio::test]
    async fn nonce_helpers_agree_when_every_batch_finalized() {
        // No errors: last_nonce_attempted and last_finalized_nonce are
        // identical (both = highest finalized batch nonce).
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        for i in 0..3 {
            rpc.enqueue_send_ok(&format!("0xtx{i}"));
            rpc.enqueue_status_ok(TxStatusV2::Finalized { block_height: i });
        }
        let attestor = make_attestor(rpc, my_addr, 1);

        let held: BTreeSet<u32> = (0..3).collect();
        let summary = attestor
            .attest(req([0xAA; 32], 3, snapshot, held, 500))
            .await;
        assert_eq!(summary.last_nonce_attempted(), Some(502));
        assert_eq!(summary.last_finalized_nonce(), Some(502));
    }

    /// Reviewer-required: `max_chunk_indices_per_tx = 0` must NOT panic
    /// — release builds run on real chains, and a misconfigured
    /// `V2Params` should never bring the node down. Surface as a
    /// typed `BadParams` error in the summary; no tx submitted; no
    /// nonce consumed.
    #[tokio::test]
    async fn zero_max_chunk_indices_per_tx_returns_bad_params() {
        let snapshot = five_archives();
        let my_addr = snapshot[0];
        let rpc = MockRpc::new();
        // No scripted send/status responses — if the attestor
        // accidentally ran, the mock would surface an error from the
        // empty queue, which would still NOT be BadParams.
        let attestor = AssignmentAttestor::new(
            rpc,
            [42u8; 32],
            my_addr,
            1337,
            1_000_000,
            V2Params {
                assignment_replication_factor: 5,
                max_chunk_indices_per_tx: 0,
            },
        );

        let held: BTreeSet<u32> = (0..16).collect();
        let summary = attestor
            .attest(req([0xBB; 32], 16, snapshot, held, 1))
            .await;
        assert!(!summary.fully_attested());
        assert!(summary.batches.is_empty());
        assert_eq!(summary.last_nonce_attempted(), None);
        assert_eq!(summary.last_finalized_nonce(), None);
        match summary.error.expect("error must be set") {
            AttestError::BadParams { reason } => {
                assert!(reason.contains("max_chunk_indices_per_tx"));
            }
            other => panic!("expected BadParams, got {other:?}"),
        }
        // Hard guarantee: the validator must not have called send_raw_transaction.
        assert_eq!(attestor.rpc.sent_count(), 0);
    }
}
