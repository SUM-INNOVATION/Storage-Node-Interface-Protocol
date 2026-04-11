//! Integration tests for the closed-loop fetch state machine (Issue #4).
//!
//! These tests prove the bug fix end-to-end by exercising the **production**
//! `SumStore::fetcher` (`FetchManager`) the same way the listen-mode event
//! loop in `main.rs` does:
//!
//! 1. **MarketSync side:** call `store.fetcher.start_fetch_with_expected_size`
//!    against a mock `FetchNet` (the same call MarketSync now makes after the
//!    Issue 4 cleanup). This registers the CID as in-flight.
//!
//! 2. **Listen loop side:** synthesize a `ShardResponse` and feed it through
//!    `store.fetcher.on_chunk_received`. This is the exact code path the new
//!    `SumNetEvent::ShardReceived` arm in `run_listen` invokes.
//!
//! 3. **Assertions:** the chunk lands on disk via `ChunkStore::put`, the
//!    fetch is no longer reported `is_active`, and (for the windowed test)
//!    the mock observed the follow-up requests issued by the FetchManager.
//!
//! Together these prove that MarketSync-issued fetches now **complete** in
//! listen mode, closing the bug where they were silently dropped.

use std::sync::Mutex;

use async_trait::async_trait;
use sum_net::{Keypair, PeerId, ShardResponse};
use sum_net::peer_id_from_keypair;
use sum_store::{cid_from_data, FetchNet, FetchOutcome, SumStore};
use sum_types::config::StoreConfig;

// ── MockFetchNet ─────────────────────────────────────────────────────────────

/// Records every `request_shard_chunk` call so the test can assert that the
/// FetchManager issued the expected initial + follow-up requests.
struct MockFetchNet {
    calls: Mutex<Vec<(PeerId, String, Option<u64>, Option<u64>)>>,
}

impl MockFetchNet {
    fn new() -> Self {
        Self { calls: Mutex::new(Vec::new()) }
    }
    fn calls(&self) -> Vec<(PeerId, String, Option<u64>, Option<u64>)> {
        self.calls.lock().unwrap().clone()
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

fn random_peer_id() -> PeerId {
    peer_id_from_keypair(&Keypair::generate_ed25519())
}

fn build_store() -> (tempfile::TempDir, SumStore) {
    let dir = tempfile::tempdir().unwrap();
    let config = StoreConfig {
        store_dir: dir.path().join("store"),
        max_chunk_msg_bytes: 64 * 1024 * 1024,
    };
    let store = SumStore::new(config).unwrap();
    (dir, store)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Single-piece happy path: prove that a fetch issued via FetchManager and
/// completed via `on_chunk_received` actually persists the chunk to disk
/// and clears the in-flight set.
#[tokio::test]
async fn market_sync_dispatch_single_piece_completes_and_persists() {
    let (_dir, mut store) = build_store();
    let mock = MockFetchNet::new();
    let peer = random_peer_id();

    let chunk_data = b"closed-loop fetch state machine works".to_vec();
    let cid = cid_from_data(&chunk_data);
    let total = chunk_data.len() as u64;

    // ── 1. MarketSync side: register the fetch via FetchManager ──
    store.fetcher
        .start_fetch_with_expected_size(&mock, peer, cid.clone(), Some(total))
        .await
        .expect("start_fetch should succeed");

    assert!(store.fetcher.is_active(&cid), "CID should be marked in-flight");
    assert!(!store.local.has(&cid), "chunk should not yet be on disk");

    // The FetchManager issued one initial windowed request to the peer.
    let calls = mock.calls();
    assert_eq!(calls.len(), 1, "expected 1 initial request, got {}", calls.len());
    assert_eq!(calls[0].0, peer);
    assert_eq!(calls[0].1, cid);
    assert_eq!(calls[0].2, Some(0)); // offset = 0
    assert!(calls[0].3.is_some(),    "max_bytes should be set");

    // ── 2. Listen loop side: synthesize a ShardResponse carrying the
    //                        whole chunk in a single piece ──
    let response = ShardResponse {
        cid: cid.clone(),
        offset: 0,
        total_bytes: total,
        data: chunk_data.clone(),
        error: None,
    };

    let outcome = store.fetcher
        .on_chunk_received(&mock, &store.local, &response)
        .await;

    // ── 3. Assertions ──
    match outcome {
        FetchOutcome::Complete { cid: got_cid, size: got_size } => {
            assert_eq!(got_cid, cid);
            assert_eq!(got_size, total);
        }
        other => panic!("expected Complete, got: {other:?}"),
    }
    assert!(!store.fetcher.is_active(&cid), "in-flight set must be cleared on completion");
    assert!(store.local.has(&cid), "chunk must be persisted to disk");
    let stored = store.local.get(&cid).unwrap();
    assert_eq!(stored, chunk_data, "stored bytes must match the original");

    // No new follow-up requests issued (single piece covered total_bytes).
    assert_eq!(mock.calls().len(), 1, "no follow-up requests should have been issued");
}

/// Failure path: a peer that returns an error must clear in-flight state
/// and leave the disk untouched.
#[tokio::test]
async fn market_sync_dispatch_failure_clears_active_no_disk_write() {
    let (_dir, mut store) = build_store();
    let mock = MockFetchNet::new();
    let peer = random_peer_id();

    let cid = "bafkr4iexample".to_string();

    store.fetcher
        .start_fetch_with_expected_size(&mock, peer, cid.clone(), Some(1024))
        .await
        .expect("start_fetch should succeed");
    assert!(store.fetcher.is_active(&cid));

    // Peer signals an error.
    let response = ShardResponse {
        cid: cid.clone(),
        offset: 0,
        total_bytes: 0,
        data: Vec::new(),
        error: Some("simulated peer disk read failure".into()),
    };

    let outcome = store.fetcher
        .on_chunk_received(&mock, &store.local, &response)
        .await;

    match outcome {
        FetchOutcome::Failed { cid: got_cid, error } => {
            assert_eq!(got_cid, cid);
            assert!(error.contains("simulated"), "got: {error}");
        }
        other => panic!("expected Failed, got: {other:?}"),
    }

    assert!(!store.fetcher.is_active(&cid), "in-flight set must be cleared on failure");
    assert!(!store.local.has(&cid), "no chunk should be persisted on failure");
}

/// Windowed (multi-piece) path: prove that the FetchManager issues a
/// follow-up `request_shard_chunk` via the trait abstraction when the
/// first response is incomplete, and reaches `Complete` after the second.
#[tokio::test]
async fn market_sync_dispatch_windowed_transfer_completes_via_followup() {
    let (_dir, mut store) = build_store();

    // Force a small per-message window so a 2KB chunk needs 2 round-trips.
    let config = StoreConfig {
        store_dir: _dir.path().join("store2"),
        max_chunk_msg_bytes: 1024,
    };
    let mut store = SumStore::new(config).unwrap();

    let mock = MockFetchNet::new();
    let peer = random_peer_id();

    // 2 KB chunk → two pieces of 1 KB each.
    let mut chunk_data = Vec::with_capacity(2048);
    for i in 0..2048 {
        chunk_data.push((i & 0xff) as u8);
    }
    let cid = cid_from_data(&chunk_data);
    let total = chunk_data.len() as u64;

    store.fetcher
        .start_fetch_with_expected_size(&mock, peer, cid.clone(), Some(total))
        .await
        .expect("start_fetch should succeed");

    // After start_fetch: 1 initial request observed.
    assert_eq!(mock.calls().len(), 1);

    // ── First piece: bytes [0..1024). Should yield InProgress AND
    //                 cause the FetchManager to issue a follow-up. ──
    let piece1 = ShardResponse {
        cid: cid.clone(),
        offset: 0,
        total_bytes: total,
        data: chunk_data[0..1024].to_vec(),
        error: None,
    };
    let outcome1 = store.fetcher
        .on_chunk_received(&mock, &store.local, &piece1)
        .await;
    assert!(matches!(outcome1, FetchOutcome::InProgress), "first piece should be InProgress, got: {outcome1:?}");
    assert!(store.fetcher.is_active(&cid), "fetch must still be in-flight after partial piece");

    // FetchManager issued a follow-up request via the trait abstraction.
    let calls_after_piece1 = mock.calls();
    assert_eq!(calls_after_piece1.len(), 2, "expected 1 follow-up after piece 1");
    let followup = &calls_after_piece1[1];
    assert_eq!(followup.0, peer);
    assert_eq!(followup.1, cid);
    assert_eq!(followup.2, Some(1024), "follow-up offset should be 1024");

    // ── Second piece: bytes [1024..2048). Should yield Complete. ──
    let piece2 = ShardResponse {
        cid: cid.clone(),
        offset: 1024,
        total_bytes: total,
        data: chunk_data[1024..2048].to_vec(),
        error: None,
    };
    let outcome2 = store.fetcher
        .on_chunk_received(&mock, &store.local, &piece2)
        .await;

    match outcome2 {
        FetchOutcome::Complete { cid: got_cid, size: got_size } => {
            assert_eq!(got_cid, cid);
            assert_eq!(got_size, total);
        }
        other => panic!("expected Complete, got: {other:?}"),
    }
    assert!(!store.fetcher.is_active(&cid), "in-flight set must be cleared after final piece");
    assert!(store.local.has(&cid));
    let stored = store.local.get(&cid).unwrap();
    assert_eq!(stored.len(), 2048);
    assert_eq!(stored, chunk_data, "reassembled bytes must match the original");

    // No additional follow-ups after Complete.
    assert_eq!(mock.calls().len(), 2, "should be exactly 2 requests total (initial + 1 follow-up)");
}
