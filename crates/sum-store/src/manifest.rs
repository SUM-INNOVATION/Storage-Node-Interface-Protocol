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

    // ── Phase 4a-3 wire-compat checks ───────────────────────────────────
    //
    // ChunkDescriptor gained `plaintext_blake3_hash: Option<[u8; 32]>` in
    // Phase 4a (Private files). The field uses
    // `#[serde(default, skip_serializing_if = "Option::is_none")]` so:
    //   - Public manifests (where the option is None) MUST serialize to
    //     bytes that contain no `plaintext_blake3_hash` key.
    //   - Pre-V2 manifests on disk (no field at all) MUST still
    //     deserialize cleanly.
    //
    // The JSON tests in `sum-types::storage` already prove this for
    // `serde_json`. This test pins the same contract for CBOR (the
    // production wire/disk format), because `skip_serializing_if` is a
    // serde-level attribute — but only by adding the test do we lock
    // any future codec swap from silently breaking it.

    use sum_types::storage::ChunkDescriptor;

    fn sample_chunk(plaintext_hash: Option<[u8; 32]>) -> ChunkDescriptor {
        ChunkDescriptor {
            chunk_index: 3,
            offset: 3 * sum_types::storage::CHUNK_SIZE,
            size: sum_types::storage::CHUNK_SIZE,
            blake3_hash: [0x77; 32],
            cid: "bafkr4iwirecompat".into(),
            plaintext_blake3_hash: plaintext_hash,
        }
    }

    /// CBOR-serializing a Public chunk (None plaintext hash) MUST omit
    /// the `plaintext_blake3_hash` map key entirely. We check by encoding
    /// twice — once as Public, once with `Some(_)` — and asserting the
    /// Public byte stream is strictly *shorter* (the encoded key + value
    /// take real bytes).
    #[test]
    fn cbor_omits_plaintext_blake3_hash_for_public_chunks() {
        let mut public_buf = Vec::new();
        ciborium::ser::into_writer(&sample_chunk(None), &mut public_buf).unwrap();

        let mut private_buf = Vec::new();
        ciborium::ser::into_writer(&sample_chunk(Some([0xcc; 32])), &mut private_buf).unwrap();

        assert!(
            public_buf.len() < private_buf.len(),
            "Public CBOR must be smaller than Private — `skip_serializing_if` not honored?\n\
             public={} bytes, private={} bytes",
            public_buf.len(),
            private_buf.len(),
        );

        // Bytes-level check: the literal field name must NOT appear in
        // the Public encoding. CBOR map keys are encoded as text strings
        // in serde's default mode.
        let needle = b"plaintext_blake3_hash";
        assert!(
            !public_buf.windows(needle.len()).any(|w| w == needle),
            "Public CBOR must not contain the field name",
        );
        assert!(
            private_buf.windows(needle.len()).any(|w| w == needle),
            "Private CBOR must contain the field name",
        );
    }

    /// CBOR round-trip with the new field set — proves Private chunks
    /// reach the wire and come back identical.
    #[test]
    fn cbor_round_trip_with_plaintext_blake3_hash() {
        let cd = sample_chunk(Some([0xde; 32]));
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&cd, &mut buf).unwrap();
        let recovered: ChunkDescriptor = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(cd, recovered);
        assert_eq!(recovered.plaintext_blake3_hash, Some([0xde; 32]));
    }

    /// A pre-Phase-4a manifest (chunks have no `plaintext_blake3_hash`
    /// field on the wire) MUST still parse — `serde(default)` fills the
    /// field with `None`. We construct such bytes by serializing a stub
    /// struct that lacks the field, then deserializing as the current
    /// `ChunkDescriptor`.
    #[test]
    fn cbor_legacy_chunk_without_field_deserializes() {
        // Mirror of pre-4a ChunkDescriptor: same field order, no
        // `plaintext_blake3_hash`. Bincode/CBOR-serializing this and
        // reading it as the current `ChunkDescriptor` must succeed
        // because `serde(default)` plugs in `None`.
        #[derive(serde::Serialize)]
        struct LegacyChunkDescriptor {
            chunk_index: u32,
            offset: u64,
            size: u64,
            blake3_hash: [u8; 32],
            cid: String,
        }
        let legacy = LegacyChunkDescriptor {
            chunk_index: 0,
            offset: 0,
            size: sum_types::storage::CHUNK_SIZE,
            blake3_hash: [0x11; 32],
            cid: "bafkr4ilegacy".into(),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut buf).unwrap();

        let cd: ChunkDescriptor = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(cd.chunk_index, 0);
        assert_eq!(cd.cid, "bafkr4ilegacy");
        assert!(
            cd.plaintext_blake3_hash.is_none(),
            "missing field must default to None",
        );
    }

    /// Whole-manifest CBOR round-trip with a mix of Public-style and
    /// Private-style chunks — verifies the per-chunk option doesn't
    /// confuse the manifest's array encoding.
    #[test]
    fn cbor_manifest_mixed_chunk_options_round_trips() {
        let chunks = vec![
            sample_chunk(None),
            sample_chunk(Some([0xab; 32])),
            sample_chunk(None),
        ];
        let manifest = DataManifest {
            file_name: "mixed.bin".into(),
            file_hash: [0x99; 32],
            total_size_bytes: 3 * sum_types::storage::CHUNK_SIZE,
            chunk_count: 3,
            merkle_root: [0x88; 32],
            chunks,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&manifest, &mut buf).unwrap();
        let recovered: DataManifest = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(recovered.chunks.len(), 3);
        assert!(recovered.chunks[0].plaintext_blake3_hash.is_none());
        assert_eq!(recovered.chunks[1].plaintext_blake3_hash, Some([0xab; 32]));
        assert!(recovered.chunks[2].plaintext_blake3_hash.is_none());
    }
}
