//! Integration test for `UploadOrchestrator` memory boundedness.
//!
//! This test addresses Issue #2 by proving two properties of the upload path:
//!
//! 1. **Bounded in-flight work.** During an upload of N chunks with
//!    `max_in_flight_chunks = M` and `R` replicas, the peak number of
//!    concurrent push requests in flight never exceeds `M * R`. Memory
//!    usage is therefore $\mathcal{O}(M)$ in the number of unique chunk
//!    buffers held simultaneously, *independent* of N.
//!
//! 2. **Shared Arc identity per chunk.** All R replica pushes for a single
//!    chunk receive the *same* `Arc<[u8]>` backing pointer — no per-replica
//!    memcpy is performed. The orchestrator clones the `Arc` (cheap pointer
//!    bump), not the `Vec<u8>`.
//!
//! The test exercises the production `UploadOrchestrator::run_with_nodes`
//! through a `MockUploadNet` that records every push and synthesizes ACK
//! events. No real swarm or RPC is needed.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use sum_net::{Keypair, PeerId, ShardResponse, SumNetEvent};
use sum_net::{l1_address_from_keypair, peer_id_from_keypair};
use sum_node::rpc_client::L1RpcClient;
use sum_node::upload::{UploadNet, UploadOrchestrator};
use sum_store::SumStore;
use sum_types::config::StoreConfig;
use sum_types::storage::{ChunkDescriptor, DataManifest, REPLICATION_FACTOR};

// ── MockUploadNet ────────────────────────────────────────────────────────────

/// One observed push call.
#[derive(Clone)]
struct PushRecord {
    cid: String,
    peer_id: PeerId,
    /// Raw pointer to the underlying `[u8]` of the Arc — used to assert
    /// that all replicas of a chunk see the same backing buffer.
    data_ptr: usize,
}

/// Mock implementation of `UploadNet` that:
///
/// - Records every push (peer, cid, Arc pointer) for later assertions.
/// - Tracks peak concurrent in-flight pushes via an atomic counter.
/// - Synthesizes a successful `ShardReceived` ACK for every push and
///   queues it on `pending_acks`.
/// - `next_event()` pops from the ACK queue and decrements the in-flight
///   counter, simulating the round-trip.
struct MockUploadNet {
    pushes: Mutex<Vec<PushRecord>>,
    pending_acks: Mutex<VecDeque<SumNetEvent>>,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
}

impl MockUploadNet {
    fn new() -> Self {
        Self {
            pushes: Mutex::new(Vec::new()),
            pending_acks: Mutex::new(VecDeque::new()),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    fn record_in_flight(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        // Atomic max for the peak.
        let mut current_peak = self.peak_in_flight.load(Ordering::SeqCst);
        while now > current_peak {
            match self.peak_in_flight.compare_exchange(
                current_peak, now, Ordering::SeqCst, Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current_peak = actual,
            }
        }
    }

    fn release_in_flight(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl UploadNet for MockUploadNet {
    async fn push_chunk_shared(
        &self,
        peer_id: PeerId,
        cid: String,
        data: Arc<[u8]>,
    ) -> Result<()> {
        // Record the push (and the Arc pointer for sharing assertions).
        let data_ptr = data.as_ptr() as usize;
        self.record_in_flight();

        self.pushes.lock().await.push(PushRecord {
            cid: cid.clone(),
            peer_id,
            data_ptr,
        });

        // Synthesize a successful ACK and queue it for next_event() to drain.
        let ack = SumNetEvent::ShardReceived {
            peer_id,
            response: ShardResponse {
                cid,
                offset: 0,
                total_bytes: data.len() as u64,
                data: Vec::new(),
                error: None,
            },
        };
        self.pending_acks.lock().await.push_back(ack);
        Ok(())
    }

    async fn next_event(&self) -> Option<SumNetEvent> {
        let mut acks = self.pending_acks.lock().await;
        let event = acks.pop_front();
        if event.is_some() {
            self.release_in_flight();
        }
        event
    }
}

// ── Test fixture helpers ─────────────────────────────────────────────────────

fn random_peer() -> (PeerId, [u8; 20]) {
    let kp = Keypair::generate_ed25519();
    let pid = peer_id_from_keypair(&kp);
    let addr = l1_address_from_keypair(&kp);
    (pid, addr)
}

/// Build a SumStore in a tempdir, populate it with `n` synthetic chunks
/// (each `chunk_bytes` bytes long), and return the store, manifest, and
/// the temp dir guard.
fn build_synthetic_store(
    n_chunks: usize,
    chunk_bytes: usize,
) -> (tempfile::TempDir, SumStore, DataManifest) {
    let dir = tempfile::tempdir().unwrap();
    let config = StoreConfig {
        store_dir: dir.path().join("store"),
        max_chunk_msg_bytes: 64 * 1024 * 1024,
    };
    let store = SumStore::new(config).unwrap();

    let mut chunks: Vec<ChunkDescriptor> = Vec::with_capacity(n_chunks);
    let mut chunk_hashes: Vec<blake3::Hash> = Vec::with_capacity(n_chunks);

    for i in 0..n_chunks {
        // Make each chunk's bytes distinct so CIDs don't collide.
        let mut data = vec![0u8; chunk_bytes];
        for (j, b) in data.iter_mut().enumerate() {
            *b = ((i * 31 + j) & 0xff) as u8;
        }
        let hash = blake3::hash(&data);
        let cid = sum_store::cid_from_data(&data);
        store.local.put(&cid, &data).unwrap();

        chunks.push(ChunkDescriptor {
            chunk_index: i as u32,
            offset: (i * chunk_bytes) as u64,
            size: chunk_bytes as u64,
            blake3_hash: *hash.as_bytes(),
            cid,
        });
        chunk_hashes.push(hash);
    }

    let tree = sum_store::MerkleTree::build(&chunk_hashes);

    let manifest = DataManifest {
        file_name: "test.bin".into(),
        file_hash: [0u8; 32],
        total_size_bytes: (n_chunks * chunk_bytes) as u64,
        chunk_count: n_chunks as u32,
        merkle_root: *tree.root().as_bytes(),
        chunks,
    };

    (dir, store, manifest)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_peak_in_flight_is_bounded_by_max_in_flight_times_replication() {
    let n_chunks = 12;
    let max_in_flight = 2usize;
    let chunk_bytes = 256;

    let (_dir, store, manifest) = build_synthetic_store(n_chunks, chunk_bytes);

    // Build R distinct (PeerId, L1 address) pairs so the assignment finds
    // R replicas for every chunk.
    let n_peers = (REPLICATION_FACTOR as usize).max(4);
    let peers: Vec<(PeerId, [u8; 20])> = (0..n_peers).map(|_| random_peer()).collect();
    let peer_addresses: HashMap<PeerId, [u8; 20]> = peers.iter().copied().collect();
    let mut node_addrs: Vec<[u8; 20]> = peers.iter().map(|(_, a)| *a).collect();
    node_addrs.sort();

    let mock = MockUploadNet::new();
    let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
    let orchestrator = UploadOrchestrator::new(rpc, Duration::from_secs(30))
        .with_max_in_flight_chunks(max_in_flight);

    let result = orchestrator
        .run_with_nodes(&mock, &store, &manifest, &peer_addresses, &node_addrs)
        .await
        .expect("upload should succeed");

    // ── Assertion 1: every push was confirmed.
    assert_eq!(
        result.confirmed as usize,
        n_chunks * REPLICATION_FACTOR as usize,
        "expected R*n confirmations"
    );
    assert_eq!(result.failed.len(), 0, "no failures expected");
    assert!(!result.timeout);

    // ── Assertion 2: peak in-flight <= max_in_flight_chunks * R.
    //
    // The orchestrator processes one slice at a time and drains ACKs before
    // moving on, so the upper bound is `max_in_flight * R`.
    let peak = mock.peak_in_flight.load(Ordering::SeqCst);
    let bound = max_in_flight * REPLICATION_FACTOR as usize;
    assert!(
        peak <= bound,
        "peak in-flight {peak} exceeds bound {bound} (max_in_flight={max_in_flight}, R={REPLICATION_FACTOR})"
    );
    // Sanity: with multiple chunks, peak should be > 0.
    assert!(peak > 0, "peak in-flight should be > 0 for a non-empty upload");

    // ── Assertion 3: shared Arc identity per chunk.
    //
    // Group recorded pushes by CID and check all R replicas saw the same
    // backing pointer (proves no per-replica buffer copy).
    let pushes = mock.pushes.lock().await.clone();
    assert_eq!(
        pushes.len(),
        n_chunks * REPLICATION_FACTOR as usize,
        "expected R*n total pushes"
    );

    let mut by_cid: HashMap<String, Vec<usize>> = HashMap::new();
    for p in &pushes {
        by_cid.entry(p.cid.clone()).or_default().push(p.data_ptr);
    }
    assert_eq!(by_cid.len(), n_chunks, "should have entries for every chunk");
    for (cid, ptrs) in &by_cid {
        assert_eq!(
            ptrs.len(),
            REPLICATION_FACTOR as usize,
            "chunk {cid} should have R recorded pushes"
        );
        let first = ptrs[0];
        for &p in &ptrs[1..] {
            assert_eq!(
                p, first,
                "all R replica pushes for chunk {cid} must share the same Arc<[u8]> backing pointer"
            );
        }
    }
}

#[tokio::test]
async fn upload_with_max_in_flight_one_still_completes() {
    // Edge case: max_in_flight = 1 forces fully sequential per-chunk processing.
    let n_chunks = 5;
    let chunk_bytes = 128;
    let (_dir, store, manifest) = build_synthetic_store(n_chunks, chunk_bytes);

    let peers: Vec<(PeerId, [u8; 20])> = (0..4).map(|_| random_peer()).collect();
    let peer_addresses: HashMap<PeerId, [u8; 20]> = peers.iter().copied().collect();
    let mut node_addrs: Vec<[u8; 20]> = peers.iter().map(|(_, a)| *a).collect();
    node_addrs.sort();

    let mock = MockUploadNet::new();
    let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
    let orchestrator = UploadOrchestrator::new(rpc, Duration::from_secs(30))
        .with_max_in_flight_chunks(1);

    let result = orchestrator
        .run_with_nodes(&mock, &store, &manifest, &peer_addresses, &node_addrs)
        .await
        .expect("upload should succeed");

    assert_eq!(result.confirmed as usize, n_chunks * REPLICATION_FACTOR as usize);
    let peak = mock.peak_in_flight.load(Ordering::SeqCst);
    assert!(peak <= REPLICATION_FACTOR as usize, "peak {peak} > R");
}

#[tokio::test]
async fn upload_empty_manifest_succeeds() {
    let (_dir, store, manifest) = build_synthetic_store(0, 0);

    let peers: Vec<(PeerId, [u8; 20])> = (0..4).map(|_| random_peer()).collect();
    let peer_addresses: HashMap<PeerId, [u8; 20]> = peers.iter().copied().collect();
    let mut node_addrs: Vec<[u8; 20]> = peers.iter().map(|(_, a)| *a).collect();
    node_addrs.sort();

    let mock = MockUploadNet::new();
    let rpc = Arc::new(L1RpcClient::new("http://invalid".into()));
    let orchestrator = UploadOrchestrator::new(rpc, Duration::from_secs(5));

    let result = orchestrator
        .run_with_nodes(&mock, &store, &manifest, &peer_addresses, &node_addrs)
        .await
        .expect("empty upload should succeed");

    assert_eq!(result.confirmed, 0);
    assert_eq!(result.total, 0);
    assert!(!result.timeout);
    assert!(result.failed.is_empty());
}
