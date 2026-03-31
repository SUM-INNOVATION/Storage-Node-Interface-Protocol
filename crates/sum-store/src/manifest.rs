//! Data manifest CBOR serialization and JSON export.
//!
//! A [`DataManifest`] is the source of truth for a chunked file: it lists
//! every chunk's CID, offset, size, BLAKE3 hash, and the Merkle root.
//! It is written to disk as CBOR alongside the chunk files.

use std::path::Path;

use sum_types::storage::DataManifest;

use crate::error::{Result, StoreError};

// ── Serialization ─────────────────────────────────────────────────────────────

/// Serialize a manifest to CBOR and write it to `path`.
pub fn write_manifest(manifest: &DataManifest, path: &Path) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    ciborium::ser::into_writer(manifest, &mut buf)
        .map_err(|e| StoreError::Other(format!("CBOR serialization: {e}")))?;
    std::fs::write(path, &buf)?;
    Ok(())
}

/// Read a manifest from a CBOR file.
pub fn read_manifest(path: &Path) -> Result<DataManifest> {
    let data = std::fs::read(path)?;
    deserialize_manifest_cbor(&data)
}

/// Deserialize a manifest from CBOR bytes.
pub fn deserialize_manifest_cbor(data: &[u8]) -> Result<DataManifest> {
    ciborium::de::from_reader(data)
        .map_err(|e| StoreError::Other(format!("CBOR deserialization: {e}")))
}

/// Pretty-print a manifest as JSON (useful for debugging / inspection).
pub fn manifest_to_json(manifest: &DataManifest) -> Result<String> {
    serde_json::to_string_pretty(manifest)
        .map_err(|e| StoreError::Other(format!("JSON serialization: {e}")))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> DataManifest {
        DataManifest {
            file_name: "test-file.bin".into(),
            file_hash: [0xaa; 32],
            total_size_bytes: 4_000_000,
            chunk_count: 4,
            merkle_root: [0xbb; 32],
            chunks: vec![],
        }
    }

    #[test]
    fn cbor_round_trip() {
        let manifest = sample_manifest();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.cbor");

        write_manifest(&manifest, &path).unwrap();
        let loaded = read_manifest(&path).unwrap();

        assert_eq!(loaded.file_name, "test-file.bin");
        assert_eq!(loaded.chunk_count, 4);
        assert_eq!(loaded.merkle_root, [0xbb; 32]);
    }

    #[test]
    fn json_export() {
        let manifest = sample_manifest();
        let json = manifest_to_json(&manifest).unwrap();
        assert!(json.contains("\"file_name\": \"test-file.bin\""));
        assert!(json.contains("\"chunk_count\": 4"));
    }
}
