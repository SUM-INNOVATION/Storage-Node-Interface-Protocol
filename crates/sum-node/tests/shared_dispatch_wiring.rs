//! Proves both serve loops route inbound shard traffic through the one shared
//! dispatcher, and that neither has grown a private copy back.
//!
//! This is a source-level assertion, and that needs justifying. The property is
//! structural — "these two functions delegate rather than duplicate" — and it
//! has no runtime observable: `run_listen` and `simple_serve_loop` both need a
//! live swarm, a chain RPC endpoint and a signing key to reach, none of which a
//! unit test can stand up. The behaviour they delegate to is covered
//! exhaustively by `sum_node::shard_dispatch::tests`; what is left is the
//! delegation itself, and reading the source is the only way to see it.
//!
//! It exists because of what happened without it. The two loops carried
//! hand-maintained copies of the same dispatch and drifted:
//! `simple_serve_loop` never grew a `ShardRequestedV2` arm, so every inbound V2
//! request fell into its catch-all and went unanswered until the swarm's 120s
//! channel reaper. Nothing failed. Nothing warned. The gap was invisible until
//! someone read both loops side by side.
//!
//! Every gate this protocol is about to grow — WP-B's V1-push retirement,
//! ambiguity denial, V2.1's activation check — lands in exactly this code. This
//! test is what makes a gate that reaches only one loop a build failure.

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
    vec![
        ("run_listen", body_of(MAIN_RS, "async fn run_listen(")),
        (
            "simple_serve_loop",
            body_of(MAIN_RS, "async fn simple_serve_loop("),
        ),
    ]
}

#[test]
fn both_serve_loops_call_the_shared_dispatcher() {
    for (name, body) in serve_loops() {
        assert!(
            body.contains("shard_dispatch.on_event("),
            "{name} does not route through the shared dispatcher"
        );
    }
}

#[test]
fn both_serve_loops_route_v1_and_v2_to_the_dispatcher() {
    for (name, body) in serve_loops() {
        assert!(
            body.contains("SumNetEvent::ShardRequested { .. }"),
            "{name} does not hand V1 shard requests to the dispatcher"
        );
        assert!(
            body.contains("SumNetEvent::ShardRequestedV2 { .. }"),
            "{name} does not hand V2 shard requests to the dispatcher — this is \
             the exact drift that left simple_serve_loop silently dropping every \
             inbound V2 request"
        );
    }
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
