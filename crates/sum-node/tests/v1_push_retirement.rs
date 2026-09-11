//! V1 push is retired, in both directions, with no way back.
//!
//! Three separate things have to hold, and each fails differently if it does
//! not:
//!
//! 1. **Inbound**: a V1 push is answered with a refusal and never reaches the
//!    store. Covered exhaustively in `sum_node::shard_dispatch::tests`; what is
//!    here is the dispatcher's public entry point, `on_event`, because that is
//!    what the serve loop actually calls.
//! 2. **Outbound**: nothing in this workspace can send one. That is a property
//!    of what does *not* exist, so it is checked against the source: the
//!    sending API, the swarm command, and the production `UploadNet` impl are
//!    all gone, and a fallback that reintroduced any of them would show up
//!    here.
//! 3. **Legacy ingest**: fails at entry with an instruction, rather than
//!    pushing chunks nobody will accept and then blocking on ACKs that cannot
//!    arrive.
//!
//! The source-level checks in (2) need the same justification as
//! `shared_dispatch_wiring.rs`: "no call site anywhere constructs this" has no
//! runtime observable. A test that drives a push and sees it fail proves only
//! that one path is closed; the claim is that every path is.

use std::sync::{Arc, Mutex};

use sum_net::{PeerId, ShardRequest, ShardResponse, ShardResponseV2, SumNetEvent};
use sum_node::shard_dispatch::{
    Handled, ShardDispatch, V1_PUSH_RETIRED_ERROR, v1_push_retired_response,
};
use sum_store::SumStore;
use sum_store::serve::{MANIFEST_REQUEST_PREFIX, RespondShard};
use sum_types::config::StoreConfig;
use tokio::sync::RwLock;

// ── Sources under inspection ─────────────────────────────────────────────────

const SUM_NET_LIB: &str = include_str!("../../sum-net/src/lib.rs");
const SUM_NET_SWARM: &str = include_str!("../../sum-net/src/swarm.rs");
const NODE_UPLOAD: &str = include_str!("../src/upload.rs");
const NODE_MAIN: &str = include_str!("../src/main.rs");

// ── Recorder ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Recorder {
    v1: Mutex<Vec<(u64, ShardResponse)>>,
}

#[async_trait::async_trait]
impl RespondShard for Recorder {
    async fn respond_shard(&self, channel_id: u64, response: ShardResponse) -> anyhow::Result<()> {
        self.v1.lock().unwrap().push((channel_id, response));
        Ok(())
    }
}

#[async_trait::async_trait]
impl sum_node::inbound_v2::RespondNet for Recorder {
    async fn respond_shard_v2(
        &self,
        _channel_id: u64,
        _response: ShardResponseV2,
    ) -> anyhow::Result<()> {
        panic!("a V1 push must not be answered on the V2 protocol");
    }
}

/// An ACL that allows everything, so a store that stays empty is evidence about
/// the push retirement and not about the ACL.
struct AllowAll;

#[async_trait::async_trait]
impl sum_node::inbound_v2::AccessChecker for AllowAll {
    async fn check_access_or_default(
        &self,
        _peer_id: &PeerId,
        _cid: &str,
        _manifest_index: &sum_store::ManifestIndex,
    ) -> bool {
        true
    }
}

fn dispatch() -> (tempfile::TempDir, Arc<RwLock<SumStore>>, ShardDispatch) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(RwLock::new(
        SumStore::new(StoreConfig {
            store_dir: dir.path().join("store"),
            max_chunk_msg_bytes: 64 * 1024 * 1024,
        })
        .unwrap(),
    ));
    let d = ShardDispatch::new(store.clone(), Arc::new(AllowAll), None);
    (dir, store, d)
}

fn push_event(cid: &str, data: Vec<u8>, channel_id: u64) -> SumNetEvent {
    SumNetEvent::ShardRequested {
        peer_id: PeerId::random(),
        request: ShardRequest {
            cid: cid.to_string(),
            offset: None,
            max_bytes: None,
            push_data: Some(data),
        },
        channel_id,
    }
}

// ── 1. Inbound ───────────────────────────────────────────────────────────────

/// Both V1 push forms, through the entry point the serve loop calls.
#[tokio::test]
async fn both_v1_push_forms_are_refused_at_the_dispatcher_entry_point() {
    let body = b"pushed bytes".to_vec();
    let cid = sum_store::content_id::cid_from_data(&body);
    let manifest_cid = format!("{MANIFEST_REQUEST_PREFIX}{}", "ab".repeat(32));

    let (_d, store, d) = dispatch();
    let net = Recorder::default();

    for (i, event) in [
        push_event(&cid, body.clone(), 1),
        push_event(&manifest_cid, b"not really cbor".to_vec(), 2),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            d.on_event(&net, &event).await,
            Handled::Yes,
            "the dispatcher owns inbound shard requests"
        );
        let replies = net.v1.lock().unwrap();
        assert_eq!(replies.len(), i + 1, "each push gets exactly one reply");
        assert_eq!(
            replies[i].1.error.as_deref(),
            Some(V1_PUSH_RETIRED_ERROR),
            "both push forms get the same stable refusal"
        );
    }

    let store = store.read().await;
    assert!(
        store.local.get(&cid).is_err(),
        "the chunk must not have been written"
    );
    assert_eq!(
        d.refusals().v1_chunk_push,
        1,
        "the chunk push was counted once"
    );
    assert_eq!(
        d.refusals().v1_manifest_push,
        1,
        "the manifest push was counted once"
    );
}

/// A V1 **pull** is untouched by the retirement. Without this, "refuse V1
/// pushes" and "refuse V1" are indistinguishable, and the second would cut off
/// every legacy reader.
#[tokio::test]
async fn a_v1_pull_is_still_served_after_the_push_retirement() {
    let body = b"servable bytes";
    let cid = sum_store::content_id::cid_from_data(body);

    let (_d, store, d) = dispatch();
    store.write().await.local.put(&cid, body).unwrap();
    let net = Recorder::default();

    let event = SumNetEvent::ShardRequested {
        peer_id: PeerId::random(),
        request: ShardRequest {
            cid: cid.clone(),
            offset: None,
            max_bytes: None,
            push_data: None,
        },
        channel_id: 3,
    };
    assert_eq!(d.on_event(&net, &event).await, Handled::Yes);

    let replies = net.v1.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].1.error, None, "a pull is not a push");
    assert_eq!(replies[0].1.data, body);
    assert_eq!(
        d.refusals().v1_chunk_push,
        0,
        "a pull must not be counted as a refused push"
    );
}

/// The refusal is a value, not a formatted string built per request, so peers
/// can match on it and the text cannot drift between the two push forms.
#[test]
fn the_refusal_is_the_same_value_for_both_push_forms() {
    let chunk = ShardRequest {
        cid: "some-cid".into(),
        offset: None,
        max_bytes: None,
        push_data: Some(vec![1]),
    };
    let manifest = ShardRequest {
        cid: format!("{MANIFEST_REQUEST_PREFIX}deadbeef"),
        ..chunk.clone()
    };

    let a = v1_push_retired_response(&chunk);
    let b = v1_push_retired_response(&manifest);
    assert_eq!(a.error, b.error);
    assert_eq!(a.error.as_deref(), Some(V1_PUSH_RETIRED_ERROR));
    assert_eq!(a.cid, "some-cid", "the refusal names what was refused");
    assert!(a.data.is_empty() && b.data.is_empty());
}

// ── 2. Outbound ──────────────────────────────────────────────────────────────

/// Source with full-line comments removed.
///
/// The removed symbols are *named* in the comments that explain why they are
/// gone, and those explanations are worth keeping — so the checks below read
/// code, not prose. Only whole comment lines are dropped, so a real call site
/// can never be hidden by this.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The V1 push sending API is gone from `sum-net`, and so is the swarm command
/// underneath it. Either one reappearing is a fallback path.
#[test]
fn no_outbound_v1_push_api_exists() {
    assert!(
        !SUM_NET_LIB.contains("pub async fn push_chunk("),
        "SumNet::push_chunk is back — it is the unauthenticated V1 push"
    );
    assert!(
        !SUM_NET_LIB.contains("pub async fn push_chunk_shared("),
        "SumNet::push_chunk_shared is back — it is the unauthenticated V1 push"
    );
    assert!(
        !code_only(SUM_NET_SWARM).contains("PushShard"),
        "SwarmCommand::PushShard is back — it is the only way a V1 push could \
         reach libp2p"
    );

    // The V2 replacements must still be there, or this test would pass on a
    // node that cannot push at all.
    assert!(SUM_NET_LIB.contains("pub async fn push_chunk_v2("));
    assert!(SUM_NET_LIB.contains("pub async fn push_manifest_v2("));
}

/// `impl UploadNet for SumNet` was the production sender. Its absence is what
/// makes `UploadOrchestrator` undriveable onto a real wire — the orchestrator
/// itself is retained and tested through mocks.
#[test]
fn the_upload_orchestrator_has_no_production_network() {
    assert!(
        !code_only(NODE_UPLOAD).contains("impl UploadNet for SumNet"),
        "the V1 push production sender is back"
    );
    assert!(
        NODE_UPLOAD.contains("pub trait UploadNet"),
        "the trait stays — tests drive the orchestrator through it"
    );
    // The trait's own `push_chunk_shared` is still declared, and must be: the
    // orchestrator calls it, and mocks implement it. What matters is that
    // `SumNet` is not one of those implementors, which is the assertion above.
}

/// No V1-push fallback anywhere: not on a V2 failure, not on a peer that turns
/// out to be V1-only, not anywhere else. A fallback would have to name one of
/// the removed symbols, and none of them exists to name.
#[test]
fn no_source_in_the_workspace_sends_a_v1_push() {
    // `sum-net` must not be able to express one at all.
    for (name, src) in [
        ("sum-net/src/lib.rs", SUM_NET_LIB),
        ("sum-net/src/swarm.rs", SUM_NET_SWARM),
    ] {
        for needle in ["push_chunk_shared", "PushShard"] {
            assert!(
                !code_only(src).contains(needle),
                "{name} still references `{needle}` — a V1 push path survives"
            );
        }
    }

    // `sum-node`'s binary must not call one. (`upload.rs` keeps the trait
    // method for its mocks; see the test above for why that is not a path.)
    assert!(
        !code_only(NODE_MAIN).contains("push_chunk_shared"),
        "main.rs still sends a V1 push"
    );

    // In particular, no fallback on a V2 failure anywhere in the crate.
    for (name, src) in [
        ("sum-node/src/upload.rs", NODE_UPLOAD),
        ("sum-node/src/main.rs", NODE_MAIN),
    ] {
        let code = code_only(src);
        assert!(
            !code.contains("PushShard"),
            "{name} names the retired swarm command"
        );
    }
}

// ── 3. Legacy ingest ─────────────────────────────────────────────────────────

/// The instruction has to be actionable. A message that says only "retired"
/// leaves an operator with a broken command and no next step.
#[test]
fn legacy_ingest_names_the_command_to_run_instead() {
    let msg = sum_node::upload::LEGACY_INGEST_RETIRED;
    assert!(
        msg.contains("ingest-v2"),
        "must name the replacement: {msg}"
    );
    assert!(
        msg.contains("/sum/storage/v1"),
        "must say what was retired: {msg}"
    );
    assert!(
        msg.contains("nothing was written") || msg.contains("No chunks were pushed"),
        "must say nothing happened, so the operator knows there is no partial \
         state to clean up: {msg}"
    );
    assert_eq!(
        sum_node::upload::legacy_ingest_retired().to_string(),
        msg,
        "the error is the message, with nothing wrapped around it"
    );
}

/// It must fail **immediately**. The old flow blocked draining `ShardReceived`
/// ACKs for chunk pushes and then again for the manifest push; with inbound V1
/// push refused those ACKs can never arrive, so the drains would run to their
/// full timeouts — minutes — and then blame replication.
///
/// So: `run_ingest` opens nothing. It is a synchronous function whose body
/// constructs the error, and the assertions below are that it has no network,
/// no store, no chain RPC and no wait in it.
#[test]
fn legacy_ingest_refuses_before_any_io() {
    let body = body_of(NODE_MAIN, "fn run_ingest(");

    assert!(
        body.contains("legacy_ingest_retired()"),
        "run_ingest must return the retirement error"
    );
    for (needle, why) in [
        ("SumNet::new", "it must not open a swarm"),
        ("ingest_file", "it must not chunk the file"),
        ("L1RpcClient::new", "it must not call the chain"),
        ("next_event()", "it must not drain ACKs that cannot arrive"),
        ("UploadOrchestrator", "it must not plan an upload"),
        ("push_manifest_to_recipients", "it must not push a manifest"),
        ("sleep", "it must not wait for anything"),
        ("timeout", "it must not wait for anything"),
    ] {
        assert!(
            !body.contains(needle),
            "run_ingest still contains `{needle}` — {why}"
        );
    }

    // And the whole ACK-draining helper is gone from the file, not merely
    // unreferenced from this one function.
    assert!(
        !NODE_MAIN.contains("async fn push_manifest_to_recipients("),
        "the synchronous manifest-ACK drain must be gone — it is the wait that \
         could never finish"
    );
}

/// Extract a function body by brace matching from its signature.
fn body_of(source: &str, signature: &str) -> String {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("signature not found: {signature}"));
    let open = start
        + source[start..]
            .find('{')
            .expect("function signature with no body");
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after {signature}");
}
