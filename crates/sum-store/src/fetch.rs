//! Outbound chunk fetch orchestration.
//!
//! [`FetchManager`] tracks in-progress fetches. Each fetch proceeds as a
//! sequence of windowed request-response round-trips, reassembles the data,
//! verifies the CID, and stores the chunk to disk.
//!
//! ## Defensive metadata validation
//!
//! The fetcher only allocates after [`validate_response_metadata`] has
//! confirmed the peer's claimed `total_bytes` is sane. The hard upper bound
//! is [`MAX_CHUNK_BYTES`] (1 MiB — the protocol's chunk size). When the
//! caller knows the expected chunk size from a manifest, it should pass it
//! via [`FetchManager::start_fetch_with_expected_size`] for an even tighter
//! bound. This defends against malicious peers who could otherwise advertise
//! `total_bytes = u64::MAX` and trigger an OOM via `Vec::reserve`.

use std::collections::HashMap;

use async_trait::async_trait;
use libp2p::PeerId;
use sum_net::{SumNet, ShardResponse};
use tracing::{info, warn};

use crate::error::{Result, StoreError};
use crate::store::ChunkStore;
use crate::verify;

// ── FetchNet trait ───────────────────────────────────────────────────────────

/// The single network operation [`FetchManager`] needs from the swarm.
///
/// Abstracting this behind a trait lets tests inject a mock without spinning
/// up a real libp2p swarm. Production callers pass `&SumNet` (which
/// implements this trait via the impl below); test callers pass any type
/// that implements [`FetchNet`].
#[async_trait]
pub trait FetchNet: Send + Sync {
    /// Issue a `request_shard_chunk` for `cid` (or a windowed sub-range)
    /// against `peer_id`.
    async fn request_shard_chunk(
        &self,
        peer_id: PeerId,
        cid: String,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> anyhow::Result<()>;
}

#[async_trait]
impl FetchNet for SumNet {
    async fn request_shard_chunk(
        &self,
        peer_id: PeerId,
        cid: String,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> anyhow::Result<()> {
        SumNet::request_shard_chunk(self, peer_id, cid, offset, max_bytes).await
    }
}

/// Hard upper bound on the total bytes the fetcher will reserve for a
/// single chunk, regardless of what a peer claims.
///
/// Set to [`sum_types::storage::CHUNK_SIZE`] (1 MiB) — the protocol
/// guarantees no chunk is larger than this. A peer claiming otherwise is
/// invalid by definition.
pub const MAX_CHUNK_BYTES: u64 = sum_types::storage::CHUNK_SIZE;

/// Tracks in-flight fetches keyed by CID.
pub struct FetchManager {
    /// Maximum bytes per request-response round-trip.
    chunk_size: u64,
    /// In-progress fetches: CID -> state.
    active: HashMap<String, FetchState>,
}

/// State for a single in-progress chunk fetch.
struct FetchState {
    peer_id: PeerId,
    /// Total chunk size as claimed by the peer's first response.
    /// `None` until the first response arrives and passes validation.
    total_bytes: Option<u64>,
    /// Byte offset of the next piece to request.
    next_offset: u64,
    /// Accumulated data.
    buffer: Vec<u8>,
    /// Expected chunk size from the manifest, if known.
    /// Used as a tighter bound during validation.
    expected_size: Option<u64>,
}

/// Outcome of processing a received piece.
#[derive(Debug)]
pub enum FetchOutcome {
    /// More data is needed — the next request has been sent.
    InProgress,
    /// The entire chunk has been received, verified, and stored.
    Complete { cid: String, size: u64 },
    /// The fetch failed.
    Failed { cid: String, error: String },
}

impl FetchManager {
    /// Create a new fetch manager with the given max message size.
    pub fn new(max_chunk_msg_bytes: usize) -> Self {
        Self {
            chunk_size: max_chunk_msg_bytes as u64,
            active: HashMap::new(),
        }
    }

    /// Start fetching a chunk from a remote peer.
    ///
    /// Equivalent to calling [`Self::start_fetch_with_expected_size`] with
    /// `expected_size = None` — only the global [`MAX_CHUNK_BYTES`] safety
    /// bound applies. Prefer the `_with_expected_size` variant when the
    /// caller has manifest context.
    pub async fn start_fetch<N: FetchNet + ?Sized>(
        &mut self,
        net: &N,
        peer_id: PeerId,
        cid: String,
    ) -> Result<()> {
        self.start_fetch_with_expected_size(net, peer_id, cid, None).await
    }

    /// Start fetching a chunk from a remote peer with a known expected size.
    ///
    /// `expected_size`, when provided, is used as a tighter bound during
    /// metadata validation: the peer's `total_bytes` must equal it exactly.
    pub async fn start_fetch_with_expected_size<N: FetchNet + ?Sized>(
        &mut self,
        net: &N,
        peer_id: PeerId,
        cid: String,
        expected_size: Option<u64>,
    ) -> Result<()> {
        if self.active.contains_key(&cid) {
            return Err(StoreError::Other(format!("fetch already in progress: {cid}")));
        }

        info!(%cid, %peer_id, ?expected_size, "starting chunk fetch");

        net.request_shard_chunk(peer_id, cid.clone(), Some(0), Some(self.chunk_size))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))?;

        self.active.insert(cid, FetchState {
            peer_id,
            total_bytes: None,
            next_offset: 0,
            buffer: Vec::new(),
            expected_size,
        });

        Ok(())
    }

    /// Process a received chunk piece.
    pub async fn on_chunk_received<N: FetchNet + ?Sized>(
        &mut self,
        net: &N,
        store: &ChunkStore,
        response: &ShardResponse,
    ) -> FetchOutcome {
        if let Some(ref err) = response.error {
            self.active.remove(&response.cid);
            return FetchOutcome::Failed {
                cid: response.cid.clone(),
                error: err.clone(),
            };
        }

        let state = match self.active.get_mut(&response.cid) {
            Some(s) => s,
            None => {
                warn!(cid = %response.cid, "received data for unknown fetch — ignoring");
                return FetchOutcome::Failed {
                    cid: response.cid.clone(),
                    error: "no active fetch for this CID".into(),
                };
            }
        };

        // ── Validate metadata BEFORE any buffer mutation ──
        // The bound is the tighter of the global safety cap and any
        // expected size we learned from the manifest.
        let max_total = state
            .expected_size
            .map(|s| s.min(MAX_CHUNK_BYTES))
            .unwrap_or(MAX_CHUNK_BYTES);

        if let Err(msg) = validate_response_metadata(
            response,
            state.next_offset,
            state.total_bytes,
            state.expected_size,
            max_total,
        ) {
            warn!(cid = %response.cid, error = %msg, "rejecting ShardResponse — invalid metadata");
            self.active.remove(&response.cid);
            return FetchOutcome::Failed {
                cid: response.cid.clone(),
                error: msg,
            };
        }

        // Validation passed: reservation is now bounded by MAX_CHUNK_BYTES.
        if state.total_bytes.is_none() {
            state.total_bytes = Some(response.total_bytes);
            state.buffer.reserve(response.total_bytes as usize);
        }

        state.buffer.extend_from_slice(&response.data);
        state.next_offset = response.offset + response.data.len() as u64;

        let total = state.total_bytes.unwrap();
        info!(
            cid = %response.cid,
            received = state.next_offset,
            total,
            "piece received"
        );

        if state.next_offset < total {
            let peer_id = state.peer_id;
            let cid = response.cid.clone();
            let offset = state.next_offset;
            let chunk_size = self.chunk_size;

            if let Err(e) = net.request_shard_chunk(
                peer_id,
                cid.clone(),
                Some(offset),
                Some(chunk_size),
            ).await {
                self.active.remove(&cid);
                return FetchOutcome::Failed {
                    cid,
                    error: format!("failed to request next piece: {e}"),
                };
            }

            return FetchOutcome::InProgress;
        }

        let cid = response.cid.clone();
        let state = self.active.remove(&cid).unwrap();

        if let Err(e) = verify::verify_cid(&state.buffer, &cid) {
            return FetchOutcome::Failed {
                cid,
                error: format!("integrity check failed: {e}"),
            };
        }

        let size = state.buffer.len() as u64;
        if let Err(e) = store.put(&cid, &state.buffer) {
            return FetchOutcome::Failed {
                cid,
                error: format!("failed to write chunk: {e}"),
            };
        }

        info!(%cid, size, "chunk fetch complete and verified");
        FetchOutcome::Complete { cid, size }
    }

    /// Check if a fetch is currently active for the given CID.
    pub fn is_active(&self, cid: &str) -> bool {
        self.active.contains_key(cid)
    }

    /// Number of in-progress fetches.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

// ── Pure validation helper ───────────────────────────────────────────────────

/// Validate the metadata in a [`ShardResponse`] before allocating buffers
/// or extending an in-progress fetch.
///
/// Rejects:
/// - `total_bytes == 0`
/// - `total_bytes > max_chunk_bytes`
/// - `total_bytes != expected_size` (when `expected_size` is known)
/// - `data.len() > total_bytes`
/// - subsequent responses where `response.total_bytes` differs from the
///   value the peer reported in its first response
/// - `response.offset != next_offset` (out-of-order or replay)
/// - `next_offset + data.len() > total_bytes` (cumulative overflow)
///
/// `declared_total` is `None` on the first piece of a fetch, and `Some(t)`
/// thereafter (where `t` is the value the peer reported in its first piece).
///
/// This is a pure function — no I/O, no async — so it can be exhaustively
/// unit-tested without spinning up a swarm or mocking the network.
pub(crate) fn validate_response_metadata(
    response: &ShardResponse,
    next_offset: u64,
    declared_total: Option<u64>,
    expected_size: Option<u64>,
    max_chunk_bytes: u64,
) -> std::result::Result<(), String> {
    let total = response.total_bytes;
    let data_len = response.data.len() as u64;

    // (1) total_bytes must be non-zero — peers signal "nothing" via `error`,
    //     not via total_bytes = 0.
    if total == 0 {
        return Err("ShardResponse.total_bytes is zero".into());
    }

    // (2) total_bytes must not exceed the safety bound. This is the core
    //     defense against malicious or buggy peers.
    if total > max_chunk_bytes {
        return Err(format!(
            "ShardResponse.total_bytes = {total} exceeds safety bound {max_chunk_bytes}"
        ));
    }

    // (3) When the caller knows the manifest size, total_bytes must match
    //     it exactly. The peer cannot grow OR shrink the chunk.
    if let Some(expected) = expected_size {
        if total != expected {
            return Err(format!(
                "ShardResponse.total_bytes = {total} does not match expected size {expected}"
            ));
        }
    }

    // (4) The first response decides total_bytes; subsequent responses
    //     must agree. A peer that flips the value mid-stream is malicious.
    if let Some(prev_total) = declared_total {
        if total != prev_total {
            return Err(format!(
                "ShardResponse.total_bytes = {total} differs from previously declared {prev_total}"
            ));
        }
    }

    // (5) A single piece cannot be larger than the whole chunk.
    if data_len > total {
        return Err(format!(
            "ShardResponse.data.len() = {data_len} exceeds total_bytes = {total}"
        ));
    }

    // (6) Pieces must arrive in order — `offset` must equal the next
    //     expected offset for this fetch.
    if response.offset != next_offset {
        return Err(format!(
            "ShardResponse.offset = {} does not match expected next offset {}",
            response.offset, next_offset
        ));
    }

    // (7) Cumulative overflow guard: the next position must not exceed
    //     total_bytes. Also catches u64 wraparound at the addition.
    let next_pos = next_offset
        .checked_add(data_len)
        .ok_or_else(|| "ShardResponse.offset + data.len() overflows u64".to_string())?;
    if next_pos > total {
        return Err(format!(
            "cumulative bytes {next_pos} would exceed total_bytes = {total}"
        ));
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `ShardResponse` for testing.
    fn resp(cid: &str, offset: u64, total_bytes: u64, data: Vec<u8>) -> ShardResponse {
        ShardResponse {
            cid: cid.into(),
            offset,
            total_bytes,
            data,
            error: None,
        }
    }

    // ── Negative: total_bytes bounds ──────────────────────────────────────

    #[test]
    fn rejects_total_bytes_above_safety_bound() {
        let r = resp("c", 0, u64::MAX, vec![]);
        let err = validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("exceeds safety bound"), "got: {err}");
    }

    #[test]
    fn rejects_total_bytes_above_max_chunk_bytes() {
        let r = resp("c", 0, MAX_CHUNK_BYTES + 1, vec![]);
        let err = validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("exceeds safety bound"), "got: {err}");
    }

    #[test]
    fn rejects_total_bytes_zero() {
        let r = resp("c", 0, 0, vec![]);
        let err = validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("zero"), "got: {err}");
    }

    // ── Negative: expected_size mismatch ─────────────────────────────────

    #[test]
    fn rejects_total_bytes_above_expected_size() {
        let r = resp("c", 0, 2048, vec![]);
        let err = validate_response_metadata(&r, 0, None, Some(1024), MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("expected size"), "got: {err}");
    }

    #[test]
    fn rejects_total_bytes_below_expected_size() {
        let r = resp("c", 0, 512, vec![]);
        let err = validate_response_metadata(&r, 0, None, Some(1024), MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("expected size"), "got: {err}");
    }

    // ── Negative: piece size invariants ──────────────────────────────────

    #[test]
    fn rejects_data_larger_than_total() {
        // total = 100, but the single piece is 200 bytes.
        let r = resp("c", 0, 100, vec![0u8; 200]);
        let err = validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("exceeds total_bytes"), "got: {err}");
    }

    // ── Negative: offset / order ─────────────────────────────────────────

    #[test]
    fn rejects_offset_mismatch() {
        // We expect next_offset = 100, but the peer sends offset = 200.
        let r = resp("c", 200, 1024, vec![0u8; 50]);
        let err = validate_response_metadata(&r, 100, Some(1024), None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("does not match expected next offset"), "got: {err}");
    }

    // ── Negative: subsequent inconsistency ───────────────────────────────

    #[test]
    fn rejects_subsequent_total_bytes_change() {
        // First piece declared total = 1024; second piece claims 2048.
        let r = resp("c", 100, 2048, vec![0u8; 50]);
        let err = validate_response_metadata(&r, 100, Some(1024), None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("differs from previously declared"), "got: {err}");
    }

    // ── Negative: cumulative overflow ────────────────────────────────────

    #[test]
    fn rejects_cumulative_overflow() {
        // total = 100, already received 80 bytes, new piece of 50 bytes
        // would put us at 130 — over the declared total.
        let r = resp("c", 80, 100, vec![0u8; 50]);
        let err = validate_response_metadata(&r, 80, Some(100), None, MAX_CHUNK_BYTES).unwrap_err();
        assert!(err.contains("would exceed total_bytes"), "got: {err}");
    }

    #[test]
    fn rejects_offset_plus_data_u64_overflow() {
        // offset near u64::MAX so offset + data.len() would wrap.
        let r = resp("c", u64::MAX - 1, 100, vec![0u8; 10]);
        // Total exceeds the bound, but the offset check fires first; either
        // way the response must be rejected. Use a high bound to force the
        // overflow path to win.
        let err = validate_response_metadata(&r, u64::MAX - 1, Some(100), None, MAX_CHUNK_BYTES).unwrap_err();
        // The first failure that triggers will be the offset/total_bytes check
        // (data_len 10 > total 100? no; total above bound? no; etc).
        // Specifically, offset != next_offset is fine here since they match.
        // Let's chase: total = 100 ok, expected = 100 ok, data_len = 10 ok,
        // offset = next_offset ok, then checked_add overflows.
        assert!(err.contains("overflows u64") || err.contains("exceed"), "got: {err}");
    }

    // ── Positive: valid cases ────────────────────────────────────────────

    #[test]
    fn accepts_valid_first_response_no_expected_size() {
        let r = resp("c", 0, 1024, vec![0u8; 1024]);
        validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap();
    }

    #[test]
    fn accepts_valid_first_response_matching_expected_size() {
        let r = resp("c", 0, 1024, vec![0u8; 1024]);
        validate_response_metadata(&r, 0, None, Some(1024), MAX_CHUNK_BYTES).unwrap();
    }

    #[test]
    fn accepts_valid_subsequent_response() {
        // First piece declared total = 1024 and delivered 512 bytes.
        // Second piece carries the remaining 512 at offset 512.
        let r = resp("c", 512, 1024, vec![0u8; 512]);
        validate_response_metadata(&r, 512, Some(1024), Some(1024), MAX_CHUNK_BYTES).unwrap();
    }

    #[test]
    fn accepts_total_bytes_at_safety_bound_exact() {
        let r = resp("c", 0, MAX_CHUNK_BYTES, vec![]);
        validate_response_metadata(&r, 0, None, None, MAX_CHUNK_BYTES).unwrap();
    }

    // ── Sanity: helper bound parameter is respected ──────────────────────

    #[test]
    fn safety_bound_can_be_tightened_by_caller() {
        // Caller passes a stricter bound than MAX_CHUNK_BYTES.
        let stricter = 4096;
        let r = resp("c", 0, 8192, vec![]);
        let err = validate_response_metadata(&r, 0, None, None, stricter).unwrap_err();
        assert!(err.contains("exceeds safety bound"), "got: {err}");
    }

    #[test]
    fn max_chunk_bytes_constant_matches_protocol_chunk_size() {
        // Sanity: the safety bound matches the protocol's CHUNK_SIZE.
        assert_eq!(MAX_CHUNK_BYTES, sum_types::storage::CHUNK_SIZE);
        assert_eq!(MAX_CHUNK_BYTES, 1_048_576);
    }

    // ── State-machine tests via MockFetchNet ────────────────────────────
    //
    // These exercise the FetchManager closed-loop state machine (Issue 4)
    // by injecting a mock FetchNet implementation. They prove:
    // - start_fetch records the CID as active
    // - on_chunk_received persists the chunk to disk on success
    // - the active set is cleared on success, failure, AND validation reject
    // - windowed (multi-piece) transfers issue follow-up requests via the trait

    use std::sync::Mutex as StdMutex;

    struct MockFetchNet {
        calls: StdMutex<Vec<(PeerId, String, Option<u64>, Option<u64>)>>,
    }
    impl MockFetchNet {
        fn new() -> Self {
            Self { calls: StdMutex::new(Vec::new()) }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl FetchNet for MockFetchNet {
        async fn request_shard_chunk(
            &self,
            peer_id: PeerId,
            cid: String,
            offset: Option<u64>,
            max_bytes: Option<u64>,
        ) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push((peer_id, cid, offset, max_bytes));
            Ok(())
        }
    }

    fn fake_peer_id() -> PeerId {
        // Generate a stable PeerId from a fresh keypair (test-only).
        sum_net::peer_id_from_keypair(&sum_net::Keypair::generate_ed25519())
    }

    fn tmp_chunk_store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn start_fetch_marks_active_via_mock() {
        let mock = MockFetchNet::new();
        let mut fm = FetchManager::new(64 * 1024);
        let peer = fake_peer_id();
        fm.start_fetch_with_expected_size(&mock, peer, "cidA".into(), Some(1024))
            .await
            .unwrap();

        assert!(fm.is_active("cidA"));
        assert_eq!(fm.active_count(), 1);
        // Mock should have observed exactly one initial windowed request.
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn on_chunk_received_completes_and_clears_active() {
        let (_dir, chunk_store) = tmp_chunk_store();
        let mock = MockFetchNet::new();
        let mut fm = FetchManager::new(64 * 1024);
        let peer = fake_peer_id();

        // Pick real bytes so the CID matches.
        let data = b"hello sum-store fetch state machine".to_vec();
        let cid = crate::content_id::cid_from_data(&data);
        let total = data.len() as u64;

        fm.start_fetch_with_expected_size(&mock, peer, cid.clone(), Some(total))
            .await
            .unwrap();
        assert!(fm.is_active(&cid));

        // Single piece carries the entire chunk.
        let response = ShardResponse {
            cid: cid.clone(),
            offset: 0,
            total_bytes: total,
            data: data.clone(),
            error: None,
        };
        let outcome = fm.on_chunk_received(&mock, &chunk_store, &response).await;

        match outcome {
            FetchOutcome::Complete { cid: got_cid, size: got_size } => {
                assert_eq!(got_cid, cid);
                assert_eq!(got_size, total);
            }
            other => panic!("expected Complete, got: {other:?}"),
        }
        assert!(!fm.is_active(&cid), "active set must be cleared on completion");
        assert!(chunk_store.has(&cid), "chunk must be persisted to disk");
        // No follow-up requests since the single piece covered total_bytes.
        assert_eq!(mock.call_count(), 1, "no follow-up requests expected");
    }

    #[tokio::test]
    async fn on_chunk_received_failed_clears_active() {
        let (_dir, chunk_store) = tmp_chunk_store();
        let mock = MockFetchNet::new();
        let mut fm = FetchManager::new(64 * 1024);
        let peer = fake_peer_id();

        fm.start_fetch_with_expected_size(&mock, peer, "cidF".into(), Some(1024))
            .await
            .unwrap();
        assert!(fm.is_active("cidF"));

        // Peer returns an error.
        let response = ShardResponse {
            cid: "cidF".into(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some("disk read failed".into()),
        };
        let outcome = fm.on_chunk_received(&mock, &chunk_store, &response).await;

        assert!(matches!(outcome, FetchOutcome::Failed { .. }));
        assert!(!fm.is_active("cidF"), "active set must be cleared on failure");
        assert!(!chunk_store.has("cidF"), "no chunk should be persisted on failure");
    }

    #[tokio::test]
    async fn on_chunk_received_invalid_metadata_clears_active() {
        // Joint regression test for Issue 3 + Issue 4: a peer that lies about
        // total_bytes must (a) be rejected by validate_response_metadata, AND
        // (b) leave the active set in a clean state.
        let (_dir, chunk_store) = tmp_chunk_store();
        let mock = MockFetchNet::new();
        let mut fm = FetchManager::new(64 * 1024);
        let peer = fake_peer_id();

        fm.start_fetch_with_expected_size(&mock, peer, "cidX".into(), Some(1024))
            .await
            .unwrap();
        assert!(fm.is_active("cidX"));

        // Peer claims total_bytes = 2048 but expected_size = 1024.
        let response = ShardResponse {
            cid: "cidX".into(),
            offset: 0,
            total_bytes: 2048,
            data: vec![0u8; 1024],
            error: None,
        };
        let outcome = fm.on_chunk_received(&mock, &chunk_store, &response).await;

        // The validation may reject with either "expected size" or
        // "exceeds safety bound" depending on which check fires first
        // (the helper computes max_total = min(MAX_CHUNK_BYTES, expected_size)).
        // Both are correct security outcomes; the important property is that
        // the fetch is rejected and the active set is cleaned up.
        match outcome {
            FetchOutcome::Failed { ref error, .. } => {
                assert!(
                    error.contains("expected size") || error.contains("exceeds safety bound"),
                    "expected validation rejection, got: {error}"
                );
            }
            other => panic!("expected Failed, got: {other:?}"),
        }
        assert!(!fm.is_active("cidX"), "active set must be cleared after validation reject");
        assert!(!chunk_store.has("cidX"));
    }

}
