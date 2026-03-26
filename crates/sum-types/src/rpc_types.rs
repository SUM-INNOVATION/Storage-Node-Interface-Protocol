//! RPC response types matching the SUM Chain L1's JSON-RPC output.
//!
//! These structs deserialize directly from the JSON shapes produced by
//! `sum-chain/crates/rpc/src/server.rs` (lines 4912-5012).

use serde::{Deserialize, Serialize};

/// File metadata returned by `storage_getAccessList` and `storage_getFundedFiles`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFileInfo {
    /// Merkle root as 0x-prefixed hex string (64 hex chars + "0x").
    pub merkle_root: String,
    /// File owner's L1 address in base58 format.
    pub owner: String,
    /// Total file size in bytes.
    pub total_size_bytes: u64,
    /// List of L1 addresses (base58) allowed to retrieve this file.
    /// Empty means the file is public.
    pub access_list: Vec<String>,
    /// Remaining Koppa in the storage fee pool (base units).
    pub fee_pool: u64,
    /// Block height at which the file was registered.
    pub created_at: u64,
}

/// Active challenge returned by `storage_getActiveChallenges`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeInfo {
    /// Unique challenge identifier (0x-prefixed hex).
    pub challenge_id: String,
    /// Challenged file's merkle root (0x-prefixed hex).
    pub merkle_root: String,
    /// Zero-based chunk index within the file.
    pub chunk_index: u32,
    /// L1 address (base58) of the node being challenged.
    pub target_node: String,
    /// Block height at which the challenge was issued.
    pub created_at_height: u64,
    /// Block height by which the proof must be submitted (created + 50 blocks).
    pub expires_at_height: u64,
}

/// Node record returned by `storage_getNodeRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecordInfo {
    /// Node's L1 address in base58.
    pub address: String,
    /// Role: "ArchiveNode" or "Validator".
    pub role: String,
    /// Staked balance in Koppa base units.
    pub staked_balance: u64,
    /// Status: "Active" or "Slashed".
    pub status: String,
    /// Block height at which the node registered.
    pub registered_at: u64,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_file_info_deserialize() {
        let json = r#"{
            "merkle_root": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            "owner": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "total_size_bytes": 10485760,
            "access_list": ["DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q"],
            "fee_pool": 100000000000,
            "created_at": 12345
        }"#;
        let info: StorageFileInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.total_size_bytes, 10_485_760);
        assert_eq!(info.access_list.len(), 1);
        assert_eq!(info.fee_pool, 100_000_000_000);
    }

    #[test]
    fn challenge_info_deserialize() {
        let json = r#"{
            "challenge_id": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "merkle_root": "0x2222222222222222222222222222222222222222222222222222222222222222",
            "chunk_index": 5,
            "target_node": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "created_at_height": 1000,
            "expires_at_height": 1050
        }"#;
        let info: ChallengeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.chunk_index, 5);
        assert_eq!(info.expires_at_height, 1050);
    }

    #[test]
    fn node_record_info_deserialize() {
        let json = r#"{
            "address": "DzPJAYgL5J5RdXRKhcZX1QfD2V8uFhDv3Q",
            "role": "ArchiveNode",
            "staked_balance": 5000000000000,
            "status": "Active",
            "registered_at": 12300
        }"#;
        let info: NodeRecordInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.role, "ArchiveNode");
        assert_eq!(info.status, "Active");
    }

    #[test]
    fn storage_file_info_empty_access_list() {
        let json = r#"{
            "merkle_root": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "owner": "test",
            "total_size_bytes": 1048576,
            "access_list": [],
            "fee_pool": 0,
            "created_at": 1
        }"#;
        let info: StorageFileInfo = serde_json::from_str(json).unwrap();
        assert!(info.access_list.is_empty());
    }
}
