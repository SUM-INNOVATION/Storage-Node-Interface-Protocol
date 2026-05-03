//! Polling primitive for waiting on a SUM Chain V2 transaction to
//! become **finalized**, surfacing the per-status terminal semantics
//! locked in the Phase 0b plan:
//!
//! | Wire status | What [`wait_for_finalized`] does |
//! |---|---|
//! | `Unknown` | Keep polling. Not fatal during the wait window — the chain may not have indexed the tx yet. On overall timeout, returned as `TxWaitError::Timeout { last_status: Unknown }` so the caller can leave the file `Pending` for `resume`. |
//! | `Pending` | Keep polling. |
//! | `Included` | Keep polling. The tx is on chain but not yet at finality depth (depth=3 for PoA, depth=0 for BFT per chain plan §4). |
//! | `Finalized { block_height }` | Return `Ok(block_height)`. |
//! | `Failed { block_height, reason }` | Terminal for THIS tx hash. Returned as `TxWaitError::Failed`. The caller may build a fresh tx (new nonce) only if the operation is idempotent and the failure reason is classified as transient. |
//! | `Dropped` | Terminal for THIS tx hash. Returned as `TxWaitError::Dropped`. Unlike `Failed`, no chain state was touched, so resubmitting the same logical operation with a new nonce is safe. |
//!
//! The tx-status source is a trait ([`TxStatusSource`]) so tests can
//! drive the waiter with scripted responses without spinning up an
//! HTTP server. `L1RpcClient` is the production implementor.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sum_types::rpc_types::TxStatusV2;
use thiserror::Error;
use tracing::{debug, warn};

use crate::rpc_client::L1RpcClient;

/// Abstraction over `chain_getTransactionStatus` for testability.
#[async_trait]
pub trait TxStatusSource: Send + Sync {
    async fn get_transaction_status(&self, tx_hash: &str) -> Result<TxStatusV2>;
}

#[async_trait]
impl TxStatusSource for L1RpcClient {
    async fn get_transaction_status(&self, tx_hash: &str) -> Result<TxStatusV2> {
        self.chain_get_transaction_status(tx_hash).await
    }
}

/// Reasons [`wait_for_finalized`] returned without seeing a `Finalized` status.
#[derive(Debug, Error)]
pub enum TxWaitError {
    /// Chain reported the tx as `Failed` (executed but reverted, OR
    /// rejected at validity). Terminal — do NOT retry the same tx hash.
    /// `reason` comes from `TxStatusV2::Failed.reason` (chain plan §10
    /// receipt-code mapping).
    #[error("transaction failed (block_height={block_height:?}): {reason}")]
    Failed {
        block_height: Option<u64>,
        reason: String,
    },

    /// Chain reported the tx as `Dropped` (mempool eviction / reorg
    /// pre-inclusion). Terminal for the tx hash, but resubmitting the
    /// same logical operation with a fresh nonce is safe.
    #[error("transaction dropped from mempool")]
    Dropped,

    /// Wait window elapsed. `last_status` is whatever we saw most
    /// recently — `Unknown`, `Pending`, or `Included { … }`. Caller
    /// decides next: `resume` later, escalate to `AbandonFileV2` if
    /// `--abandon-on-failure`, or surface to the operator.
    #[error("timed out waiting for finality (last_status: {last_status:?})")]
    Timeout { last_status: TxStatusV2 },

    /// Underlying RPC error (transport failure, malformed response,
    /// chain returned a JSON-RPC `error` field). The caller can
    /// distinguish transient network errors from terminal chain
    /// rejections by inspecting the wrapped error.
    #[error("RPC error while polling tx status: {0}")]
    Rpc(#[from] anyhow::Error),
}

/// Default poll interval — matches `block_time_ms` from chain plan
/// Appendix B. Polling faster wastes RPC calls; slower delays detection.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Poll `chain_getTransactionStatus` until the tx reaches `Finalized`,
/// terminates (`Failed`/`Dropped`), the wait window elapses, or an RPC
/// error bubbles up.
///
/// On `Ok(height)`, the tx is finalized at that block height.
///
/// Polling cadence is exponential-backoff-free: a fixed
/// `poll_interval` (default `DEFAULT_POLL_INTERVAL = 2s`). Operations
/// that need a different cadence pass their own.
pub async fn wait_for_finalized<S: TxStatusSource + ?Sized>(
    rpc: &S,
    tx_hash: &str,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<u64, TxWaitError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_status = TxStatusV2::Unknown;

    loop {
        let status = rpc.get_transaction_status(tx_hash).await?;
        last_status = status.clone();

        match status {
            TxStatusV2::Finalized { block_height } => {
                debug!(%tx_hash, block_height, "tx finalized");
                return Ok(block_height);
            }
            TxStatusV2::Failed { block_height, reason } => {
                warn!(%tx_hash, ?block_height, %reason, "tx failed (terminal)");
                return Err(TxWaitError::Failed { block_height, reason });
            }
            TxStatusV2::Dropped => {
                warn!(%tx_hash, "tx dropped from mempool (terminal, resubmittable)");
                return Err(TxWaitError::Dropped);
            }
            TxStatusV2::Unknown | TxStatusV2::Pending | TxStatusV2::Included { .. } => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    warn!(%tx_hash, ?last_status, "tx wait timed out");
                    return Err(TxWaitError::Timeout { last_status });
                }
                // Sleep until the earlier of (poll_interval) or (deadline).
                let until = std::cmp::min(now + poll_interval, deadline);
                tokio::time::sleep_until(until).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test source that returns scripted statuses in order. Each call
    /// consumes one entry; if the script is exhausted, returns the last
    /// entry repeatedly (so callers can model "stuck on Unknown" by
    /// passing `[Unknown]` once).
    struct ScriptedSource {
        script: Mutex<std::collections::VecDeque<Result<TxStatusV2, String>>>,
        last: Mutex<Option<Result<TxStatusV2, String>>>,
    }

    impl ScriptedSource {
        fn new(script: Vec<Result<TxStatusV2, &'static str>>) -> Self {
            let q: std::collections::VecDeque<_> = script
                .into_iter()
                .map(|r| r.map_err(|s| s.to_string()))
                .collect();
            Self {
                script: Mutex::new(q),
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl TxStatusSource for ScriptedSource {
        async fn get_transaction_status(&self, _tx_hash: &str) -> Result<TxStatusV2> {
            let next = {
                let mut q = self.script.lock().unwrap();
                q.pop_front()
            };
            let resp = match next {
                Some(r) => {
                    *self.last.lock().unwrap() = Some(r.clone());
                    r
                }
                None => self
                    .last
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("scripted source must have at least one entry"),
            };
            resp.map_err(anyhow::Error::msg)
        }
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn finalized_immediately_returns_height() {
        let src = ScriptedSource::new(vec![Ok(TxStatusV2::Finalized { block_height: 42 })]);
        let height = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(height, 42);
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn pending_then_finalized() {
        let src = ScriptedSource::new(vec![
            Ok(TxStatusV2::Pending),
            Ok(TxStatusV2::Pending),
            Ok(TxStatusV2::Finalized { block_height: 100 }),
        ]);
        let height = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(height, 100);
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn included_treated_as_keep_polling_until_finalized() {
        // Included is NOT terminal — keep polling.
        let src = ScriptedSource::new(vec![
            Ok(TxStatusV2::Pending),
            Ok(TxStatusV2::Included { block_height: 99 }),
            Ok(TxStatusV2::Included { block_height: 99 }),
            Ok(TxStatusV2::Finalized { block_height: 99 }),
        ]);
        let height = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(height, 99);
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn unknown_is_not_fatal_during_window() {
        // Unknown then Finalized — should poll through Unknown.
        let src = ScriptedSource::new(vec![
            Ok(TxStatusV2::Unknown),
            Ok(TxStatusV2::Unknown),
            Ok(TxStatusV2::Finalized { block_height: 7 }),
        ]);
        let height = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(height, 7);
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn failed_is_terminal_no_retry() {
        let src = ScriptedSource::new(vec![
            Ok(TxStatusV2::Pending),
            Ok(TxStatusV2::Failed {
                block_height: Some(50),
                reason: "low-order x25519 public key rejected".into(),
            }),
        ]);
        let err = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap_err();
        match err {
            TxWaitError::Failed { block_height, reason } => {
                assert_eq!(block_height, Some(50));
                assert!(reason.contains("low-order"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn dropped_is_terminal_resubmittable() {
        let src = ScriptedSource::new(vec![Ok(TxStatusV2::Pending), Ok(TxStatusV2::Dropped)]);
        let err = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(matches!(err, TxWaitError::Dropped));
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn timeout_returns_last_seen_status() {
        // Pending forever — wait_for_finalized must time out and report
        // last_status = Pending.
        let src = ScriptedSource::new(vec![Ok(TxStatusV2::Pending)]);
        let err = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_millis(100))
            .await
            .unwrap_err();
        match err {
            TxWaitError::Timeout { last_status } => {
                assert_eq!(last_status, TxStatusV2::Pending);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn timeout_with_unknown_lets_caller_resume() {
        // Stuck on Unknown the whole window — operator-visible scenario
        // where the chain hasn't indexed the tx yet. Must NOT auto-fail
        // as Failed/Dropped; must time out so the caller can `resume`.
        let src = ScriptedSource::new(vec![Ok(TxStatusV2::Unknown)]);
        let err = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_millis(100))
            .await
            .unwrap_err();
        match err {
            TxWaitError::Timeout { last_status: TxStatusV2::Unknown } => {}
            other => panic!("expected Timeout(Unknown), got {other:?}"),
        }
    }

    // Tokio's `start_paused` requires the `test-util` feature, which is
    // not enabled in the workspace. The tests run on real time with
    // 10 ms poll intervals, so each completes in well under 1 s.
    #[tokio::test]
    async fn rpc_error_propagates() {
        let src = ScriptedSource::new(vec![Err("network unreachable")]);
        let err = wait_for_finalized(&src, "0xtx", Duration::from_millis(10), Duration::from_secs(60))
            .await
            .unwrap_err();
        match err {
            TxWaitError::Rpc(e) => assert!(e.to_string().contains("network unreachable")),
            other => panic!("expected Rpc, got {other:?}"),
        }
    }
}
