//! Gossipsub chunk availability announcements.
//!
//! When a node ingests a file it publishes one [`ChunkAnnouncement`] per chunk
//! on the `sum/storage/v1` topic so that other nodes learn which peer holds
//! which chunks.

use serde::{Deserialize, Serialize};

/// Message published on `sum/storage/v1` after ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkAnnouncement {
    /// Hex-encoded Merkle root identifying the file.
    pub merkle_root: String,
    /// Zero-based chunk index within the file.
    pub chunk_index: u32,
    /// CIDv1 identifying this chunk.
    pub chunk_cid: String,
    /// Size of the chunk data in bytes.
    pub size_bytes: u64,
}

/// Encode a [`ChunkAnnouncement`] to bincode bytes for Gossipsub publishing.
pub fn encode_announcement(ann: &ChunkAnnouncement) -> Vec<u8> {
    bincode::serde::encode_to_vec(ann, bincode::config::standard())
        .expect("ChunkAnnouncement serialization is infallible")
}

/// Decode a [`ChunkAnnouncement`] from bincode bytes received via Gossipsub.
pub fn decode_announcement(data: &[u8]) -> Option<ChunkAnnouncement> {
    bincode::serde::decode_from_slice(data, bincode::config::standard())
        .ok()
        .map(|(ann, _)| ann)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_round_trip() {
        let ann = ChunkAnnouncement {
            merkle_root: "a".repeat(64),
            chunk_index: 0,
            chunk_cid: "bafkr4itest".into(),
            size_bytes: 1_048_576,
        };
        let bytes = encode_announcement(&ann);
        let decoded = decode_announcement(&bytes).unwrap();
        assert_eq!(decoded.merkle_root, ann.merkle_root);
        assert_eq!(decoded.chunk_index, ann.chunk_index);
        assert_eq!(decoded.chunk_cid, ann.chunk_cid);
        assert_eq!(decoded.size_bytes, ann.size_bytes);
    }
}
