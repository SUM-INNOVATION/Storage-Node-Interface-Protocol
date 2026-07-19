// Core storage types for the SUM Storage Node Protocol.
//
// These types represent the off-chain counterparts to the L1's StorageMetadata.
// CHUNK_SIZE must match the L1 constant (1 MB = 1_048_576 bytes).

use serde::{Deserialize, Serialize};

/// Fixed chunk size matching the SUM Chain L1 constant.
/// Every file is sliced into uniform 1 MB chunks (last chunk may be smaller).
///
/// Re-exported from the published `sumchain-wire` crate (the single home of
/// the chain's frozen wire constants) so SNIP's chunk size cannot drift from
/// the L1. The shared wire constant is `u64` and equals `1_048_576`; the
/// `pub use` preserves the exact name, `u64` type, and `pub` visibility so
/// all existing call sites are untouched.
pub use sumchain_wire::storage_metadata::CHUNK_SIZE;

/// Fallback replication factor used ONLY by dev-profile paths when
/// `chain_getChainParams` is unreachable. Every production runtime path
/// MUST read `ChainParamsInfo::assignment_replication_factor` at runtime
/// and thread it through
/// `sum_node::runtime_params::RuntimeChainParams`. See
/// `docs/architecture/chain-integration.md` for the eligibility
/// contract and R plumbing story.
///
/// The mainnet chain currently ships this value as `3`; other
/// deployments may configure any `u32`. Chain-side assignment uses
/// `min(configured_r, active_snapshot.len())` and returns empty for
/// `r == 0` (upstream
/// `sum-chain/crates/primitives/src/storage_metadata.rs:299-302`);
/// SNIP mirrors that domain exactly, so this constant is not
/// consulted for validation.
pub const DEFAULT_REPLICATION_FACTOR: u32 = 3;

/// Deprecated alias for [`DEFAULT_REPLICATION_FACTOR`]. Kept one
/// release cycle to accommodate downstream code that pinned the old
/// name. Production code paths must NOT consume it; a repository
/// audit at release time confirms that only the constant definition
/// site, `RuntimeChainParams::dev_fallback`, and `#[cfg(test)]` code
/// reference either name.
#[deprecated(
    note = "renamed to DEFAULT_REPLICATION_FACTOR; production code MUST read \
            ChainParamsInfo::assignment_replication_factor at runtime. Only \
            sum_node::runtime_params::RuntimeChainParams::dev_fallback may \
            consume DEFAULT_REPLICATION_FACTOR."
)]
pub const REPLICATION_FACTOR: u32 = DEFAULT_REPLICATION_FACTOR;

// ── Chunk Descriptor ─────────────────────────────────────────────────────────

/// Descriptor for a single chunk of a chunked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkDescriptor {
    /// Zero-based index of this chunk within the file.
    pub chunk_index: u32,
    /// Byte offset within the original file.
    pub offset: u64,
    /// Actual byte count (last chunk may be < CHUNK_SIZE).
    ///
    /// For Public files this is plaintext bytes; for Private (V2) files
    /// this is the *ciphertext* size (plaintext + 16-byte AEAD tag), so
    /// `chunk_count == ceil(stored_size_bytes / CHUNK_SIZE)` holds for
    /// both visibilities — the chain rule is uniform.
    pub size: u64,
    /// BLAKE3 hash of this chunk's stored bytes (32 bytes).
    ///
    /// For Public this is the plaintext hash; for Private this is the
    /// hash of the *ciphertext* chunk that lives on disk and is fetched
    /// over the wire. This is what the chain commits to and what
    /// serving nodes prove against the file's Merkle root.
    pub blake3_hash: [u8; 32],
    /// CIDv1 string (BLAKE3, raw codec) for content addressing.
    pub cid: String,
    /// BLAKE3 hash of this chunk's *plaintext* bytes (Private files only).
    ///
    /// Used by the downloader to verify decrypted content end-to-end.
    /// `None` for Public files (where `blake3_hash` already covers the
    /// plaintext) and absent on the wire to keep Public manifests
    /// byte-identical to the pre-V2 format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plaintext_blake3_hash: Option<[u8; 32]>,
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
    fn chunk_size_matches_wire() {
        // The re-export must resolve to the wire crate's constant with no
        // width or value drift: `u64` and exactly 1 MiB.
        assert_eq!(CHUNK_SIZE, sumchain_wire::storage_metadata::CHUNK_SIZE);
        let _: u64 = CHUNK_SIZE;
        assert_eq!(CHUNK_SIZE, 1_048_576u64);
    }

    #[test]
    fn chunk_descriptor_serde_round_trip() {
        let cd = ChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: CHUNK_SIZE,
            blake3_hash: [0xaa; 32],
            cid: "bafkr4itest".into(),
            plaintext_blake3_hash: None,
        };
        let json = serde_json::to_string(&cd).unwrap();
        let round: ChunkDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(cd, round);
    }

    #[test]
    fn chunk_descriptor_omits_plaintext_hash_when_none() {
        // Public-file manifests must serialize byte-identically to the
        // pre-V2 format (no `plaintext_blake3_hash` field on the wire).
        let cd = ChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: CHUNK_SIZE,
            blake3_hash: [0xaa; 32],
            cid: "bafkr4itest".into(),
            plaintext_blake3_hash: None,
        };
        let json = serde_json::to_string(&cd).unwrap();
        assert!(
            !json.contains("plaintext_blake3_hash"),
            "expected field to be skipped, got: {json}"
        );
    }

    #[test]
    fn chunk_descriptor_with_plaintext_hash_round_trips() {
        let cd = ChunkDescriptor {
            chunk_index: 7,
            offset: 7 * CHUNK_SIZE,
            size: CHUNK_SIZE,
            blake3_hash: [0xaa; 32],
            cid: "bafkr4itest".into(),
            plaintext_blake3_hash: Some([0xdd; 32]),
        };
        let json = serde_json::to_string(&cd).unwrap();
        assert!(json.contains("plaintext_blake3_hash"));
        let round: ChunkDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(cd, round);
    }

    #[test]
    fn chunk_descriptor_accepts_legacy_manifests() {
        // Legacy (V1) manifests have no `plaintext_blake3_hash` field;
        // `serde(default)` must let them deserialize cleanly.
        let legacy = r#"{
            "chunk_index": 0,
            "offset": 0,
            "size": 1048576,
            "blake3_hash": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
                            0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            "cid": "bafkr4ilegacy"
        }"#;
        let cd: ChunkDescriptor = serde_json::from_str(legacy).unwrap();
        assert!(cd.plaintext_blake3_hash.is_none());
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
