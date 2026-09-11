//! Outbound request/response correlation.
//!
//! A response arriving on a request-response stream carries no proof of what
//! it answers. Its CID, its Merkle root, its variant — every field is written
//! by the peer. Validating one peer-supplied field against another peer-supplied
//! field proves only that the peer was self-consistent, which a malicious peer
//! has no trouble being.
//!
//! The only trustworthy statement of what this node asked for is the request
//! this node itself constructed. So we keep it. [`OutboundTracker::record`] is
//! called at the `send_request` call site with the request still in hand;
//! [`OutboundTracker::correlate_response`] returns that retained identity, and
//! rejects the response outright when it does not answer the request the id
//! belongs to. Nothing downstream reconstructs the expectation from the wire.
//!
//! ## Keying
//!
//! `OutboundRequestId` is **not** a key on its own. libp2p is explicit about
//! this: "`OutboundRequestId`'s uniqueness is only guaranteed between outbound
//! requests of the same originating `Behaviour`" (libp2p-request-response
//! 0.28.0, `lib.rs`). One behaviour is registered today, so ids happen to be
//! globally unique — but WP-B splits shard transfer into a `/sum/storage/v1`
//! behaviour and a `/sum/storage/v2` behaviour, and on that day both counters
//! start at zero and collide immediately.
//!
//! [`OutboundKey`] therefore pairs the id with a [`RequestDomain`] naming the
//! behaviour that issued it. Adding WP-B's second behaviour is one new variant,
//! and a record filed under one domain can never be taken by the other.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use libp2p::PeerId;
use libp2p::request_response::OutboundRequestId;

use crate::codec::{
    SHARD_XFER_PROTOCOL_V1, SHARD_XFER_PROTOCOL_V2, ShardRequest, ShardRequestV2, ShardResponseV2,
    ShardResponseVersioned,
};

/// CID prefix marking a V1 manifest pull: `manifest:<merkle_root_hex>`.
pub const MANIFEST_CID_PREFIX: &str = "manifest:";

/// How long an unanswered outbound record is retained before the reaper drops
/// it. libp2p's own request timeout terminates every request id with either a
/// response or an `OutboundFailure`, so under correct operation nothing ever
/// reaches this age. It exists so that a libp2p bug, or a future behaviour that
/// forgets to report a terminal event, degrades into a bounded leak instead of
/// an unbounded one.
pub(crate) const OUTBOUND_RECORD_TTL: Duration = Duration::from_secs(300);

/// How long a poisoned key stays refused before the reaper frees it.
///
/// Must exceed libp2p's request timeout — 120s as this swarm configures it —
/// so both colliding requests have certainly produced and been refused their
/// terminal events before the slot is released. See
/// [`OutboundTracker::record`] for why a later request cannot inherit it.
pub(crate) const POISON_TTL: Duration = Duration::from_secs(600);

// ── Domain ───────────────────────────────────────────────────────────────────

/// Which registered request-response behaviour an [`OutboundRequestId`] was
/// minted by.
///
/// See the module docs: ids are unique per behaviour, not per process, so this
/// is a required part of the key rather than a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestDomain {
    /// The single behaviour registered today, carrying `/sum/storage/v1` and
    /// `/sum/storage/v2` on one `VersionedShardCodec`.
    ///
    /// WP-B replaces this with one variant per protocol. That is a change to
    /// this enum and to the `record` call sites — deliberately, so the compiler
    /// names every place a domain must be chosen.
    ShardXfer,

    /// Stand-in for the second behaviour WP-B will register.
    ///
    /// It exists only under `cfg(test)`, and only so that
    /// `ids_are_namespaced_by_domain` can demonstrate the property this key is
    /// built for *before* there is a second real behaviour to demonstrate it
    /// with. When WP-B lands, that test retargets to the real variant and this
    /// one goes away.
    #[cfg(test)]
    SecondBehaviourProbe,
}

/// Fully-qualified identifier for one outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutboundKey {
    pub domain: RequestDomain,
    pub id: OutboundRequestId,
}

impl OutboundKey {
    pub fn new(domain: RequestDomain, id: OutboundRequestId) -> Self {
        Self { domain, id }
    }
}

// ── Request kind ─────────────────────────────────────────────────────────────

/// What this node asked for, recorded from the request it built.
///
/// Deliberately compact: it holds the identifying fields, never the payload.
/// A push retains its CID, not its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundRequestKind {
    /// V1 pull of chunk bytes by CID.
    V1Chunk { cid: String },
    /// V1 pull of a manifest. `cid` is the exact wire string
    /// (`manifest:<root_hex>`); `root_hex` is the root this node asked for.
    V1Manifest { root_hex: String, cid: String },
    /// V1 push of chunk bytes. The ACK echoes the CID.
    V1Push { cid: String },
    /// V2 pull of chunk bytes by CID.
    V2Pull { cid: String },
    /// V2 push of chunk bytes with a Merkle proof.
    V2Push {
        merkle_root: [u8; 32],
        chunk_index: u32,
    },
    /// V2 push of manifest bytes.
    V2ManifestPush { merkle_root: [u8; 32] },
    /// V2 pull of a manifest by root.
    V2ManifestPull { merkle_root: [u8; 32] },
}

impl OutboundRequestKind {
    /// Classify a V1 request **that this node is about to send**.
    ///
    /// The `strip_prefix` below reads like the defect this module exists to
    /// prevent, and is its exact opposite: `req.cid` here is a string this
    /// process constructed (`SumNet::request_manifest` formats it from a root
    /// the caller supplied). No peer has seen it yet. The same operation on a
    /// *response* field is what is forbidden.
    pub(crate) fn from_v1(req: &ShardRequest) -> Self {
        if req.push_data.is_some() {
            return Self::V1Push {
                cid: req.cid.clone(),
            };
        }
        match req.cid.strip_prefix(MANIFEST_CID_PREFIX) {
            Some(root_hex) => Self::V1Manifest {
                root_hex: root_hex.to_string(),
                cid: req.cid.clone(),
            },
            None => Self::V1Chunk {
                cid: req.cid.clone(),
            },
        }
    }

    /// Classify a V2 request this node is about to send.
    pub(crate) fn from_v2(req: &ShardRequestV2) -> Self {
        match req {
            ShardRequestV2::Pull { cid, .. } => Self::V2Pull { cid: cid.clone() },
            ShardRequestV2::Push {
                merkle_root,
                chunk_index,
                ..
            } => Self::V2Push {
                merkle_root: *merkle_root,
                chunk_index: *chunk_index,
            },
            ShardRequestV2::ManifestPush { merkle_root, .. } => Self::V2ManifestPush {
                merkle_root: *merkle_root,
            },
            ShardRequestV2::ManifestPull { merkle_root } => Self::V2ManifestPull {
                merkle_root: *merkle_root,
            },
        }
    }

    /// The protocol this request was written for.
    pub fn protocol(&self) -> &'static str {
        match self {
            Self::V1Chunk { .. } | Self::V1Manifest { .. } | Self::V1Push { .. } => {
                SHARD_XFER_PROTOCOL_V1
            }
            Self::V2Pull { .. }
            | Self::V2Push { .. }
            | Self::V2ManifestPush { .. }
            | Self::V2ManifestPull { .. } => SHARD_XFER_PROTOCOL_V2,
        }
    }

    /// Short label for logs and mismatch messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::V1Chunk { .. } => "v1-chunk-pull",
            Self::V1Manifest { .. } => "v1-manifest-pull",
            Self::V1Push { .. } => "v1-chunk-push",
            Self::V2Pull { .. } => "v2-chunk-pull",
            Self::V2Push { .. } => "v2-chunk-push",
            Self::V2ManifestPush { .. } => "v2-manifest-push",
            Self::V2ManifestPull { .. } => "v2-manifest-pull",
        }
    }

    /// Check that `response` answers *this* request.
    ///
    /// Every comparison is retained-value against wire-value. No two wire
    /// values are ever compared with each other.
    fn check(&self, response: &ShardResponseVersioned) -> Result<(), CorrelationError> {
        match (self, response) {
            // ── V1 ───────────────────────────────────────────────────────
            (
                Self::V1Chunk { cid } | Self::V1Manifest { cid, .. } | Self::V1Push { cid },
                ShardResponseVersioned::V1(resp),
            ) => cid_eq(cid, &resp.cid),

            // ── V2 ───────────────────────────────────────────────────────
            (
                Self::V2Pull { cid },
                ShardResponseVersioned::V2(ShardResponseV2::Data { cid: got, .. }),
            ) => cid_eq(cid, got),
            (
                Self::V2Push {
                    merkle_root,
                    chunk_index,
                },
                ShardResponseVersioned::V2(ShardResponseV2::PushAck {
                    merkle_root: got_root,
                    chunk_index: got_index,
                    ..
                }),
            ) => {
                root_eq(merkle_root, got_root)?;
                if chunk_index != got_index {
                    return Err(CorrelationError::ChunkIndexMismatch {
                        expected: *chunk_index,
                        got: *got_index,
                    });
                }
                Ok(())
            }
            (
                Self::V2ManifestPush { merkle_root },
                ShardResponseVersioned::V2(ShardResponseV2::ManifestPushAck {
                    merkle_root: got,
                    ..
                }),
            ) => root_eq(merkle_root, got),
            (
                Self::V2ManifestPull { merkle_root },
                ShardResponseVersioned::V2(ShardResponseV2::ManifestData {
                    merkle_root: got, ..
                }),
            ) => root_eq(merkle_root, got),

            // Anything else is a response of the wrong shape for the request.
            (expected, got) => Err(CorrelationError::ResponseKindMismatch {
                expected: expected.label(),
                got: response_label(got),
            }),
        }
    }
}

fn cid_eq(expected: &str, got: &str) -> Result<(), CorrelationError> {
    if expected == got {
        Ok(())
    } else {
        Err(CorrelationError::CidMismatch {
            expected: expected.to_string(),
            got: got.to_string(),
        })
    }
}

fn root_eq(expected: &[u8; 32], got: &[u8; 32]) -> Result<(), CorrelationError> {
    if expected == got {
        Ok(())
    } else {
        Err(CorrelationError::RootMismatch {
            expected: hex_root(expected),
            got: hex_root(got),
        })
    }
}

fn hex_root(root: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in root {
        // Infallible: `String`'s `write_str` never fails.
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn response_label(response: &ShardResponseVersioned) -> &'static str {
    match response {
        ShardResponseVersioned::V1(_) => "v1-response",
        ShardResponseVersioned::V2(ShardResponseV2::Data { .. }) => "v2-data",
        ShardResponseVersioned::V2(ShardResponseV2::PushAck { .. }) => "v2-push-ack",
        ShardResponseVersioned::V2(ShardResponseV2::ManifestPushAck { .. }) => {
            "v2-manifest-push-ack"
        }
        ShardResponseVersioned::V2(ShardResponseV2::ManifestData { .. }) => "v2-manifest-data",
    }
}

// ── Origin ───────────────────────────────────────────────────────────────────

/// The locally-retained identity of the request a response answers.
///
/// Handed to the upper layer alongside every accepted response, so consumers
/// have a non-peer-controlled statement of what was asked. Consumers that need
/// the requested root — the manifest ingress especially — take it from here,
/// never from the response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundOrigin {
    peer: PeerId,
    protocol: &'static str,
    kind: OutboundRequestKind,
}

impl OutboundOrigin {
    pub fn new(peer: PeerId, kind: OutboundRequestKind) -> Self {
        Self {
            peer,
            protocol: kind.protocol(),
            kind,
        }
    }

    /// The peer this node sent the request to (recorded at send time, not read
    /// from the response envelope).
    pub fn peer(&self) -> PeerId {
        self.peer
    }

    /// The protocol the request was written for.
    pub fn protocol(&self) -> &'static str {
        self.protocol
    }

    pub fn kind(&self) -> &OutboundRequestKind {
        &self.kind
    }

    /// The Merkle root hex this node asked for, if this was a V1 manifest pull.
    ///
    /// `None` for every other request kind — a caller that indexes manifests
    /// therefore cannot proceed on a response to a chunk pull.
    pub fn requested_manifest_root_hex(&self) -> Option<&str> {
        match &self.kind {
            OutboundRequestKind::V1Manifest { root_hex, .. } => Some(root_hex),
            _ => None,
        }
    }

    /// The Merkle root this node asked for, if this was a V2 manifest pull.
    pub fn requested_manifest_root(&self) -> Option<[u8; 32]> {
        match &self.kind {
            OutboundRequestKind::V2ManifestPull { merkle_root } => Some(*merkle_root),
            _ => None,
        }
    }

    /// The CID this node asked for, for the kinds identified by one.
    pub fn requested_cid(&self) -> Option<&str> {
        match &self.kind {
            OutboundRequestKind::V1Chunk { cid }
            | OutboundRequestKind::V1Manifest { cid, .. }
            | OutboundRequestKind::V1Push { cid }
            | OutboundRequestKind::V2Pull { cid } => Some(cid),
            _ => None,
        }
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a response was refused. Every variant means the response is dropped
/// before it reaches the upper layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrelationError {
    /// No outbound record for this (domain, id). Either the id was never
    /// issued here, or it was already spent — request ids are single-use.
    UnknownRequestId,
    /// Two requests claimed this id, so no identity can be trusted for it.
    /// Both are abandoned; see [`OutboundTracker::record`]. The two labels are
    /// carried for the log line only — neither identity was retained.
    PoisonedRequestId {
        first: &'static str,
        second: &'static str,
    },
    /// The response arrived from a peer other than the one the request went to.
    ///
    /// Boxed because a `PeerId` is large (~80 bytes) and this is the error half
    /// of a `Result` returned on every response; two inline ones would make the
    /// whole enum the size of the success path several times over.
    PeerMismatch(Box<PeerMismatch>),
    /// The response variant cannot answer the request variant.
    ResponseKindMismatch {
        expected: &'static str,
        got: &'static str,
    },
    /// Right shape, wrong subject.
    CidMismatch { expected: String, got: String },
    /// Right shape, wrong Merkle root.
    RootMismatch { expected: String, got: String },
    /// Right root, wrong chunk index.
    ChunkIndexMismatch { expected: u32, got: u32 },
}

/// Payload of [`CorrelationError::PeerMismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMismatch {
    /// The peer the request was sent to, from the outbound record.
    pub expected: PeerId,
    /// The peer the response arrived from.
    pub got: PeerId,
}

impl std::fmt::Display for CorrelationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRequestId => {
                write!(f, "no outbound request record for this response")
            }
            Self::PoisonedRequestId { first, second } => write!(
                f,
                "request id was claimed by both {first} and {second} — \
                 no identity can be trusted for it"
            ),
            Self::PeerMismatch(m) => write!(
                f,
                "response peer mismatch: asked {}, answered by {}",
                m.expected, m.got
            ),
            Self::ResponseKindMismatch { expected, got } => {
                write!(f, "response kind mismatch: asked {expected}, got {got}")
            }
            Self::CidMismatch { expected, got } => {
                write!(f, "response CID mismatch: asked {expected}, got {got}")
            }
            Self::RootMismatch { expected, got } => {
                write!(
                    f,
                    "response Merkle root mismatch: asked {expected}, got {got}"
                )
            }
            Self::ChunkIndexMismatch { expected, got } => {
                write!(
                    f,
                    "response chunk index mismatch: asked {expected}, got {got}"
                )
            }
        }
    }
}

impl std::error::Error for CorrelationError {}

/// An outbound request id was issued twice while the first was still pending.
///
/// Neither identity is kept. The key is poisoned and both requests are
/// abandoned — see [`OutboundTracker::record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordCollision {
    pub key: OutboundKey,
    /// The request that was already filed under this key. Not retained.
    pub first: &'static str,
    /// The request that collided with it. Not retained either.
    pub second: &'static str,
}

impl std::fmt::Display for RecordCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request id {} was claimed by both {} and {} — key poisoned, both abandoned",
            self.key.id, self.first, self.second
        )
    }
}

impl std::error::Error for RecordCollision {}

// ── Tracker ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct PendingOutbound {
    peer: PeerId,
    kind: OutboundRequestKind,
    issued_at: Instant,
}

/// A key that two requests claimed. Neither identity is retained.
#[derive(Debug)]
struct Poison {
    at: Instant,
    first: &'static str,
    second: &'static str,
}

#[derive(Debug)]
enum Slot {
    /// One request, filed at send time.
    Pending(PendingOutbound),
    /// Two or more requests claimed this key. See [`OutboundTracker::record`].
    Poisoned(Poison),
}

/// What a failure event's key resolved to.
#[derive(Debug)]
pub(crate) enum FailureOutcome {
    /// The identity retained when the request was sent. Both fields come from
    /// the record, not from the failure event.
    Retained {
        peer: PeerId,
        kind: OutboundRequestKind,
    },
    /// No record. Nothing can be said about which request failed.
    Unknown,
    /// The key is poisoned. Refuse.
    Poisoned,
}

/// The set of outbound requests awaiting a terminal event.
///
/// A pending entry leaves by exactly one of three doors — a correlated
/// response, an outbound failure, or the reaper — and all three remove, so a
/// spent id is never usable. A poisoned entry leaves only by the reaper.
#[derive(Debug, Default)]
pub(crate) struct OutboundTracker {
    slots: HashMap<OutboundKey, Slot>,
}

impl OutboundTracker {
    /// File the identity of a request at the moment it is sent.
    ///
    /// # Collisions
    ///
    /// A key that is already occupied means libp2p issued an id while an
    /// earlier request under the same id is still outstanding. That should be
    /// impossible, and if it happens the tracker cannot tell which of the two
    /// requests any later response belongs to.
    ///
    /// Keeping the first record is **not** fail-closed, which is what an
    /// earlier version of this got wrong. Both requests are on the wire under
    /// the same key, so a response to the *second* would be matched against the
    /// *first*'s retained identity — and if the two happen to share a shape and
    /// a subject, it would be accepted. Keeping the second is wrong for the
    /// mirror-image reason. There is no safe choice among the two identities,
    /// so neither is kept.
    ///
    /// The key is marked poisoned instead. Every subsequent terminal event for
    /// it — response or failure — is refused, and no domain event is emitted
    /// for either request. Both are abandoned, which is the only outcome that
    /// cannot accept a response for the wrong request.
    ///
    /// # Clearing the poison
    ///
    /// Only the reaper clears it, after [`POISON_TTL`]. That constant must
    /// exceed libp2p's request timeout (120s as this swarm configures it), so
    /// by the time a poisoned slot is freed both colliding requests have long
    /// since produced their terminal events and been refused.
    ///
    /// A later request cannot inherit the poison. `OutboundRequestId` wraps a
    /// `u64` that `Behaviour::next_outbound_request_id` only ever increments,
    /// so within a process an id is issued once and the counter never returns
    /// to it. Reaching a poisoned key again would take 2^64 requests. The TTL
    /// therefore exists to bound memory, not to make the key reusable — and if
    /// the counter invariant is ever broken badly enough to reissue an id, the
    /// second collision simply re-poisons.
    pub(crate) fn record(
        &mut self,
        key: OutboundKey,
        peer: PeerId,
        kind: OutboundRequestKind,
    ) -> Result<(), RecordCollision> {
        use std::collections::hash_map::Entry;
        match self.slots.entry(key) {
            Entry::Vacant(v) => {
                v.insert(Slot::Pending(PendingOutbound {
                    peer,
                    kind,
                    issued_at: Instant::now(),
                }));
                Ok(())
            }
            Entry::Occupied(mut o) => {
                let first = match o.get() {
                    Slot::Pending(p) => p.kind.label(),
                    Slot::Poisoned(p) => p.first,
                };
                let second = kind.label();
                // Neither identity survives.
                o.insert(Slot::Poisoned(Poison {
                    at: Instant::now(),
                    first,
                    second,
                }));
                Err(RecordCollision { key, first, second })
            }
        }
    }

    /// Match a response against its retained request.
    ///
    /// A pending record is removed on **every** path, success or failure:
    /// libp2p delivers at most one terminal event per request id, so the id is
    /// spent either way, and a rejected response must not leave a record a
    /// second attempt could match. A poisoned slot is left in place — the
    /// poison outlives individual responses, because both colliding requests
    /// will produce one and both must be refused.
    pub(crate) fn correlate_response(
        &mut self,
        key: OutboundKey,
        peer: PeerId,
        response: &ShardResponseVersioned,
    ) -> Result<OutboundOrigin, CorrelationError> {
        match self.slots.get(&key) {
            None => return Err(CorrelationError::UnknownRequestId),
            Some(Slot::Poisoned(p)) => {
                return Err(CorrelationError::PoisonedRequestId {
                    first: p.first,
                    second: p.second,
                });
            }
            Some(Slot::Pending(_)) => {}
        }
        let Some(Slot::Pending(record)) = self.slots.remove(&key) else {
            unreachable!("checked immediately above")
        };

        if record.peer != peer {
            return Err(CorrelationError::PeerMismatch(Box::new(PeerMismatch {
                expected: record.peer,
                got: peer,
            })));
        }

        record.kind.check(response)?;
        Ok(OutboundOrigin::new(record.peer, record.kind))
    }

    /// Resolve a failure event's key to the identity retained at send time.
    ///
    /// Returns the recorded **peer** as well as the kind. The caller compares
    /// it against the peer the failure event carries and builds
    /// [`OutboundOrigin`] from the returned values only — never from the event.
    /// A pending record is removed; a poisoned slot is left in place.
    pub(crate) fn on_failure(&mut self, key: OutboundKey) -> FailureOutcome {
        match self.slots.get(&key) {
            None => FailureOutcome::Unknown,
            Some(Slot::Poisoned(_)) => FailureOutcome::Poisoned,
            Some(Slot::Pending(_)) => match self.slots.remove(&key) {
                Some(Slot::Pending(r)) => FailureOutcome::Retained {
                    peer: r.peer,
                    kind: r.kind,
                },
                _ => unreachable!("checked immediately above"),
            },
        }
    }

    /// Drop pending records older than `ttl` and poisoned slots older than
    /// [`POISON_TTL`]. Returns how many went.
    pub(crate) fn reap(&mut self, now: Instant, ttl: Duration) -> usize {
        let before = self.slots.len();
        self.slots.retain(|_, slot| match slot {
            Slot::Pending(r) => now.duration_since(r.issued_at) < ttl,
            Slot::Poisoned(p) => now.duration_since(p.at) < POISON_TTL,
        });
        before - self.slots.len()
    }

    /// How many keys are poisoned. Non-zero means the request-id invariant was
    /// violated at least once in this process.
    #[cfg(test)]
    pub(crate) fn poisoned_len(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s, Slot::Poisoned(_)))
            .count()
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use libp2p::request_response::{self, ProtocolSupport};

    use crate::codec::{
        ShardRequest, ShardResponse, ShardResponseV2, ShardResponseVersioned, VersionedShardCodec,
    };

    /// `OutboundRequestId`'s inner counter is private, so ids are minted the
    /// only way production mints them: from a real `request_response::Behaviour`
    /// configured exactly as the swarm configures it. `send_request` to an
    /// unconnected peer queues a dial and returns the id, which is all we need.
    fn behaviour() -> request_response::Behaviour<VersionedShardCodec> {
        request_response::Behaviour::with_codec(
            VersionedShardCodec::default(),
            [
                (SHARD_XFER_PROTOCOL_V2.to_string(), ProtocolSupport::Full),
                (SHARD_XFER_PROTOCOL_V1.to_string(), ProtocolSupport::Full),
            ],
            request_response::Config::default().with_request_timeout(Duration::from_secs(120)),
        )
    }

    fn mint(b: &mut request_response::Behaviour<VersionedShardCodec>, peer: PeerId) -> OutboundKey {
        let id = b.send_request(
            &peer,
            crate::codec::ShardRequestVersioned::V1(chunk_request("placeholder")),
        );
        OutboundKey::new(RequestDomain::ShardXfer, id)
    }

    fn chunk_request(cid: &str) -> ShardRequest {
        ShardRequest {
            cid: cid.to_string(),
            offset: None,
            max_bytes: None,
            push_data: None,
        }
    }

    fn v1_response(cid: &str) -> ShardResponseVersioned {
        ShardResponseVersioned::V1(ShardResponse {
            cid: cid.to_string(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: None,
        })
    }

    const ROOT_A: [u8; 32] = [0xAA; 32];
    const ROOT_B: [u8; 32] = [0xBB; 32];

    fn hex_a() -> String {
        hex_root(&ROOT_A)
    }
    fn hex_b() -> String {
        hex_root(&ROOT_B)
    }

    fn manifest_cid(root_hex: &str) -> String {
        format!("{MANIFEST_CID_PREFIX}{root_hex}")
    }

    fn manifest_pull_kind(root_hex: &str) -> OutboundRequestKind {
        OutboundRequestKind::from_v1(&chunk_request(&manifest_cid(root_hex)))
    }

    // ── Classification happens on the request ────────────────────────────

    #[test]
    fn a_locally_built_manifest_pull_is_classified_as_one() {
        let kind = manifest_pull_kind(&hex_a());
        assert_eq!(
            kind,
            OutboundRequestKind::V1Manifest {
                root_hex: hex_a(),
                cid: manifest_cid(&hex_a()),
            }
        );
        assert_eq!(kind.protocol(), SHARD_XFER_PROTOCOL_V1);
    }

    #[test]
    fn a_push_is_a_push_even_under_a_manifest_cid() {
        let mut req = chunk_request(&manifest_cid(&hex_a()));
        req.push_data = Some(vec![1, 2, 3]);
        assert_eq!(
            OutboundRequestKind::from_v1(&req),
            OutboundRequestKind::V1Push {
                cid: manifest_cid(&hex_a())
            }
        );
    }

    /// The accessor a manifest ingress keys off must stay `None` for anything
    /// that is not a manifest pull — that is what stops a chunk response from
    /// reaching the index.
    #[test]
    fn only_a_manifest_pull_reports_a_requested_manifest_root() {
        let peer = PeerId::random();
        let cases = [
            OutboundRequestKind::V1Chunk { cid: "c".into() },
            OutboundRequestKind::V1Push { cid: "c".into() },
            OutboundRequestKind::V2Pull { cid: "c".into() },
            OutboundRequestKind::V2Push {
                merkle_root: ROOT_A,
                chunk_index: 0,
            },
            OutboundRequestKind::V2ManifestPush {
                merkle_root: ROOT_A,
            },
            OutboundRequestKind::V2ManifestPull {
                merkle_root: ROOT_A,
            },
        ];
        for kind in cases {
            let origin = OutboundOrigin::new(peer, kind.clone());
            assert_eq!(
                origin.requested_manifest_root_hex(),
                None,
                "{kind:?} must not present a V1 manifest root"
            );
        }

        let origin = OutboundOrigin::new(peer, manifest_pull_kind(&hex_a()));
        assert_eq!(origin.requested_manifest_root_hex(), Some(hex_a().as_str()));
    }

    // ── The attack, at the networking layer ──────────────────────────────

    /// This node pulls the manifest for root A. The peer answers with a reply
    /// labelled for root B. Every field the peer wrote agrees with every other
    /// field it wrote; none of them agree with the request we kept.
    #[test]
    fn a_manifest_response_for_a_different_root_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();

        let err = t
            .correlate_response(key, peer, &v1_response(&manifest_cid(&hex_b())))
            .unwrap_err();
        assert_eq!(
            err,
            CorrelationError::CidMismatch {
                expected: manifest_cid(&hex_a()),
                got: manifest_cid(&hex_b()),
            }
        );
    }

    #[test]
    fn a_correlated_response_yields_the_retained_identity() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        let origin = t
            .correlate_response(key, peer, &v1_response(&manifest_cid(&hex_a())))
            .unwrap();

        assert_eq!(origin.peer(), peer);
        assert_eq!(origin.protocol(), SHARD_XFER_PROTOCOL_V1);
        assert_eq!(origin.requested_manifest_root_hex(), Some(hex_a().as_str()));
        assert_eq!(t.len(), 0, "a correlated response spends its record");
    }

    // ── Id discipline ────────────────────────────────────────────────────

    #[test]
    fn an_unrecorded_request_id_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        assert_eq!(
            t.correlate_response(key, peer, &v1_response("anything"))
                .unwrap_err(),
            CorrelationError::UnknownRequestId
        );
    }

    #[test]
    fn a_request_id_is_single_use() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);
        let cid = manifest_cid(&hex_a());

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert!(t.correlate_response(key, peer, &v1_response(&cid)).is_ok());
        assert_eq!(
            t.correlate_response(key, peer, &v1_response(&cid))
                .unwrap_err(),
            CorrelationError::UnknownRequestId,
            "a second response on the same id must not correlate"
        );
    }

    /// A refused response spends the id too. Otherwise a peer could probe with
    /// wrong answers and keep the slot alive for a later attempt.
    #[test]
    fn a_refused_response_still_spends_the_id() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert!(
            t.correlate_response(key, peer, &v1_response(&manifest_cid(&hex_b())))
                .is_err()
        );
        assert_eq!(
            t.correlate_response(key, peer, &v1_response(&manifest_cid(&hex_a())))
                .unwrap_err(),
            CorrelationError::UnknownRequestId
        );
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn a_response_from_another_peer_is_refused() {
        let asked = PeerId::random();
        let other = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, asked);

        t.record(key, asked, manifest_pull_kind(&hex_a())).unwrap();
        assert_eq!(
            t.correlate_response(key, other, &v1_response(&manifest_cid(&hex_a())))
                .unwrap_err(),
            CorrelationError::PeerMismatch(Box::new(PeerMismatch {
                expected: asked,
                got: other
            }))
        );
        assert_eq!(t.len(), 0);
    }

    /// `OutboundRequestId` is unique per behaviour, not per process. The key
    /// carries the domain so that two behaviours minting the same raw id — which
    /// is what WP-B's split makes routine — cannot take each other's records.
    #[test]
    fn ids_are_namespaced_by_domain() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();

        let shard = mint(&mut b, peer);
        let probe = OutboundKey::new(RequestDomain::SecondBehaviourProbe, shard.id);
        assert_eq!(shard.id, probe.id, "same raw id, different behaviour");
        assert_ne!(shard, probe);

        t.record(shard, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert_eq!(
            t.correlate_response(probe, peer, &v1_response(&manifest_cid(&hex_a())))
                .unwrap_err(),
            CorrelationError::UnknownRequestId,
            "one domain must not answer another domain's request id"
        );
        assert_eq!(t.len(), 1, "the real record is untouched");
    }

    // ── Shape and subject matching ───────────────────────────────────────

    #[test]
    fn a_v2_response_to_a_v1_request_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        let resp = ShardResponseVersioned::V2(ShardResponseV2::ManifestData {
            merkle_root: ROOT_A,
            manifest_bytes: Vec::new(),
            error: None,
        });
        assert_eq!(
            t.correlate_response(key, peer, &resp).unwrap_err(),
            CorrelationError::ResponseKindMismatch {
                expected: "v1-manifest-pull",
                got: "v2-manifest-data",
            }
        );
    }

    #[test]
    fn a_v2_manifest_pull_answered_with_a_push_ack_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(
            key,
            peer,
            OutboundRequestKind::V2ManifestPull {
                merkle_root: ROOT_A,
            },
        )
        .unwrap();
        let resp = ShardResponseVersioned::V2(ShardResponseV2::ManifestPushAck {
            merkle_root: ROOT_A,
            error: None,
        });
        assert_eq!(
            t.correlate_response(key, peer, &resp).unwrap_err(),
            CorrelationError::ResponseKindMismatch {
                expected: "v2-manifest-pull",
                got: "v2-manifest-push-ack",
            }
        );
    }

    #[test]
    fn a_v2_manifest_pull_answered_for_another_root_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(
            key,
            peer,
            OutboundRequestKind::V2ManifestPull {
                merkle_root: ROOT_A,
            },
        )
        .unwrap();
        let resp = ShardResponseVersioned::V2(ShardResponseV2::ManifestData {
            merkle_root: ROOT_B,
            manifest_bytes: Vec::new(),
            error: None,
        });
        assert_eq!(
            t.correlate_response(key, peer, &resp).unwrap_err(),
            CorrelationError::RootMismatch {
                expected: hex_a(),
                got: hex_b(),
            }
        );
    }

    #[test]
    fn a_push_ack_for_another_chunk_index_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(
            key,
            peer,
            OutboundRequestKind::V2Push {
                merkle_root: ROOT_A,
                chunk_index: 7,
            },
        )
        .unwrap();
        let resp = ShardResponseVersioned::V2(ShardResponseV2::PushAck {
            merkle_root: ROOT_A,
            chunk_index: 8,
            error: None,
        });
        assert_eq!(
            t.correlate_response(key, peer, &resp).unwrap_err(),
            CorrelationError::ChunkIndexMismatch {
                expected: 7,
                got: 8
            }
        );
    }

    #[test]
    fn a_v2_pull_answered_for_another_cid_is_refused() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(
            key,
            peer,
            OutboundRequestKind::V2Pull {
                cid: "asked".into(),
            },
        )
        .unwrap();
        let resp = ShardResponseVersioned::V2(ShardResponseV2::Data {
            cid: "answered".into(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: None,
        });
        assert_eq!(
            t.correlate_response(key, peer, &resp).unwrap_err(),
            CorrelationError::CidMismatch {
                expected: "asked".into(),
                got: "answered".into(),
            }
        );
    }

    /// Every V2 request kind must accept its own matching response, so the
    /// matcher is proved to be a filter and not a blanket refusal.
    #[test]
    fn every_request_kind_accepts_its_own_response() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();

        let pairs: Vec<(OutboundRequestKind, ShardResponseVersioned)> = vec![
            (
                OutboundRequestKind::V1Chunk { cid: "c".into() },
                v1_response("c"),
            ),
            (
                OutboundRequestKind::V1Push { cid: "c".into() },
                v1_response("c"),
            ),
            (
                manifest_pull_kind(&hex_a()),
                v1_response(&manifest_cid(&hex_a())),
            ),
            (
                OutboundRequestKind::V2Pull { cid: "c".into() },
                ShardResponseVersioned::V2(ShardResponseV2::Data {
                    cid: "c".into(),
                    offset: 0,
                    total_bytes: 0,
                    data: Vec::new(),
                    error: None,
                }),
            ),
            (
                OutboundRequestKind::V2Push {
                    merkle_root: ROOT_A,
                    chunk_index: 3,
                },
                ShardResponseVersioned::V2(ShardResponseV2::PushAck {
                    merkle_root: ROOT_A,
                    chunk_index: 3,
                    error: None,
                }),
            ),
            (
                OutboundRequestKind::V2ManifestPush {
                    merkle_root: ROOT_A,
                },
                ShardResponseVersioned::V2(ShardResponseV2::ManifestPushAck {
                    merkle_root: ROOT_A,
                    error: None,
                }),
            ),
            (
                OutboundRequestKind::V2ManifestPull {
                    merkle_root: ROOT_A,
                },
                ShardResponseVersioned::V2(ShardResponseV2::ManifestData {
                    merkle_root: ROOT_A,
                    manifest_bytes: vec![1],
                    error: None,
                }),
            ),
        ];

        for (kind, resp) in pairs {
            let key = mint(&mut b, peer);
            t.record(key, peer, kind.clone()).unwrap();
            assert!(
                t.correlate_response(key, peer, &resp).is_ok(),
                "{kind:?} must accept its own response"
            );
        }
        assert_eq!(t.len(), 0);
    }

    // ── Terminal paths ───────────────────────────────────────────────────

    #[test]
    fn an_outbound_failure_removes_the_record() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert_eq!(t.len(), 1);

        let FailureOutcome::Retained { peer: from, kind } = t.on_failure(key) else {
            panic!("the record was there");
        };
        assert_eq!(from, peer, "the peer comes from the record");
        assert_eq!(kind.label(), "v1-manifest-pull");
        assert_eq!(t.len(), 0);
        assert!(
            matches!(t.on_failure(key), FailureOutcome::Unknown),
            "failure is idempotent"
        );

        assert_eq!(
            t.correlate_response(key, peer, &v1_response(&manifest_cid(&hex_a())))
                .unwrap_err(),
            CorrelationError::UnknownRequestId,
            "a failed request must not be answerable afterwards"
        );
    }

    #[test]
    fn the_reaper_bounds_the_pending_set() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();

        for _ in 0..5 {
            let key = mint(&mut b, peer);
            t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        }
        assert_eq!(t.len(), 5);

        // Nothing is stale yet.
        assert_eq!(t.reap(Instant::now(), OUTBOUND_RECORD_TTL), 0);
        assert_eq!(t.len(), 5);

        // A clock far enough ahead retires all of them.
        let later = Instant::now() + OUTBOUND_RECORD_TTL + Duration::from_secs(1);
        assert_eq!(t.reap(later, OUTBOUND_RECORD_TTL), 5);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn error_messages_name_both_sides() {
        let e = CorrelationError::CidMismatch {
            expected: manifest_cid(&hex_a()),
            got: manifest_cid(&hex_b()),
        };
        let s = e.to_string();
        assert!(s.contains(&hex_a()), "{s}");
        assert!(s.contains(&hex_b()), "{s}");
    }

    // ── Several requests in flight to one peer ───────────────────────────

    /// The failure case the peer id alone cannot express.
    ///
    /// Two requests are outstanding to the *same* peer. One fails. A consumer
    /// that keys off the peer would settle both — abandoning a healthy
    /// transfer and re-issuing it. The retained identity settles exactly one,
    /// and the other stays outstanding and still correlates afterwards.
    #[test]
    fn one_of_two_requests_to_one_peer_failing_settles_only_that_one() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();

        let doomed = mint(&mut b, peer);
        let healthy = mint(&mut b, peer);
        assert_ne!(doomed, healthy, "two sends must mint distinct ids");

        t.record(
            doomed,
            peer,
            OutboundRequestKind::V1Chunk {
                cid: "cid-doomed".into(),
            },
        )
        .unwrap();
        t.record(
            healthy,
            peer,
            OutboundRequestKind::V1Chunk {
                cid: "cid-healthy".into(),
            },
        )
        .unwrap();
        assert_eq!(t.len(), 2);

        // The doomed one fails. The failure names it, and only it — and the
        // peer it names comes from the record, not from the failure event.
        let FailureOutcome::Retained {
            peer: failed_peer,
            kind: failed_kind,
        } = t.on_failure(doomed)
        else {
            panic!("the record was there");
        };
        assert_eq!(failed_peer, peer);
        assert_eq!(
            OutboundOrigin::new(failed_peer, failed_kind).requested_cid(),
            Some("cid-doomed"),
            "the failure must identify the exact request, not merely the peer"
        );
        assert_eq!(t.len(), 1, "the other request is still outstanding");

        // The healthy one still correlates — it was not collateral damage.
        let origin = t
            .correlate_response(healthy, peer, &v1_response("cid-healthy"))
            .expect("the surviving request must still correlate");
        assert_eq!(origin.requested_cid(), Some("cid-healthy"));
        assert_eq!(t.len(), 0);
    }

    /// The same shape on the response side: a response to one of two
    /// concurrent requests must not be matched against the other's identity.
    #[test]
    fn two_requests_to_one_peer_do_not_answer_for_each_other() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();

        let first = mint(&mut b, peer);
        let second = mint(&mut b, peer);
        t.record(first, peer, manifest_pull_kind(&hex_a())).unwrap();
        t.record(second, peer, manifest_pull_kind(&hex_b()))
            .unwrap();

        // The reply for root B arrives carrying the first request's id.
        assert_eq!(
            t.correlate_response(first, peer, &v1_response(&manifest_cid(&hex_b())))
                .unwrap_err(),
            CorrelationError::CidMismatch {
                expected: manifest_cid(&hex_a()),
                got: manifest_cid(&hex_b()),
            }
        );
        // And the second request is untouched by that rejection.
        assert_eq!(t.len(), 1);
        let origin = t
            .correlate_response(second, peer, &v1_response(&manifest_cid(&hex_b())))
            .unwrap();
        assert_eq!(origin.requested_manifest_root_hex(), Some(hex_b().as_str()));
    }

    // ── Collisions ───────────────────────────────────────────────────────

    // ── Collisions poison the key ────────────────────────────────────────

    /// A reused request id must not leave *either* identity usable.
    ///
    /// Keeping the first would not be fail-closed: both requests are on the
    /// wire under the same key, so a response to the second would be matched
    /// against the first's retained identity. The key is poisoned instead and
    /// both requests are abandoned.
    ///
    /// Identical semantics in debug and release — there is no `debug_assert`
    /// deciding safety, so this test runs in both profiles.
    #[test]
    fn a_reused_request_id_poisons_the_key_and_keeps_neither_identity() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        let err = t
            .record(
                key,
                peer,
                OutboundRequestKind::V1Chunk {
                    cid: "intruder".into(),
                },
            )
            .unwrap_err();
        assert_eq!(err.first, "v1-manifest-pull");
        assert_eq!(err.second, "v1-chunk-pull");
        assert_eq!(t.poisoned_len(), 1, "the key must be poisoned");

        // The FIRST request's own response is now refused too. That is the
        // point: the first identity is not privileged.
        assert!(matches!(
            t.correlate_response(key, peer, &v1_response(&manifest_cid(&hex_a())))
                .unwrap_err(),
            CorrelationError::PoisonedRequestId { .. }
        ));
        // And so is the second's.
        assert!(matches!(
            t.correlate_response(key, peer, &v1_response("intruder"))
                .unwrap_err(),
            CorrelationError::PoisonedRequestId { .. }
        ));
        // The poison survives both refusals.
        assert_eq!(t.poisoned_len(), 1);
    }

    /// Both arrival orders, with the responses made **identical in shape and
    /// subject** so that a tracker keeping either identity would accept one of
    /// them. Neither may be accepted, whichever arrives first.
    #[test]
    fn neither_colliding_request_is_answerable_in_either_arrival_order() {
        // The two requests are the same kind for the same root, so the single
        // response below is a correct answer to *both* — exactly the case a
        // keep-one policy gets wrong.
        let shared = v1_response(&manifest_cid(&hex_a()));

        for first_then_second in [true, false] {
            let peer = PeerId::random();
            let mut b = behaviour();
            let mut t = OutboundTracker::default();
            let key = mint(&mut b, peer);

            let a_kind = manifest_pull_kind(&hex_a());
            let b_kind = manifest_pull_kind(&hex_a());
            assert_eq!(a_kind, b_kind, "fixture: the two requests are identical");

            if first_then_second {
                t.record(key, peer, a_kind).unwrap();
                assert!(t.record(key, peer, b_kind).is_err());
            } else {
                t.record(key, peer, b_kind).unwrap();
                assert!(t.record(key, peer, a_kind).is_err());
            }

            // Two terminal events arrive, one per request. Both refused.
            for attempt in 0..2 {
                assert!(
                    matches!(
                        t.correlate_response(key, peer, &shared).unwrap_err(),
                        CorrelationError::PoisonedRequestId { .. }
                    ),
                    "attempt {attempt} was accepted (first_then_second = {first_then_second})"
                );
            }
            assert!(matches!(t.on_failure(key), FailureOutcome::Poisoned));
            assert_eq!(t.poisoned_len(), 1);
        }
    }

    /// A failure for a poisoned key is refused too, and does not free it.
    #[test]
    fn a_failure_for_a_poisoned_key_is_refused_and_leaves_it_poisoned() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert!(t.record(key, peer, manifest_pull_kind(&hex_b())).is_err());

        assert!(matches!(t.on_failure(key), FailureOutcome::Poisoned));
        assert!(matches!(t.on_failure(key), FailureOutcome::Poisoned));
        assert_eq!(t.poisoned_len(), 1, "a failure must not free the poison");
    }

    /// Only the reaper clears a poison, and only after `POISON_TTL` — which is
    /// longer than the ordinary record TTL, so a poisoned key outlives every
    /// terminal event the two colliding requests can produce.
    #[test]
    fn only_the_reaper_clears_a_poison_and_only_after_the_poison_ttl() {
        assert!(
            POISON_TTL > OUTBOUND_RECORD_TTL,
            "a poison must outlive an ordinary pending record"
        );

        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);
        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert!(t.record(key, peer, manifest_pull_kind(&hex_b())).is_err());

        // The ordinary record TTL does not touch it.
        let mid = Instant::now() + OUTBOUND_RECORD_TTL + Duration::from_secs(1);
        assert_eq!(t.reap(mid, OUTBOUND_RECORD_TTL), 0);
        assert_eq!(t.poisoned_len(), 1);

        let later = Instant::now() + POISON_TTL + Duration::from_secs(1);
        assert_eq!(t.reap(later, OUTBOUND_RECORD_TTL), 1);
        assert_eq!(t.poisoned_len(), 0);
        assert_eq!(t.len(), 0);
    }

    /// A key freed by a terminal event may be filed again — the refusal is
    /// about *live* collisions, not about the id ever having been used.
    #[test]
    fn a_key_may_be_refiled_after_it_is_spent() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, peer);

        t.record(key, peer, manifest_pull_kind(&hex_a())).unwrap();
        assert!(matches!(t.on_failure(key), FailureOutcome::Retained { .. }));
        t.record(key, peer, manifest_pull_kind(&hex_b()))
            .expect("a spent key is free to reuse");
        assert_eq!(t.len(), 1);
        assert_eq!(t.poisoned_len(), 0);
    }

    // ── Failure identity comes from the record, not the event ────────────

    /// `on_failure` must hand back the peer recorded at send time, so the
    /// caller can compare it with the peer the libp2p event carries and build
    /// the origin from retained state alone.
    #[test]
    fn a_failure_yields_the_peer_recorded_at_send_time() {
        let asked = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, asked);
        t.record(key, asked, manifest_pull_kind(&hex_a())).unwrap();

        let FailureOutcome::Retained { peer, kind } = t.on_failure(key) else {
            panic!("the record was there");
        };
        assert_eq!(peer, asked, "the peer must come from the record");
        assert_eq!(kind.label(), "v1-manifest-pull");

        // Built from retained state only — this is the value the event carries.
        let origin = OutboundOrigin::new(peer, kind);
        assert_eq!(origin.peer(), asked);
        assert_eq!(origin.requested_manifest_root_hex(), Some(hex_a().as_str()));
        assert_eq!(t.len(), 0);
    }

    /// A failure event naming a different peer than the record is detectable,
    /// which is what lets the swarm drop it instead of attributing it.
    #[test]
    fn a_failure_from_an_unexpected_peer_is_detectable_from_retained_state() {
        let asked = PeerId::random();
        let other = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let key = mint(&mut b, asked);
        t.record(
            key,
            asked,
            OutboundRequestKind::V1Chunk {
                cid: "cid-a".into(),
            },
        )
        .unwrap();

        let FailureOutcome::Retained { peer, .. } = t.on_failure(key) else {
            panic!("the record was there");
        };
        assert_ne!(
            peer, other,
            "the retained peer must not equal the event's peer, so the \
             caller's comparison can reject it"
        );
        assert_eq!(peer, asked);
    }

    #[test]
    fn an_unknown_key_is_distinguishable_from_a_poisoned_one() {
        let peer = PeerId::random();
        let mut b = behaviour();
        let mut t = OutboundTracker::default();
        let fresh = mint(&mut b, peer);
        assert!(matches!(t.on_failure(fresh), FailureOutcome::Unknown));

        let poisoned = mint(&mut b, peer);
        t.record(poisoned, peer, manifest_pull_kind(&hex_a()))
            .unwrap();
        assert!(
            t.record(poisoned, peer, manifest_pull_kind(&hex_b()))
                .is_err()
        );
        assert!(matches!(t.on_failure(poisoned), FailureOutcome::Poisoned));
    }
}
