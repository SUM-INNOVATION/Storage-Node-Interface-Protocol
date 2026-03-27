// Core storage types for the SUM Storage Node Protocol.
//
// These types represent the off-chain counterparts to the L1's StorageMetadata.
// CHUNK_SIZE must match the L1 constant (1 MB = 1_048_576 bytes).

use serde::{Deserialize, Serialize};

/// Fixed chunk size matching the SUM Chain L1 constant.
/// Every file is sliced into uniform 1 MB chunks (last chunk may be smaller).
pub const CHUNK_SIZE: u64 = 1_048_576;

/// Replication factor: each chunk is stored on this many nodes.
/// Must match the L1 constant in sum-chain/crates/primitives/src/storage_metadata.rs.
pub const REPLICATION_FACTOR: u32 = 3;

// ── Chunk Descriptor ─────────────────────────────────────────────────────────

/// Descriptor for a single chunk of a chunked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkDescriptor {
    /// Zero-based index of this chunk within the file.
    pub chunk_index: u32,
    /// Byte offset within the original file.
    pub offset: u64,
    /// Actual byte count (last chunk may be < CHUNK_SIZE).
    pub size: u64,
    /// BLAKE3 hash of this chunk's data (32 bytes).
    pub blake3_hash: [u8; 32],
    /// CIDv1 string (BLAKE3, raw codec) for content addressing.
    pub cid: String,
}

// ── Data Manifest ────────────────────────────────────────────────────────────

/// Complete manifest describing a chunked file.
///
/// This is the off-chain counterpart to the L1's `StorageMetadata`.
/// Serialized as CBOR for on-disk storage; JSON for debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataManifest {
    /// Original file name.
    pub file_name: String,
    /// BLAKE3 hash of the entire original file (32 bytes).
    pub file_hash: [u8; 32],
    /// Total size of the original file in bytes.
    pub total_size_bytes: u64,
    /// Number of 1 MB chunks.
    pub chunk_count: u32,
    /// Root of the Merkle tree over chunk hashes (32 bytes).
    /// This value is sent to the L1 as the file's identity.
    pub merkle_root: [u8; 32],
    /// Ordered list of chunk descriptors.
    pub chunks: Vec<ChunkDescriptor>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_matches_l1() {
        assert_eq!(CHUNK_SIZE, 1_048_576);
    }

    #[test]
    fn chunk_descriptor_serde_round_trip() {
        let cd = ChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: CHUNK_SIZE,
            blake3_hash: [0xaa; 32],
            cid: "bafkr4itest".into(),
        };
        let json = serde_json::to_string(&cd).unwrap();
        let round: ChunkDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(cd, round);
    }

    #[test]
    fn data_manifest_serde_round_trip() {
        let m = DataManifest {
            file_name: "test.bin".into(),
            file_hash: [0xbb; 32],
            total_size_bytes: 2_500_000,
            chunk_count: 3,
            merkle_root: [0xcc; 32],
            chunks: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        let round: DataManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(round.file_name, "test.bin");
        assert_eq!(round.chunk_count, 3);
    }
}
