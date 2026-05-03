// sum-net::codec — request-response codec(s) for the SNIP shard-transfer
// protocols. Transfers chunk data and manifests between peers over the
// libp2p QUIC transport.
//
// Wire format (both V1 and V2): [u32 big-endian length][bincode payload].
// Bincode is used because shard responses carry raw `Vec<u8>` chunk data;
// bincode writes this as length + raw bytes (zero overhead), whereas JSON
// would base64-encode it (+33%) and CBOR adds tagging overhead.
//
// Two protocols coexist:
//
//   * `/sum/storage/v1` — original; carries `ShardRequest` / `ShardResponse`.
//     V1 wire format is bit-compatible with what nodes have been speaking
//     since this crate shipped. Do not change.
//
//   * `/sum/storage/v2` — V2 lifecycle (chain plan v3.2). Carries
//     `ShardRequestV2` / `ShardResponseV2` with explicit Pull / Push /
//     ManifestPush / ManifestPull variants and per-push Merkle proofs.
//
// `VersionedShardCodec` is the per-stream codec that dispatches on the
// libp2p-negotiated protocol name (passed in on every read/write call).
// V1 wire stays bit-compatible: a V1 peer sees the same bytes from
// `VersionedShardCodec` as it always saw from `ShardCodec`.

use std::io;

use async_trait::async_trait;
use futures::prelude::*;
use serde::{Deserialize, Serialize};

/// V1 protocol identifier negotiated via ALPN during substream opening.
pub const SHARD_XFER_PROTOCOL_V1: &str = "/sum/storage/v1";

/// V2 protocol identifier. Carries the chain-plan-v3.2 request/response
/// variants with per-chunk Merkle proofs.
pub const SHARD_XFER_PROTOCOL_V2: &str = "/sum/storage/v2";

/// Backwards-compat alias. Existing call sites that import
/// `SHARD_XFER_PROTOCOL` keep working; new code should use the
/// version-explicit name.
pub const SHARD_XFER_PROTOCOL: &str = SHARD_XFER_PROTOCOL_V1;

/// Safety limit: reject any single message larger than 256 MiB.
/// The actual chunk size is controlled by `StoreConfig::max_shard_msg_bytes`
/// (default 64 MiB); this codec limit is intentionally higher to allow for
/// bincode framing overhead.
const MAX_MSG_BYTES: usize = 256 * 1024 * 1024;

// ── Message Types ─────────────────────────────────────────────────────────────

/// Request for a shard (or sub-chunk thereof), identified by CID.
///
/// When `push_data` is `Some(...)`, this is a **push request**: the sender
/// is proactively delivering chunk data for the receiver to store. The
/// receiver verifies the CID, writes to disk, and responds with an ACK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardRequest {
    /// CIDv1 string identifying the desired shard.
    pub cid: String,
    /// Byte offset within the shard to start reading from.
    /// `None` or `Some(0)` means from the beginning.
    pub offset: Option<u64>,
    /// Maximum bytes to return. `None` means the entire shard.
    /// Used for windowed streaming of large shards.
    pub max_bytes: Option<u64>,
    /// When present, this is a push (store) request carrying chunk data.
    /// The receiver must verify the CID before storing.
    #[serde(default)]
    pub push_data: Option<Vec<u8>>,
}

/// Response carrying shard data (or an error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardResponse {
    /// The CID this response corresponds to.
    pub cid: String,
    /// Byte offset this chunk starts at within the full shard.
    pub offset: u64,
    /// Total shard size in bytes (so the requester knows when it has
    /// received everything and how many sub-chunk requests remain).
    pub total_bytes: u64,
    /// The shard payload (may be a sub-chunk).
    pub data: Vec<u8>,
    /// If present, the request failed — this contains the error message.
    /// When set, `data` is empty.
    pub error: Option<String>,
}

// ── Codec ─────────────────────────────────────────────────────────────────────

/// Codec for the `/sum/shard-xfer/1` request-response protocol.
#[derive(Debug, Clone)]
pub struct ShardCodec {
    max_msg_bytes: usize,
}

impl Default for ShardCodec {
    fn default() -> Self {
        Self {
            max_msg_bytes: MAX_MSG_BYTES,
        }
    }
}

#[async_trait]
impl libp2p::request_response::Codec for ShardCodec {
    type Protocol = String;
    type Request = ShardRequest;
    type Response = ShardResponse;

    async fn read_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let buf = read_length_prefixed(io, self.max_msg_bytes).await?;
        let (req, _) =
            bincode::serde::decode_from_slice(&buf, bincode::config::standard())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(req)
    }

    async fn read_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let buf = read_length_prefixed(io, self.max_msg_bytes).await?;
        let (resp, _) =
            bincode::serde::decode_from_slice(&buf, bincode::config::standard())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(resp)
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let buf = bincode::serde::encode_to_vec(&req, bincode::config::standard())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        write_length_prefixed(io, &buf).await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        resp: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let buf = bincode::serde::encode_to_vec(&resp, bincode::config::standard())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        write_length_prefixed(io, &buf).await
    }
}

// ── V2 Message Types ─────────────────────────────────────────────────────────

/// V2 request shape — distinct variants for each operation, so a chunk
/// push always carries its Merkle proof and a manifest push always
/// carries its CBOR payload, as types. Receivers don't have to do
/// runtime "did the proof field arrive?" checks the way they did when
/// V1 squeezed everything into one struct with `Option<>` fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardRequestV2 {
    /// Pull a chunk by CID. `offset`/`max_bytes` allow windowed reads.
    Pull {
        cid: String,
        offset: u64,
        max_bytes: u64,
    },
    /// Push a chunk with a Merkle proof binding it to a registered
    /// `merkle_root` at a specific `chunk_index`. The receiver verifies
    /// the proof against chain state via the strict
    /// `verify_merkle_proof_bytes_for_tree` and rejects if it doesn't
    /// match. `data` is the raw chunk bytes; the CID is computed from
    /// `blake3(data)` and never trusted from the wire.
    Push {
        data: Vec<u8>,
        merkle_root: [u8; 32],
        chunk_index: u32,
        merkle_path: Vec<[u8; 32]>,
    },
    /// Push a manifest blob (CBOR-encoded `DataManifest`) under
    /// `manifest:<merkle_root>` semantics. Receiver validates internal
    /// consistency via `validate_manifest_push` (recomputed root, CID
    /// ↔ blake3 binding, ordered indices, etc.). Distinct from `Push`
    /// because manifests don't have Merkle proofs of their own — they
    /// ARE the structure that proofs are over.
    ManifestPush {
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
    },
    /// Pull the manifest for `merkle_root`. ACL-gated like any other pull.
    ManifestPull { merkle_root: [u8; 32] },
}

/// V2 response shape, paired with `ShardRequestV2`. Each request variant
/// has its own response variant so the upper layer can match exhaustively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardResponseV2 {
    /// Response to a `Pull` — carries chunk bytes (or an error).
    Data {
        cid: String,
        offset: u64,
        total_bytes: u64,
        data: Vec<u8>,
        error: Option<String>,
    },
    /// Response to a `Push` — empty ACK on success, error string on
    /// validation rejection (proof failure / unknown root / unassigned).
    PushAck {
        merkle_root: [u8; 32],
        chunk_index: u32,
        error: Option<String>,
    },
    /// Response to a `ManifestPush` — empty ACK or error string from
    /// `validate_manifest_push`.
    ManifestPushAck {
        merkle_root: [u8; 32],
        error: Option<String>,
    },
    /// Response to a `ManifestPull` — CBOR manifest bytes (or error).
    ManifestData {
        merkle_root: [u8; 32],
        manifest_bytes: Vec<u8>,
        error: Option<String>,
    },
}

// ── Versioned wrappers ───────────────────────────────────────────────────────

/// Per-stream request type used by `VersionedShardCodec`. The variant
/// carried matches the libp2p-negotiated protocol on this stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardRequestVersioned {
    V1(ShardRequest),
    V2(ShardRequestV2),
}

/// Per-stream response type, mirror of [`ShardRequestVersioned`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardResponseVersioned {
    V1(ShardResponse),
    V2(ShardResponseV2),
}

// ── VersionedShardCodec ──────────────────────────────────────────────────────

/// Codec that dispatches on the libp2p-negotiated protocol name. V1 and
/// V2 streams share one codec instance; the protocol passed on every
/// read/write call selects which inner type to (de)serialize.
///
/// Tests in this module verify (a) V1 wire bytes are bit-compatible
/// with the legacy `ShardCodec`, (b) every V2 variant round-trips, and
/// (c) constructing a V2-shaped request and writing it to a V1 stream
/// (or vice versa) is rejected as a programmer error rather than
/// silently producing junk on the wire.
///
/// **Not yet wired into `SumNet`** — that swap is W5 / receive-side
/// dispatch. Until then this type exists only as a tested building
/// block. The legacy `ShardCodec` is still what the swarm registers.
#[derive(Debug, Clone)]
pub struct VersionedShardCodec {
    max_msg_bytes: usize,
}

impl Default for VersionedShardCodec {
    fn default() -> Self {
        Self {
            max_msg_bytes: MAX_MSG_BYTES,
        }
    }
}

#[async_trait]
impl libp2p::request_response::Codec for VersionedShardCodec {
    type Protocol = String;
    type Request = ShardRequestVersioned;
    type Response = ShardResponseVersioned;

    async fn read_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let buf = read_length_prefixed(io, self.max_msg_bytes).await?;
        match protocol.as_str() {
            SHARD_XFER_PROTOCOL_V1 => {
                let (req, _) = bincode::serde::decode_from_slice::<ShardRequest, _>(
                    &buf,
                    bincode::config::standard(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                Ok(ShardRequestVersioned::V1(req))
            }
            SHARD_XFER_PROTOCOL_V2 => {
                let (req, _) = bincode::serde::decode_from_slice::<ShardRequestV2, _>(
                    &buf,
                    bincode::config::standard(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                Ok(ShardRequestVersioned::V2(req))
            }
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unknown shard-xfer protocol: {other}"),
            )),
        }
    }

    async fn read_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let buf = read_length_prefixed(io, self.max_msg_bytes).await?;
        match protocol.as_str() {
            SHARD_XFER_PROTOCOL_V1 => {
                let (resp, _) = bincode::serde::decode_from_slice::<ShardResponse, _>(
                    &buf,
                    bincode::config::standard(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                Ok(ShardResponseVersioned::V1(resp))
            }
            SHARD_XFER_PROTOCOL_V2 => {
                let (resp, _) = bincode::serde::decode_from_slice::<ShardResponseV2, _>(
                    &buf,
                    bincode::config::standard(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                Ok(ShardResponseVersioned::V2(resp))
            }
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unknown shard-xfer protocol: {other}"),
            )),
        }
    }

    async fn write_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let buf = match (protocol.as_str(), req) {
            (SHARD_XFER_PROTOCOL_V1, ShardRequestVersioned::V1(r)) => {
                bincode::serde::encode_to_vec(&r, bincode::config::standard())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            }
            (SHARD_XFER_PROTOCOL_V2, ShardRequestVersioned::V2(r)) => {
                bincode::serde::encode_to_vec(&r, bincode::config::standard())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            }
            (proto, ShardRequestVersioned::V1(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("V1 request written to non-V1 stream (protocol={proto})"),
                ));
            }
            (proto, ShardRequestVersioned::V2(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("V2 request written to non-V2 stream (protocol={proto})"),
                ));
            }
        };
        write_length_prefixed(io, &buf).await
    }

    async fn write_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        resp: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let buf = match (protocol.as_str(), resp) {
            (SHARD_XFER_PROTOCOL_V1, ShardResponseVersioned::V1(r)) => {
                bincode::serde::encode_to_vec(&r, bincode::config::standard())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            }
            (SHARD_XFER_PROTOCOL_V2, ShardResponseVersioned::V2(r)) => {
                bincode::serde::encode_to_vec(&r, bincode::config::standard())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            }
            (proto, ShardResponseVersioned::V1(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("V1 response written to non-V1 stream (protocol={proto})"),
                ));
            }
            (proto, ShardResponseVersioned::V2(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("V2 response written to non-V2 stream (protocol={proto})"),
                ));
            }
        };
        write_length_prefixed(io, &buf).await
    }
}

// ── Wire Helpers ──────────────────────────────────────────────────────────────

/// Read a `[u32 BE length][payload]` frame.
async fn read_length_prefixed<T>(io: &mut T, max_bytes: usize) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes (max {max_bytes})"),
        ));
    }
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a `[u32 BE length][payload]` frame.
async fn write_length_prefixed<T>(io: &mut T, data: &[u8]) -> io::Result<()>
where
    T: AsyncWrite + Unpin + Send,
{
    let len = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload exceeds u32::MAX: {} bytes", data.len()),
        )
    })?;
    io.write_all(&len.to_be_bytes()).await?;
    io.write_all(data).await?;
    io.flush().await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;
    use libp2p::request_response::Codec;

    #[tokio::test]
    async fn request_round_trip() {
        let mut codec = ShardCodec::default();
        let req = ShardRequest {
            cid: "bafkr4itest".into(),
            offset: Some(1024),
            max_bytes: Some(65536),
            push_data: None,
        };

        let mut buf = Vec::new();
        codec
            .write_request(&String::new(), &mut Cursor::new(&mut buf), req.clone())
            .await
            .unwrap();

        let decoded = codec
            .read_request(&String::new(), &mut Cursor::new(&buf))
            .await
            .unwrap();

        assert_eq!(decoded.cid, req.cid);
        assert_eq!(decoded.offset, req.offset);
        assert_eq!(decoded.max_bytes, req.max_bytes);
        assert!(decoded.push_data.is_none());
    }

    #[tokio::test]
    async fn push_request_round_trip() {
        let mut codec = ShardCodec::default();
        let push_payload = vec![0xDE; 8192];
        let req = ShardRequest {
            cid: "bafkr4ipush".into(),
            offset: None,
            max_bytes: None,
            push_data: Some(push_payload.clone()),
        };

        let mut buf = Vec::new();
        codec
            .write_request(&String::new(), &mut Cursor::new(&mut buf), req.clone())
            .await
            .unwrap();

        let decoded = codec
            .read_request(&String::new(), &mut Cursor::new(&buf))
            .await
            .unwrap();

        assert_eq!(decoded.cid, "bafkr4ipush");
        assert!(decoded.push_data.is_some());
        assert_eq!(decoded.push_data.unwrap(), push_payload);
    }

    #[tokio::test]
    async fn response_round_trip() {
        let mut codec = ShardCodec::default();
        let resp = ShardResponse {
            cid: "bafkr4itest".into(),
            offset: 0,
            total_bytes: 1_000_000,
            data: vec![0xAB; 4096],
            error: None,
        };

        let mut buf = Vec::new();
        codec
            .write_response(&String::new(), &mut Cursor::new(&mut buf), resp.clone())
            .await
            .unwrap();

        let decoded = codec
            .read_response(&String::new(), &mut Cursor::new(&buf))
            .await
            .unwrap();

        assert_eq!(decoded.cid, resp.cid);
        assert_eq!(decoded.offset, resp.offset);
        assert_eq!(decoded.total_bytes, resp.total_bytes);
        assert_eq!(decoded.data.len(), 4096);
        assert_eq!(decoded.data[0], 0xAB);
        assert!(decoded.error.is_none());
    }

    #[tokio::test]
    async fn error_response_round_trip() {
        let mut codec = ShardCodec::default();
        let resp = ShardResponse {
            cid: "bafkr4imissing".into(),
            offset: 0,
            total_bytes: 0,
            data: Vec::new(),
            error: Some("shard not found".into()),
        };

        let mut buf = Vec::new();
        codec
            .write_response(&String::new(), &mut Cursor::new(&mut buf), resp.clone())
            .await
            .unwrap();

        let decoded = codec
            .read_response(&String::new(), &mut Cursor::new(&buf))
            .await
            .unwrap();

        assert_eq!(decoded.error.as_deref(), Some("shard not found"));
        assert!(decoded.data.is_empty());
    }

    #[tokio::test]
    async fn rejects_oversized_message() {
        let mut codec = ShardCodec {
            max_msg_bytes: 16, // tiny limit for test
        };

        // Fabricate a frame claiming 1000 bytes.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 1000]);

        let result = codec
            .read_request(&String::new(), &mut Cursor::new(&buf))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("message too large"));
    }

    // ── VersionedShardCodec / V2 wire tests ───────────────────────────────────

    /// V1 frames written by `VersionedShardCodec` on a V1 stream are
    /// **byte-for-byte identical** to frames written by the legacy
    /// `ShardCodec`. This is the wire-compat guarantee that lets V2
    /// peers talk to V1 peers without breaking existing deployments.
    #[tokio::test]
    async fn v1_wire_bytes_match_legacy_codec() {
        let req = ShardRequest {
            cid: "bafkr4iv1compat".into(),
            offset: Some(0),
            max_bytes: Some(1024),
            push_data: None,
        };

        let mut legacy = ShardCodec::default();
        let mut legacy_bytes = Vec::new();
        legacy
            .write_request(
                &SHARD_XFER_PROTOCOL_V1.to_string(),
                &mut Cursor::new(&mut legacy_bytes),
                req.clone(),
            )
            .await
            .unwrap();

        let mut versioned = VersionedShardCodec::default();
        let mut v_bytes = Vec::new();
        versioned
            .write_request(
                &SHARD_XFER_PROTOCOL_V1.to_string(),
                &mut Cursor::new(&mut v_bytes),
                ShardRequestVersioned::V1(req),
            )
            .await
            .unwrap();

        assert_eq!(legacy_bytes, v_bytes, "V1 wire compat broken");
    }

    /// V2 Pull / Push / ManifestPush / ManifestPull all round-trip
    /// through the versioned codec on a V2 stream.
    #[tokio::test]
    async fn v2_request_round_trips_all_variants() {
        let cases = [
            ShardRequestV2::Pull {
                cid: "bafkr4ipull".into(),
                offset: 0,
                max_bytes: 1024,
            },
            ShardRequestV2::Push {
                data: vec![0xCC; 1024],
                merkle_root: [0xAB; 32],
                chunk_index: 17,
                merkle_path: vec![[0x01; 32], [0x02; 32], [0x03; 32]],
            },
            ShardRequestV2::ManifestPush {
                merkle_root: [0xDE; 32],
                manifest_bytes: vec![0xEF; 4096],
            },
            ShardRequestV2::ManifestPull {
                merkle_root: [0xFA; 32],
            },
        ];

        for req in cases {
            let mut codec = VersionedShardCodec::default();
            let mut buf = Vec::new();
            codec
                .write_request(
                    &SHARD_XFER_PROTOCOL_V2.to_string(),
                    &mut Cursor::new(&mut buf),
                    ShardRequestVersioned::V2(req.clone()),
                )
                .await
                .unwrap();

            let decoded = codec
                .read_request(&SHARD_XFER_PROTOCOL_V2.to_string(), &mut Cursor::new(&buf))
                .await
                .unwrap();

            match (decoded, req.clone()) {
                (ShardRequestVersioned::V2(got), expected) => {
                    let got_dbg = format!("{got:?}");
                    let exp_dbg = format!("{expected:?}");
                    assert_eq!(got_dbg, exp_dbg, "V2 request round-trip mismatch");
                }
                (other, _) => panic!("V2 round-trip yielded non-V2 variant: {other:?}"),
            }
        }
    }

    /// V2 Data / PushAck / ManifestPushAck / ManifestData all round-trip.
    #[tokio::test]
    async fn v2_response_round_trips_all_variants() {
        let cases = [
            ShardResponseV2::Data {
                cid: "bafkr4ipull".into(),
                offset: 0,
                total_bytes: 1024,
                data: vec![0xAB; 512],
                error: None,
            },
            ShardResponseV2::PushAck {
                merkle_root: [0xAB; 32],
                chunk_index: 17,
                error: None,
            },
            ShardResponseV2::PushAck {
                merkle_root: [0xAB; 32],
                chunk_index: 17,
                error: Some("merkle proof failed".into()),
            },
            ShardResponseV2::ManifestPushAck {
                merkle_root: [0xDE; 32],
                error: None,
            },
            ShardResponseV2::ManifestData {
                merkle_root: [0xFA; 32],
                manifest_bytes: vec![0xCD; 1234],
                error: None,
            },
        ];

        for resp in cases {
            let mut codec = VersionedShardCodec::default();
            let mut buf = Vec::new();
            codec
                .write_response(
                    &SHARD_XFER_PROTOCOL_V2.to_string(),
                    &mut Cursor::new(&mut buf),
                    ShardResponseVersioned::V2(resp.clone()),
                )
                .await
                .unwrap();

            let decoded = codec
                .read_response(&SHARD_XFER_PROTOCOL_V2.to_string(), &mut Cursor::new(&buf))
                .await
                .unwrap();

            match decoded {
                ShardResponseVersioned::V2(got) => {
                    let got_dbg = format!("{got:?}");
                    let exp_dbg = format!("{resp:?}");
                    assert_eq!(got_dbg, exp_dbg, "V2 response round-trip mismatch");
                }
                other => panic!("V2 round-trip yielded non-V2 variant: {other:?}"),
            }
        }
    }

    /// Programmer-error guard: writing a V2-shaped request to a V1 stream
    /// (or vice versa) is rejected at write time, not silently encoded
    /// as garbage on the wire.
    #[tokio::test]
    async fn rejects_variant_protocol_mismatch_on_write() {
        let mut codec = VersionedShardCodec::default();

        // V2 variant on V1 protocol → reject.
        let mut buf = Vec::new();
        let result = codec
            .write_request(
                &SHARD_XFER_PROTOCOL_V1.to_string(),
                &mut Cursor::new(&mut buf),
                ShardRequestVersioned::V2(ShardRequestV2::ManifestPull {
                    merkle_root: [0; 32],
                }),
            )
            .await;
        assert!(result.is_err(), "expected error for V2-on-V1");
        assert!(
            result.unwrap_err().to_string().contains("V2 request written to non-V2"),
            "wrong error message"
        );

        // V1 variant on V2 protocol → reject.
        let mut buf = Vec::new();
        let result = codec
            .write_request(
                &SHARD_XFER_PROTOCOL_V2.to_string(),
                &mut Cursor::new(&mut buf),
                ShardRequestVersioned::V1(ShardRequest {
                    cid: "x".into(),
                    offset: None,
                    max_bytes: None,
                    push_data: None,
                }),
            )
            .await;
        assert!(result.is_err(), "expected error for V1-on-V2");
        assert!(
            result.unwrap_err().to_string().contains("V1 request written to non-V1"),
            "wrong error message"
        );
    }

    /// Reading from a stream that negotiated an unknown protocol is
    /// rejected with `Unsupported`. (libp2p's own protocol negotiation
    /// usually filters this out, but the codec is a defensive layer.)
    #[tokio::test]
    async fn rejects_unknown_protocol() {
        let mut codec = VersionedShardCodec::default();

        // Write a well-formed length-prefixed empty payload, then try
        // to read it under an unknown protocol name.
        let mut buf = Vec::new();
        write_length_prefixed(&mut Cursor::new(&mut buf), &[0u8; 4])
            .await
            .unwrap();

        let result = codec
            .read_request(&"/sum/storage/v99".to_string(), &mut Cursor::new(&buf))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown shard-xfer protocol"),
            "wrong error message"
        );
    }
}
