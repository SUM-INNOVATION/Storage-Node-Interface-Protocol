//! WP-S1 regression: the V1 chunk-pull range is reachable on a PUBLIC
//! file through the production ACL, and a malformed range no longer
//! terminates the handler.
//!
//! This mirrors the pull dispatch in `sum-node/src/main.rs` (`run_listen`):
//! the production `AclChecker` decides first, and only then is
//! `sum_store::serve::handle_request` invoked. The point of the fixture is
//! that the ACL says *yes* for a public file — so the range arithmetic is
//! reachable by any peer that knows a CID this node serves, with no
//! access-list membership — and that the malformed request is answered
//! rather than killing the serving loop.
//!
//! The chain is stubbed with `httpmock` (already a dev-dependency here for
//! wire-shape tests); no live network or real chain is involved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use httpmock::prelude::*;
use sum_net::{PeerId, ShardRequest, ShardResponse};
use sum_node::acl::AclChecker;
use sum_node::profile::NodeProfile;
use sum_node::rpc_client::L1RpcClient;
use sum_store::ManifestIndex;
use sum_store::merkle::MerkleTree;
use sum_store::serve::{self, RespondShard};
use sum_store::store::ChunkStore;
use sum_types::rpc_types::{LifecycleV2, StorageFileInfoV2, VisibilityV2};
use sum_types::storage::{ChunkDescriptor, DataManifest};
use tokio::sync::RwLock;

const CHUNK: &[u8] = b"abcd";

/// Capturing responder — the handlers' only outbound edge.
#[derive(Default)]
struct RecorderNet {
    sent: Mutex<Vec<(u64, ShardResponse)>>,
}

#[async_trait::async_trait]
impl RespondShard for RecorderNet {
    async fn respond_shard(&self, channel_id: u64, response: ShardResponse) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((channel_id, response));
        Ok(())
    }
}

impl RecorderNet {
    fn take(&self) -> Vec<(u64, ShardResponse)> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

/// A one-chunk public file: the store holds the bytes, and the manifest
/// index maps its CID to the file's merkle root so the ACL can resolve it.
fn fixture() -> (tempfile::TempDir, ChunkStore, ManifestIndex, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::new(dir.path().join("chunks")).unwrap();
    let cid = sum_store::content_id::cid_from_data(CHUNK);
    store.put(&cid, CHUNK).unwrap();

    let hash = blake3::hash(CHUNK);
    let root = *MerkleTree::build(&[hash]).root().as_bytes();
    let manifest = DataManifest {
        merkle_root: root,
        file_name: "public.bin".to_string(),
        file_hash: root,
        total_size_bytes: CHUNK.len() as u64,
        chunk_count: 1,
        chunks: vec![ChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: CHUNK.len() as u64,
            blake3_hash: *hash.as_bytes(),
            cid: cid.clone(),
            plaintext_blake3_hash: None,
        }],
    };
    let mut idx = ManifestIndex::load(dir.path()).unwrap();
    idx.insert(&manifest).unwrap();

    let root_hex = format!("0x{}", hex::encode(root));
    (dir, store, idx, cid, root_hex)
}

/// Stub chain serving one PUBLIC V2 file row for `root_hex`.
fn public_file_chain(server: &MockServer, root_hex: &str) {
    let info = StorageFileInfoV2 {
        merkle_root: root_hex.to_string(),
        owner: "PublicOwner".into(),
        plaintext_size_bytes: CHUNK.len() as u64,
        stored_size_bytes: CHUNK.len() as u64,
        chunk_count: 1,
        fee_pool: 0,
        created_at: 1,
        activated_at_height: Some(1),
        abandoned_at_height: None,
        assignment_height: 1,
        visibility: VisibilityV2::PUBLIC,
        lifecycle: LifecycleV2::ACTIVE,
        // A Public file is open-read; the empty list is what production
        // rows for public files carry.
        access_list: vec![],
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": serde_json::to_value(&info).unwrap(),
    });
    server.mock(|when, then| {
        when.method(POST).body_contains("storage_getFileInfoV2");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(body);
    });
}

#[tokio::test]
async fn public_file_acl_allows_then_malformed_range_is_answered_and_handler_survives() {
    let (_dir, store, idx, cid, root_hex) = fixture();
    let server = MockServer::start_async().await;
    public_file_chain(&server, &root_hex);

    let acl = AclChecker::new(
        Arc::new(L1RpcClient::new(server.base_url())),
        // Empty identify map: a Public file must not require resolving the
        // peer's L1 address at all. If this fixture ever starts depending
        // on it, the assertion below fails rather than silently passing.
        Arc::new(RwLock::new(HashMap::new())),
        NodeProfile::Production,
    );
    let peer = PeerId::random();

    // ── The ACL fixture: production profile, unknown peer, public file. ──
    assert!(
        acl.check_access_or_default(&peer, &cid, &idx).await,
        "a Public V2 file must be readable by any peer — this is what makes \
         the range arithmetic below remotely reachable"
    );

    let net = RecorderNet::default();

    // ── 1. The request that used to terminate the serving loop. ─────────
    let malicious = ShardRequest {
        cid: cid.clone(),
        offset: Some(1),
        max_bytes: Some(u64::MAX),
        push_data: None,
    };
    serve::handle_request(&net, &store, &idx, &malicious, 21).await;
    let sent = net.take();
    assert_eq!(sent.len(), 1, "the malformed request is answered");
    let (channel, resp) = &sent[0];
    assert_eq!(*channel, 21);
    assert_eq!(resp.cid, cid);
    assert_eq!(resp.offset, 1);
    assert_eq!(resp.total_bytes, CHUNK.len() as u64);
    assert_eq!(resp.data, b"bcd");
    assert!(resp.error.is_none());

    // ── 2. A valid request afterwards, on the same handler and store. ───
    assert!(acl.check_access_or_default(&peer, &cid, &idx).await);
    let valid = ShardRequest {
        cid: cid.clone(),
        offset: Some(0),
        max_bytes: Some(2),
        push_data: None,
    };
    serve::handle_request(&net, &store, &idx, &valid, 22).await;
    let sent = net.take();
    assert_eq!(sent.len(), 1, "the serving path is still usable");
    let (channel, resp) = &sent[0];
    assert_eq!(*channel, 22);
    assert_eq!(resp.offset, 0);
    assert_eq!(resp.total_bytes, CHUNK.len() as u64);
    assert_eq!(resp.data, b"ab");
    assert!(resp.error.is_none());
}
