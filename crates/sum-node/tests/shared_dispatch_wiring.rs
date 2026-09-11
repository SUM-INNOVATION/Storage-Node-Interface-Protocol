//! Proves every serve loop routes inbound shard traffic through the one shared
//! dispatcher, and that none has grown a private copy back.
//!
//! There is one serve loop now. `simple_serve_loop` is gone: its only caller
//! was V1 `ingest`'s node mode, and `ingest` is retired. So the test no longer
//! checks that two loops agree — it checks the stronger thing, that no *second*
//! loop exists at all. `every_inbound_shard_match_is_inside_a_known_serve_loop`
//! is what makes a reintroduced private loop a build failure rather than a
//! thing someone notices later.
//!
//! This is a source-level assertion, and that needs justifying. The property is
//! structural — "this function delegates rather than duplicates" — and it has
//! no runtime observable: `run_listen` needs a live swarm, a chain RPC endpoint
//! and a signing key to reach, none of which a unit test can stand up. The
//! behaviour it delegates to is covered exhaustively by
//! `sum_node::shard_dispatch::tests`; what is left is the delegation itself,
//! and reading the source is the only way to see it.
//!
//! It exists because of what happened without it. Two loops once carried
//! hand-maintained copies of the same dispatch and drifted: `simple_serve_loop`
//! never grew a `ShardRequestedV2` arm, so every inbound V2 request fell into
//! its catch-all and went unanswered until the swarm's 120s channel reaper.
//! Nothing failed. Nothing warned. The gap stayed invisible until someone read
//! the two loops side by side.
//!
//! Every gate this protocol grows — the V1-push retirement that landed here,
//! ambiguity denial, V2.1's activation check — lands in exactly this code. This
//! test is what makes a gate that reaches only part of it a build failure.

const MAIN_RS: &str = include_str!("../src/main.rs");

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

fn serve_loops() -> Vec<(&'static str, String)> {
    vec![("run_listen", body_of(MAIN_RS, "async fn run_listen("))]
}

#[test]
fn every_serve_loop_calls_the_shared_dispatcher() {
    for (name, body) in serve_loops() {
        assert!(
            body.contains("shard_dispatch.on_event("),
            "{name} does not route through the shared dispatcher"
        );
    }
}

#[test]
fn every_serve_loop_routes_v1_and_v2_to_the_dispatcher() {
    for (name, body) in serve_loops() {
        assert!(
            body.contains("SumNetEvent::ShardRequested { .. }"),
            "{name} does not hand V1 shard requests to the dispatcher"
        );
        assert!(
            body.contains("SumNetEvent::ShardRequestedV2 { .. }"),
            "{name} does not hand V2 shard requests to the dispatcher — this is \
             the exact drift that left the old second loop silently dropping \
             every inbound V2 request"
        );
    }
}

/// A second serve loop is how the drift started last time: one loop grew a
/// gate, the other did not, and nothing said so. There is one loop now, and
/// every place in `main.rs` that matches on an inbound shard event must be
/// inside it.
///
/// A new loop therefore cannot be added quietly — it either delegates from
/// inside `run_listen`, or it appears here.
#[test]
fn every_inbound_shard_match_is_inside_a_known_serve_loop() {
    // The elided form, `SumNetEvent::ShardRequested { .. }`, is what a routing
    // arm looks like: it does not bind the request, because it hands the whole
    // event to the dispatcher. `print_event` binds the fields to log them and
    // makes no dispatch decision, so it is not counted here.
    const ROUTING_ARMS: &[&str] = &[
        "SumNetEvent::ShardRequested { .. }",
        "SumNetEvent::ShardRequestedV2 { .. }",
    ];
    let bodies: Vec<String> = serve_loops().into_iter().map(|(_, b)| b).collect();

    for arm in ROUTING_ARMS {
        let total = MAIN_RS.matches(arm).count();
        let inside: usize = bodies.iter().map(|b| b.matches(arm).count()).sum();
        assert!(total > 0, "no serve loop routes `{arm}`");
        assert_eq!(
            total, inside,
            "main.rs routes `{arm}` outside every known serve loop — a second \
             dispatch has appeared. Route it through ShardDispatch::on_event, \
             or add its loop to serve_loops()."
        );
    }
}

/// The retired second loop must stay retired.
#[test]
fn the_retired_ingest_serve_loop_has_not_come_back() {
    assert!(
        !MAIN_RS.contains("async fn simple_serve_loop("),
        "simple_serve_loop was removed with V1 ingest; a second serve loop is \
         what this whole test file exists to prevent"
    );
}

/// No serve loop may make a dispatch decision of its own. Each of these was a
/// decision one loop made privately; centralising them is the point.
#[test]
fn no_serve_loop_carries_a_private_dispatch_decision() {
    const FORBIDDEN: &[(&str, &str)] = &[
        (
            "handle_manifest_push",
            "manifest-push routing belongs to the shared dispatcher",
        ),
        (
            "handle_request",
            "V1 serve routing belongs to the shared dispatcher",
        ),
        (
            "check_access_or_default",
            "the ACL gate belongs to the shared dispatcher",
        ),
        (
            "V2 disabled on this node",
            "the V2-disabled refusal belongs to the shared dispatcher",
        ),
        (
            "ACCESS_DENIED: not in file access list",
            "the ACL denial response belongs to the shared dispatcher",
        ),
        (
            "MANIFEST_REQUEST_PREFIX",
            "the manifest/chunk split belongs to the shared dispatcher",
        ),
        (
            "dispatcher.handle(",
            "the V2 hand-off belongs to the shared dispatcher",
        ),
    ];

    for (name, body) in serve_loops() {
        for (needle, why) in FORBIDDEN {
            assert!(
                !body.contains(needle),
                "{name} still contains `{needle}` — {why}"
            );
        }
    }
}

/// Both loops must produce the same refusal for a node that cannot serve V2.
/// Before this, only one of them produced any refusal at all.
#[test]
fn the_v2_disabled_refusal_has_exactly_one_definition() {
    assert_eq!(
        MAIN_RS.matches("V2 disabled on this node").count(),
        0,
        "the V2-disabled string is defined in shard_dispatch::V2_DISABLED_ERROR, \
         not in main.rs"
    );
    assert_eq!(
        sum_node::shard_dispatch::V2_DISABLED_ERROR,
        "V2 disabled on this node",
        "the wire text must not change silently — peers match on it"
    );
}
