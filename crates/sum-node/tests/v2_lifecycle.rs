//! W12 — V2 lifecycle integration tests.
//!
//! These tests drive the **public** `V2Dispatcher::handle()` entry
//! point — the one production wires into `SumNetEvent::ShardRequestedV2`
//! arms in `main.rs run_listen` ([main.rs §V2 dispatch arm]). Unit
//! tests in `inbound_v2.rs` cover the internal building blocks
//! (`build_pull_response` / `build_manifest_pull_response` /
//! validator+store side effects). What's missing is end-to-end
//! coverage of `dispatcher.handle(net, peer, request, channel_id)`
//! itself: it's the function that wires validate → persist → respond
//! and emits a typed `ShardResponseV2` back through the swarm.
//!
//! These integration tests use a `CapturingNet` that implements the
//! production `RespondNet` trait so we can assert on the response
//! shape without standing up two libp2p swarms. Same mock-based
//! pattern as `tests/upload_bounded.rs` and `tests/market_sync_dispatch.rs`.
//!
//! ## What W12 covers vs. unit tests
//!
//! | Path                                         | Unit (W4/W5) | W12 |
//! |----------------------------------------------|:------------:|:---:|
//! | `validate_push` correctness                  | ✓            |     |
//! | `validate_manifest_push` correctness         | ✓            |     |
//! | `build_pull_response` (deny+allow)           | ✓            |     |
//! | `dispatcher.handle()` Push end-to-end        |              | ✓   |
//! | `dispatcher.handle()` ManifestPush async-ack |              | ✓   |
//! | `dispatcher.handle()` ManifestPull ACL       |              | ✓   |

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use sum_net::{Keypair, PeerId, ShardRequestV2, ShardResponseV2, l1_address_base58};
use sum_node::assignment_attestor::{AssignmentAttestor, AttestorRpc};
use sum_node::inbound_v2::{AccessChecker, AttestTriggerRpc, FileShape, RespondNet, V2Dispatcher};
use sum_node::push_validator::{PushValidator, V2Params, V2RpcClient};
use sum_node::tx_wait::TxStatusSource;
use sum_store::SumStore;
use sum_store::manifest_index::ManifestIndex;
use sum_store::merkle::MerkleTree;
use sum_types::config::StoreConfig;
use sum_types::rpc_types::{
    LifecycleV2, NodeRecordInfo, StorageFileInfoV2, TxStatusV2, VisibilityV2,
};
use sum_types::storage::{ChunkDescriptor, DataManifest};
use tokio::sync::RwLock;

// ── Test harness ────────────────────────────────────────────────────────────

/// Captures responses sent via `respond_shard_v2`. Tests assert on the
/// captured payload after `dispatcher.handle()` returns.
#[derive(Default)]
struct CapturingNet {
    responses: StdMutex<HashMap<u64, ShardResponseV2>>,
}

impl CapturingNet {
    fn new() -> Self {
        Self::default()
    }
    fn take(&self, channel_id: u64) -> Option<ShardResponseV2> {
        self.responses.lock().unwrap().remove(&channel_id)
    }
}

#[async_trait]
impl RespondNet for CapturingNet {
    async fn respond_shard_v2(&self, channel_id: u64, response: ShardResponseV2) -> Result<()> {
        self.responses.lock().unwrap().insert(channel_id, response);
        Ok(())
    }
}

/// Combined mock for V2RpcClient + AttestorRpc + AttestTriggerRpc +
/// TxStatusSource. Mirrors the inbound_v2 tests' `AllMockRpc`.
#[derive(Default)]
struct MockRpc {
    files: StdMutex<HashMap<String, StorageFileInfoV2>>,
    snapshots: StdMutex<HashMap<u64, Vec<NodeRecordInfo>>>,
    decoded_snapshots: StdMutex<HashMap<u64, Vec<[u8; 20]>>>,
    send_responses: StdMutex<VecDeque<Result<String, String>>>,
    status_responses: StdMutex<VecDeque<Result<TxStatusV2, String>>>,
    nonce_responses: StdMutex<HashMap<String, u64>>,
    sent_txs: StdMutex<Vec<String>>,
}

impl MockRpc {
    fn add_file(&self, root_hex: &str, info: StorageFileInfoV2) {
        self.files
            .lock()
            .unwrap()
            .insert(root_hex.to_string(), info);
    }
    fn add_snapshot(&self, height: u64, nodes: Vec<NodeRecordInfo>, decoded: Vec<[u8; 20]>) {
        self.snapshots.lock().unwrap().insert(height, nodes);
        self.decoded_snapshots
            .lock()
            .unwrap()
            .insert(height, decoded);
    }
    fn enqueue_send(&self, hash: &str) {
        self.send_responses
            .lock()
            .unwrap()
            .push_back(Ok(hash.to_string()));
    }
    fn enqueue_status(&self, st: TxStatusV2) {
        self.status_responses.lock().unwrap().push_back(Ok(st));
    }
    fn set_nonce(&self, addr: &str, nonce: u64) {
        self.nonce_responses
            .lock()
            .unwrap()
            .insert(addr.to_string(), nonce);
    }
}

#[async_trait]
impl V2RpcClient for MockRpc {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>> {
        // Missing root → clean not-found (Ok(None)); present → Ok(Some).
        Ok(self.files.lock().unwrap().get(merkle_root_hex).cloned())
    }
    async fn storage_get_active_nodes_at_height(&self, height: u64) -> Result<Vec<NodeRecordInfo>> {
        self.snapshots
            .lock()
            .unwrap()
            .get(&height)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no snapshot at height {height}"))
    }
}

#[async_trait]
impl TxStatusSource for MockRpc {
    async fn get_transaction_status(&self, _tx_hash: &str) -> Result<TxStatusV2> {
        let next = self
            .status_responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no status response queued"))?;
        next.map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl AttestorRpc for MockRpc {
    async fn send_raw_transaction(&self, hex: &str) -> Result<String> {
        self.sent_txs.lock().unwrap().push(hex.to_string());
        let next = self
            .send_responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no send response queued"))?;
        next.map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl AttestTriggerRpc for MockRpc {
    async fn fetch_file_shape(&self, merkle_root: &[u8; 32]) -> Result<FileShape> {
        let key = format!("0x{}", hex::encode(merkle_root));
        let info = self
            .files
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("trigger: unknown root"))?;
        Ok(FileShape {
            chunk_count: info.chunk_count,
            assignment_height: info.assignment_height,
            visibility: info.visibility,
        })
    }
    async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>> {
        self.decoded_snapshots
            .lock()
            .unwrap()
            .get(&height)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("trigger: no snapshot"))
    }
    async fn fetch_nonce(&self, address_base58: &str) -> Result<u64> {
        self.nonce_responses
            .lock()
            .unwrap()
            .get(address_base58)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("trigger: no nonce for {address_base58}"))
    }
}

/// Newtype around `Arc<MockRpc>` so we can implement V2RpcClient,
/// AttestorRpc, etc. for it without orphan-rule issues.
#[derive(Clone)]
struct ArcRpc(Arc<MockRpc>);

#[async_trait]
impl V2RpcClient for ArcRpc {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>> {
        self.0.storage_get_file_info_v2(merkle_root_hex).await
    }
    async fn storage_get_active_nodes_at_height(&self, height: u64) -> Result<Vec<NodeRecordInfo>> {
        self.0.storage_get_active_nodes_at_height(height).await
    }
}

#[async_trait]
impl TxStatusSource for ArcRpc {
    async fn get_transaction_status(&self, tx_hash: &str) -> Result<TxStatusV2> {
        self.0.get_transaction_status(tx_hash).await
    }
}

#[async_trait]
impl AttestorRpc for ArcRpc {
    async fn send_raw_transaction(&self, hex: &str) -> Result<String> {
        self.0.send_raw_transaction(hex).await
    }
}

#[derive(Clone)]
struct ArcRpcTrigger(Arc<MockRpc>);

#[async_trait]
impl AttestTriggerRpc for ArcRpcTrigger {
    async fn fetch_file_shape(&self, root: &[u8; 32]) -> Result<FileShape> {
        self.0.fetch_file_shape(root).await
    }
    async fn fetch_snapshot(&self, height: u64) -> Result<Vec<[u8; 20]>> {
        self.0.fetch_snapshot(height).await
    }
    async fn fetch_nonce(&self, address_base58: &str) -> Result<u64> {
        self.0.fetch_nonce(address_base58).await
    }
}

/// Toggle ACL — denies or allows all checks based on a single flag.
struct ToggleAcl {
    allow_default: bool,
}

#[async_trait]
impl AccessChecker for ToggleAcl {
    async fn check_access_or_default(
        &self,
        _peer_id: &PeerId,
        _cid: &str,
        _manifest_index: &ManifestIndex,
    ) -> bool {
        self.allow_default
    }
}

// ── Fixture builders ────────────────────────────────────────────────────────

fn five_archives() -> Vec<[u8; 20]> {
    (0..5)
        .map(|i| {
            let mut a = [0u8; 20];
            a[0] = 0xAA + i;
            a
        })
        .collect()
}

fn node_record(addr: &[u8; 20]) -> NodeRecordInfo {
    NodeRecordInfo {
        address: l1_address_base58(addr),
        role: "ArchiveNode".into(),
        staked_balance: 1_000_000_000,
        status: "Active".into(),
        registered_at: 1,
    }
}

fn file_info_active(
    root: &[u8; 32],
    chunk_count: u32,
    assignment_height: u64,
) -> StorageFileInfoV2 {
    StorageFileInfoV2 {
        merkle_root: format!("0x{}", hex::encode(root)),
        owner: l1_address_base58(&[0x01; 20]),
        plaintext_size_bytes: 1024,
        stored_size_bytes: 1024,
        chunk_count,
        fee_pool: 1000,
        created_at: 100,
        activated_at_height: Some(150),
        abandoned_at_height: None,
        assignment_height,
        visibility: VisibilityV2::PUBLIC,
        lifecycle: LifecycleV2::ACTIVE,
        access_list: vec![],
    }
}

fn make_manifest(n: u32, salt: u8) -> (DataManifest, Vec<Vec<u8>>, MerkleTree) {
    let chunks: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut v = vec![salt; 64];
            v.extend_from_slice(&i.to_le_bytes());
            v
        })
        .collect();
    let leaves: Vec<blake3::Hash> = chunks.iter().map(|c| blake3::hash(c)).collect();
    let tree = MerkleTree::build(&leaves);
    let mut offset = 0u64;
    let descriptors: Vec<ChunkDescriptor> = leaves
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let size = chunks[i].len() as u64;
            let d = ChunkDescriptor {
                chunk_index: i as u32,
                offset,
                size,
                blake3_hash: *h.as_bytes(),
                cid: sum_store::content_id::cid_from_blake3_hash(h),
                plaintext_blake3_hash: None,
            };
            offset += size;
            d
        })
        .collect();
    let total_size: u64 = chunks.iter().map(|c| c.len() as u64).sum();
    let manifest = DataManifest {
        file_name: format!("v2-int-{salt:02x}.bin"),
        file_hash: *blake3::hash(&chunks.concat()).as_bytes(),
        total_size_bytes: total_size,
        chunk_count: n,
        merkle_root: *tree.root().as_bytes(),
        chunks: descriptors,
    };
    (manifest, chunks, tree)
}

fn make_store() -> Arc<RwLock<SumStore>> {
    let temp_dir = tempfile::tempdir().unwrap();
    let store_dir = temp_dir.path().join("store");
    std::fs::create_dir_all(&store_dir).unwrap();
    let cfg = StoreConfig {
        store_dir,
        ..StoreConfig::default()
    };
    let store = SumStore::new(cfg).expect("SumStore::new");
    std::mem::forget(temp_dir);
    Arc::new(RwLock::new(store))
}

fn fake_peer() -> PeerId {
    Keypair::generate_ed25519().public().to_peer_id()
}

/// Build a V2Dispatcher wired through the Arc-typed mocks.
struct ArchiveFixture {
    dispatcher: V2Dispatcher<ArcRpc, ArcRpc, ArcRpcTrigger>,
    rpc: Arc<MockRpc>,
    store: Arc<RwLock<SumStore>>,
    snapshot: Vec<[u8; 20]>,
    my_addr: [u8; 20],
}

fn build_archive(acl_allows: bool) -> ArchiveFixture {
    let snapshot = five_archives();
    let my_addr = snapshot[0];
    let rpc = Arc::new(MockRpc::default());
    let store = make_store();

    let validator = Arc::new(PushValidator::new(
        ArcRpc(rpc.clone()),
        my_addr,
        V2Params::DEFAULTS,
    ));
    let attestor = Arc::new(AssignmentAttestor::new(
        ArcRpc(rpc.clone()),
        [42u8; 32],
        my_addr,
        1337,
        1_000_000,
        V2Params {
            assignment_replication_factor: 5,
            max_chunk_indices_per_tx: 64,
        },
    ));
    let trigger = Arc::new(ArcRpcTrigger(rpc.clone()));
    let acl: Arc<dyn AccessChecker> = Arc::new(ToggleAcl {
        allow_default: acl_allows,
    });
    let dispatcher = V2Dispatcher::new(validator, attestor, trigger, store.clone(), acl, my_addr)
        .with_attest_timing(Duration::from_millis(10), Duration::from_secs(2));

    ArchiveFixture {
        dispatcher,
        rpc,
        store,
        snapshot,
        my_addr,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// **W12-1**: V2 Push request → dispatcher.handle() → real PushValidator
/// (consults file_info+snapshot+assignment_v2+strict-Merkle) →
/// ChunkStore::put → CapturingNet receives `PushAck { error: None }`.
#[tokio::test]
async fn v2_push_handle_validates_persists_and_acks() {
    let arch = build_archive(true);
    let (manifest, chunks, tree) = make_manifest(8, 0xA0);
    let root = manifest.merkle_root;
    let assignment_height = 500;

    arch.rpc.add_file(
        &format!("0x{}", hex::encode(root)),
        StorageFileInfoV2 {
            chunk_count: manifest.chunk_count,
            assignment_height,
            lifecycle: LifecycleV2::ACTIVE,
            ..file_info_active(&root, manifest.chunk_count, assignment_height)
        },
    );
    arch.rpc.add_snapshot(
        assignment_height,
        arch.snapshot.iter().map(node_record).collect(),
        arch.snapshot.clone(),
    );

    // Find a chunk_index assigned to my_addr under R=3 (the default R
    // PushValidator will use).
    let r = V2Params::DEFAULTS.assignment_replication_factor;
    let mut good = None;
    for i in 0..manifest.chunk_count {
        let assigned = sum_store::assignment_v2::assigned_archives(&root, &arch.snapshot, i, r);
        if assigned.contains(&arch.my_addr) {
            good = Some(i);
            break;
        }
    }
    let i = good.expect("at least one chunk assigned to my_addr");
    let proof = tree.proof_bytes(i);

    let net = CapturingNet::new();
    let request = ShardRequestV2::Push {
        data: chunks[i as usize].clone(),
        merkle_root: root,
        chunk_index: i,
        merkle_path: proof,
    };
    let channel_id = 42;
    arch.dispatcher
        .handle(&net, fake_peer(), request, channel_id)
        .await;

    // Verify response is PushAck { error: None }.
    let resp = net.take(channel_id).expect("response captured");
    match resp {
        ShardResponseV2::PushAck {
            merkle_root: r,
            chunk_index,
            error,
        } => {
            assert_eq!(r, root);
            assert_eq!(chunk_index, i);
            assert!(error.is_none(), "expected no error, got: {error:?}");
        }
        other => panic!("expected PushAck, got {other:?}"),
    }
    // Verify chunk persisted under cid_from_blake3_hash(leaf_hash).
    let leaf = blake3::hash(&chunks[i as usize]);
    let cid = sum_store::content_id::cid_from_blake3_hash(&leaf);
    assert!(
        arch.store.read().await.local.has(&cid),
        "chunk must be persisted in the local store"
    );
    // Held tracker recorded the chunk_index for the root.
    let held: BTreeSet<u32> = arch.dispatcher.held_for(&root);
    assert!(held.contains(&i), "held tracker must record validated push");
}

/// **W12-2**: V2 Push with tampered Merkle path → dispatcher.handle()
/// → PushValidator returns BadProof → CapturingNet receives `PushAck
/// { error: Some(_) }`. The rejection must come back as a typed
/// PushAck (not a libp2p timeout / dropped connection).
#[tokio::test]
async fn v2_push_handle_bad_proof_returns_typed_pushack_error() {
    let arch = build_archive(true);
    let (manifest, chunks, tree) = make_manifest(8, 0xB0);
    let root = manifest.merkle_root;
    let assignment_height = 600;

    arch.rpc.add_file(
        &format!("0x{}", hex::encode(root)),
        StorageFileInfoV2 {
            chunk_count: manifest.chunk_count,
            assignment_height,
            lifecycle: LifecycleV2::ACTIVE,
            ..file_info_active(&root, manifest.chunk_count, assignment_height)
        },
    );
    arch.rpc.add_snapshot(
        assignment_height,
        arch.snapshot.iter().map(node_record).collect(),
        arch.snapshot.clone(),
    );

    let r = V2Params::DEFAULTS.assignment_replication_factor;
    let mut good = None;
    for i in 0..manifest.chunk_count {
        let assigned = sum_store::assignment_v2::assigned_archives(&root, &arch.snapshot, i, r);
        if assigned.contains(&arch.my_addr) {
            good = Some(i);
            break;
        }
    }
    let i = good.unwrap();

    // Tamper one byte of the proof so verification fails.
    let mut bad_proof = tree.proof_bytes(i);
    if !bad_proof.is_empty() {
        bad_proof[0][0] ^= 0xFF;
    } else {
        bad_proof = vec![[0xFF; 32]];
    }

    let net = CapturingNet::new();
    let request = ShardRequestV2::Push {
        data: chunks[i as usize].clone(),
        merkle_root: root,
        chunk_index: i,
        merkle_path: bad_proof,
    };
    arch.dispatcher.handle(&net, fake_peer(), request, 7).await;

    let resp = net.take(7).expect("response captured");
    match resp {
        ShardResponseV2::PushAck {
            merkle_root: r,
            chunk_index,
            error,
        } => {
            assert_eq!(r, root);
            assert_eq!(chunk_index, i);
            let err = error.expect("bad proof must surface as typed PushAck error");
            assert!(
                err.contains("Merkle proof failed verification") || err.contains("BadProof"),
                "got: {err}"
            );
        }
        other => panic!("expected PushAck, got {other:?}"),
    }
    // Chunk MUST NOT be persisted on a rejected push.
    let leaf = blake3::hash(&chunks[i as usize]);
    let cid = sum_store::content_id::cid_from_blake3_hash(&leaf);
    assert!(
        !arch.store.read().await.local.has(&cid),
        "rejected push must not persist the chunk"
    );
}

/// **W12-3**: V2 ManifestPull with ACL denied → dispatcher.handle()
/// → ACL gate fires → CapturingNet receives `ManifestData { error:
/// Some("ACCESS_DENIED..."), manifest_bytes: empty }`. Verifies the W5
/// ACL gate over the production handle() entry point.
#[tokio::test]
async fn v2_manifest_pull_handle_acl_denied_returns_access_denied() {
    let arch = build_archive(/* acl_allows = */ false);
    let (manifest, _chunks, _tree) = make_manifest(4, 0xC0);
    let root = manifest.merkle_root;

    // Seed the manifest into the store so we can prove the ACL gate
    // (not "manifest absent") is what blocks the response.
    {
        let mut s = arch.store.write().await;
        s.manifest_idx.insert(&manifest).unwrap();
    }
    assert!(
        arch.store
            .read()
            .await
            .manifest_idx
            .get_by_merkle_root(&root)
            .is_some()
    );

    let net = CapturingNet::new();
    let request = ShardRequestV2::ManifestPull { merkle_root: root };
    arch.dispatcher.handle(&net, fake_peer(), request, 99).await;

    let resp = net.take(99).expect("response captured");
    match resp {
        ShardResponseV2::ManifestData {
            merkle_root: r,
            manifest_bytes,
            error,
        } => {
            assert_eq!(r, root);
            assert!(
                manifest_bytes.is_empty(),
                "denied ManifestPull must not carry CBOR bytes"
            );
            let err = error.expect("denied pull must set error");
            assert!(err.contains("ACCESS_DENIED"), "got: {err}");
        }
        other => panic!("expected ManifestData, got {other:?}"),
    }
}

/// **W12-4 (locked-decision regression):** V2 `ManifestPushAck` must
/// arrive before the spawned `AcceptAssignmentV2` attestation
/// finalizes. To make this a real regression we MUST:
///
///   1. Seed the dispatcher's held tracker with at least one chunk
///      that's actually in this archive's V2 assignment (via a real
///      `handle(Push)` call). With an empty held set, the spawned
///      attestation would early-exit and the test would pass even on
///      a sync-await bug.
///   2. Make attestation finality *impossible* within the test
///      window — we only enqueue a `Pending` tx status, which the
///      attestor's `wait_for_finalized` loops on indefinitely. If
///      `handle(ManifestPush)` synchronously awaited the spawned
///      attestation, the test would hang.
///
/// Then assert `dispatcher.handle(ManifestPush)` returns within a
/// tight `tokio::time::timeout` and the captured response is a clean
/// `ManifestPushAck { error: None }`.
#[tokio::test]
async fn v2_manifest_push_acks_before_attestation_completes() {
    let arch = build_archive(true);
    let (manifest, chunks, tree) = make_manifest(2, 0xD0);
    let root = manifest.merkle_root;
    let assignment_height = 700;

    // Pending lifecycle so V2 dispatcher's push-validation accepts.
    arch.rpc.add_file(
        &format!("0x{}", hex::encode(root)),
        StorageFileInfoV2 {
            chunk_count: manifest.chunk_count,
            assignment_height,
            lifecycle: LifecycleV2::PENDING,
            ..file_info_active(&root, manifest.chunk_count, assignment_height)
        },
    );
    arch.rpc.add_snapshot(
        assignment_height,
        arch.snapshot.iter().map(node_record).collect(),
        arch.snapshot.clone(),
    );
    arch.rpc.set_nonce(&l1_address_base58(&arch.my_addr), 1);

    // Seed held tracker with a real validated push.
    // Find a chunk_index assigned to my_addr under PushValidator's R.
    let r = V2Params::DEFAULTS.assignment_replication_factor;
    let mut good = None;
    for i in 0..manifest.chunk_count {
        let assigned = sum_store::assignment_v2::assigned_archives(&root, &arch.snapshot, i, r);
        if assigned.contains(&arch.my_addr) {
            good = Some(i);
            break;
        }
    }
    let chunk_idx = good.expect("at least one chunk assigned to my_addr");
    let push_proof = tree.proof_bytes(chunk_idx);
    let push_net = CapturingNet::new();
    arch.dispatcher
        .handle(
            &push_net,
            fake_peer(),
            ShardRequestV2::Push {
                data: chunks[chunk_idx as usize].clone(),
                merkle_root: root,
                chunk_index: chunk_idx,
                merkle_path: push_proof,
            },
            10,
        )
        .await;
    let push_resp = push_net.take(10).expect("push captured");
    match push_resp {
        ShardResponseV2::PushAck { error: None, .. } => (),
        other => panic!("seed push must succeed; got {other:?}"),
    }
    assert!(
        arch.dispatcher.held_for(&root).contains(&chunk_idx),
        "seed push must have populated the held tracker — without this the \
         spawned attestation would early-exit and the regression below would \
         pass even on a sync-await bug"
    );

    // Make attestation finality IMPOSSIBLE within the test:
    //   * `send_raw_transaction` accepts and returns a hash.
    //   * Every subsequent `get_transaction_status` returns `Pending`.
    // The attestor's `wait_for_finalized` loops on Pending until its
    // own `batch_timeout` (set to 2s by the dispatcher's
    // `with_attest_timing`) — which would block the dispatcher's
    // `handle()` if it synchronously awaited.
    arch.rpc.enqueue_send("0xtx-attest-stuck");
    for _ in 0..200 {
        arch.rpc.enqueue_status(TxStatusV2::Pending);
    }

    // CBOR-serialize the manifest.
    let mut manifest_bytes = Vec::new();
    ciborium::ser::into_writer(&manifest, &mut manifest_bytes).unwrap();

    // Tight wall-clock budget — handle() must return well below this.
    // If it synchronously awaited attestation finality, it would block
    // for the dispatcher's batch_timeout (2s) and trip this timeout.
    let net = CapturingNet::new();
    let request = ShardRequestV2::ManifestPush {
        merkle_root: root,
        manifest_bytes,
    };
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        arch.dispatcher.handle(&net, fake_peer(), request, 11),
    )
    .await;
    assert!(
        result.is_ok(),
        "ManifestPushAck must return promptly (decoupled from chain finality); \
         dispatcher.handle() exceeded 500ms wall-clock — sync-await regression"
    );

    let resp = net.take(11).expect("ManifestPushAck captured");
    match resp {
        ShardResponseV2::ManifestPushAck {
            merkle_root: r,
            error,
        } => {
            assert_eq!(r, root);
            assert!(
                error.is_none(),
                "ManifestPushAck must succeed before attestation finalizes \
                 (locked decision); got: {error:?}"
            );
        }
        other => panic!("expected ManifestPushAck, got {other:?}"),
    }

    // Attestation is still in-flight (looping on Pending). It would
    // eventually time out at the dispatcher's `batch_timeout = 2s`
    // — but our handle() already returned. Don't .await it; tokio
    // drops the spawned task at test teardown.
}
