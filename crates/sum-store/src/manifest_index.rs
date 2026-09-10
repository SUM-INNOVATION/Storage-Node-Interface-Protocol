//! Persistent manifest index for fast lookup by merkle root or chunk CID.
//!
//! Two on-disk shapes coexist under `<store_dir>/manifests/`:
//!
//!   * `<hex_root>.cbor` — Public V2 / V1 files. Plaintext CBOR
//!     `DataManifest`, validated on insert (CID + Merkle root rebuild
//!     by `validate_manifest_push`), indexed by both merkle root and
//!     chunk CID for fast reverse lookup. Used by PoR, market sync,
//!     and the existing Public download path.
//!
//!   * `<hex_root>.opaque` — Private V2 files (Phase 4b). The chain
//!     attests to the file's ciphertext-Merkle root, so an archive
//!     node can persist the encrypted manifest blob without ever
//!     decoding it. Indexed only by merkle root — the chunk-CID
//!     reverse map is empty for Private files because the manifest
//!     is encrypted and the archive can't read the chunk list.
//!     Served back verbatim on ACL-gated `ManifestPull`; downstream
//!     readers (recipients with `K_file`) decrypt and decode locally.
//!
//! On startup both shapes are scanned and loaded into memory.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use sum_types::storage::DataManifest;

use crate::error::Result;
use crate::manifest;

/// In-memory index of all tracked file manifests.
pub struct ManifestIndex {
    manifests_dir: PathBuf,
    /// Primary index for Public files: merkle_root -> decoded CBOR manifest.
    by_root: HashMap<[u8; 32], DataManifest>,
    /// Reverse index: chunk CID -> merkle_root. Public files only —
    /// Private chunk CIDs aren't recoverable from the encrypted manifest
    /// without `K_file`.
    cid_to_root: HashMap<String, [u8; 32]>,
    /// Private (Phase 4b): opaque encrypted manifest bytes keyed by
    /// merkle root. Stored separately from `by_root` so a Private
    /// merkle root cannot be confused with a Public CBOR-validated
    /// manifest, and so PoR / market-sync paths that consult
    /// `get_by_merkle_root()` correctly skip Private files (they
    /// don't have a decoded `DataManifest` shape to operate on).
    private_bytes: HashMap<[u8; 32], Vec<u8>>,
    /// Private (Phase 4b): chunk-CID → merkle-root reverse index,
    /// populated on each accepted V2 Push for a Private file. Solves
    /// a chain-of-failure that otherwise breaks Private downloads:
    /// Private manifests are encrypted, so `cid_to_root` (which is
    /// derived from the decoded `DataManifest`) stays empty for
    /// ciphertext chunk CIDs. Without this Private-side mapping, the
    /// ACL gate on chunk pulls (`merkle_root_for_cid`) fails to
    /// resolve a CID to its file root and denies authorized
    /// recipients.
    ///
    /// Persisted as one CID per line in `<hex_root>.private_chunks`
    /// so Private downloads survive node restarts.
    private_cid_to_root: HashMap<String, [u8; 32]>,
}

/// Result of accepting a validated network manifest.
#[derive(Debug)]
pub enum AcceptOutcome {
    /// Newly written to disk and indexed.
    Indexed(DataManifest),
    /// Already held under this root; nothing was written.
    AlreadyPresent(DataManifest),
}

impl AcceptOutcome {
    pub fn manifest(&self) -> &DataManifest {
        match self {
            AcceptOutcome::Indexed(m) | AcceptOutcome::AlreadyPresent(m) => m,
        }
    }
}

/// A manifest that has been checked against a Merkle root this node asked for.
///
/// The field is private and [`validate_network_manifest`] is the only
/// constructor, so a value of this type cannot exist unless
/// [`crate::serve::validate_manifest_push`] returned `Ok` for it: CBOR decoded,
/// `chunk_count` agreeing with the vector, indices ordered `0..n`, every
/// chunk's CID bound to its own blake3 hash, and the Merkle root recomputed
/// from the leaves equal to the root that was asked for.
///
/// It is the argument type of [`ManifestIndex::commit_validated`], which is the
/// only way network bytes reach the index. `commit_validated` cannot decode,
/// hash, or check anything — it has no bytes to work from. So "commit an
/// unvalidated network manifest" is not a mistake that can be made by writing
/// the wrong code; it requires constructing a `ValidatedManifest`, and the only
/// thing that constructs one is the validator.
///
/// Splitting the work this way is also what keeps CBOR parsing, Merkle
/// construction and CID derivation off the store's write lock: validation takes
/// no lock and needs none, the caller acquires the lock afterwards, and the
/// commit under the lock is map and file writes only.
#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    manifest: DataManifest,
}

impl ValidatedManifest {
    pub fn manifest(&self) -> &DataManifest {
        &self.manifest
    }

    /// The root this manifest was validated against — equal to the root the
    /// local node requested, and equal to the root recomputed from the chunk
    /// leaves. Those two being the same value is the whole point.
    pub fn merkle_root(&self) -> [u8; 32] {
        self.manifest.merkle_root
    }

    pub fn into_manifest(self) -> DataManifest {
        self.manifest
    }
}

/// Validate network-supplied manifest bytes against the root **this node
/// asked for**. Step one of two; takes no lock and touches no store state.
///
/// `expected_root_hex` must come from the local record of the outbound request
/// (`sum_net::OutboundOrigin::requested_manifest_root_hex`). It must never be
/// derived from the response: a peer that supplies both the root and the
/// manifest can trivially make them agree, and matching them proves only that
/// the peer was self-consistent.
///
/// Note what this does and does not establish. Against a root *this node
/// requested*, it establishes that the manifest is the one that root names.
/// Against a root *a peer supplied* — an inbound push — it establishes only
/// internal consistency, which a self-created manifest has for free. The
/// strength of the guarantee comes from where the root came from, not from
/// this function.
///
/// The result carries no lock and no borrow, so the caller is free to acquire
/// the store write guard afterwards and pass it to
/// [`ManifestIndex::commit_validated`].
pub fn validate_network_manifest(
    expected_root_hex: &str,
    data: &[u8],
) -> std::result::Result<ValidatedManifest, String> {
    let manifest = crate::serve::validate_manifest_push(expected_root_hex, data)?;
    Ok(ValidatedManifest { manifest })
}

impl ManifestIndex {
    /// Load all manifests from `<store_dir>/manifests/*.cbor` into memory.
    ///
    /// If `manifests/` is empty but a legacy `manifest.cbor` exists in
    /// `store_dir`, it will be migrated into the new directory.
    pub fn load(store_dir: &Path) -> Result<Self> {
        let manifests_dir = store_dir.join("manifests");
        fs::create_dir_all(&manifests_dir)?;

        let mut index = Self {
            manifests_dir,
            by_root: HashMap::new(),
            cid_to_root: HashMap::new(),
            private_bytes: HashMap::new(),
            private_cid_to_root: HashMap::new(),
        };

        // Load all existing manifests. `.cbor` → Public manifest;
        // `.opaque` → Private encrypted manifest blob; `.private_chunks`
        // → newline-separated list of ciphertext CIDs for a Private
        // file (rebuilds the cid → root reverse index so ACL works on
        // a fresh boot). Anything else is ignored.
        for entry in fs::read_dir(&index.manifests_dir)? {
            let entry = entry?;
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("cbor") => match manifest::read_manifest(&path) {
                    Ok(m) => index.add_to_maps(m),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), %e, "skipping corrupt Public manifest");
                    }
                },
                Some("opaque") => {
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    match decode_hex_root(stem) {
                        Some(root) => match fs::read(&path) {
                            Ok(bytes) => {
                                index.private_bytes.insert(root, bytes);
                            }
                            Err(e) => {
                                tracing::warn!(path = %path.display(), %e, "skipping unreadable Private manifest");
                            }
                        },
                        None => {
                            tracing::warn!(path = %path.display(), "skipping Private manifest with non-hex stem");
                        }
                    }
                }
                Some("private_chunks") => {
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    let Some(root) = decode_hex_root(stem) else {
                        tracing::warn!(path = %path.display(), "skipping private-chunks sidecar with non-hex stem");
                        continue;
                    };
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            for line in content.lines() {
                                let cid = line.trim();
                                if !cid.is_empty() {
                                    index.private_cid_to_root.insert(cid.to_string(), root);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(path = %path.display(), %e, "skipping unreadable private-chunks sidecar");
                        }
                    }
                }
                _ => {}
            }
        }

        // Migrate legacy manifest.cbor if the index is empty.
        let legacy_path = store_dir.join("manifest.cbor");
        if index.by_root.is_empty() && index.private_bytes.is_empty() && legacy_path.exists() {
            if let Ok(m) = manifest::read_manifest(&legacy_path) {
                tracing::info!("migrating legacy manifest.cbor into manifests/");
                // Write to new location (ignore write errors on migration).
                let _ = index.write_manifest(&m);
                index.add_to_maps(m);
            }
        }

        tracing::info!(
            public_manifests = index.by_root.len(),
            private_manifests = index.private_bytes.len(),
            public_indexed_chunks = index.cid_to_root.len(),
            private_indexed_chunks = index.private_cid_to_root.len(),
            "manifest index loaded"
        );

        Ok(index)
    }

    /// Commit an already-validated manifest. Step two of two; the caller holds
    /// the store write lock across this call and nothing else.
    ///
    /// There is no `data: &[u8]` parameter and no validation here, by design:
    /// this function is incapable of accepting unchecked network bytes. The
    /// proof of validation is the argument type — see [`ValidatedManifest`].
    ///
    /// Idempotent: a manifest already held under this root is reported as
    /// [`AcceptOutcome::AlreadyPresent`] with nothing written.
    pub fn commit_validated(
        &mut self,
        validated: &ValidatedManifest,
    ) -> std::result::Result<AcceptOutcome, String> {
        let manifest = validated.manifest();
        if self.get_by_merkle_root(&validated.merkle_root()).is_some() {
            return Ok(AcceptOutcome::AlreadyPresent(manifest.clone()));
        }
        self.insert(manifest)
            .map_err(|e| format!("manifest index insert failed: {e}"))?;
        Ok(AcceptOutcome::Indexed(manifest.clone()))
    }

    /// Insert a Public manifest: write to disk as CBOR and update
    /// in-memory indexes (both root→manifest and chunk-CID→root).
    ///
    /// Trusts what it is handed. Callers holding a manifest that came back
    /// from a **pull this node issued** must go through
    /// [`validate_network_manifest`] and [`ManifestIndex::commit_validated`],
    /// which bind it to the root that was requested.
    ///
    /// The remaining callers are not all equally safe, and it is worth being
    /// exact about which is which:
    ///
    /// * `SumStore::ingest_file` — local, trusted; the manifest was built here.
    /// * `download.rs` (V1 and V2) — validated against a root this node chose:
    ///   the one it is downloading, or the one the chain states.
    /// * `serve::handle_manifest_push` — an **inbound V1 push**. The root comes
    ///   from the pushing peer's own request CID, and the peer is neither
    ///   authenticated nor authorized for it. Validation proves the chunk list
    ///   produces the root the pusher named; it proves nothing about whether
    ///   that peer was entitled to name it. A self-created manifest is
    ///   internally consistent by construction.
    /// * `inbound_v2::handle_manifest_push` — an inbound V2 push, same shape
    ///   plus a chain probe that the root is registered. A cost, not a barrier.
    ///
    /// Closing the V1 push path is WP-B's job, not this module's.
    pub fn insert(&mut self, manifest: &DataManifest) -> Result<()> {
        self.write_manifest(manifest)?;
        self.add_to_maps(manifest.clone());
        Ok(())
    }

    /// Insert a Private (encrypted) manifest blob: persist verbatim to
    /// `<root>.opaque` and index by merkle root only. The bytes are
    /// stored AS-IS — the archive node MUST NOT decode or reinterpret
    /// them. Authorized recipients pull them on demand and decrypt
    /// locally with `K_file`.
    ///
    /// Idempotent: a second insert with the same root overwrites the
    /// in-memory entry and re-writes the disk file. Callers
    /// (`inbound_v2::handle_manifest_push`) typically check `get_private_bytes`
    /// first and skip on a hit.
    pub fn insert_private(&mut self, root: [u8; 32], bytes: Vec<u8>) -> Result<()> {
        self.write_private_bytes(&root, &bytes)?;
        self.private_bytes.insert(root, bytes);
        Ok(())
    }

    /// Look up a Public manifest by its merkle root. Returns `None`
    /// for Private files (callers needing Private should use
    /// `get_private_bytes`) and for unknown roots.
    pub fn get_by_merkle_root(&self, root: &[u8; 32]) -> Option<&DataManifest> {
        self.by_root.get(root)
    }

    /// Look up a Private (opaque, encrypted) manifest blob by its
    /// merkle root. Returns `None` for Public files (use
    /// `get_by_merkle_root` for those) and for unknown roots.
    pub fn get_private_bytes(&self, root: &[u8; 32]) -> Option<&[u8]> {
        self.private_bytes.get(root).map(|v| v.as_slice())
    }

    /// Record a `(cid, merkle_root)` mapping for a Private (encrypted)
    /// chunk that this archive has just accepted via V2 Push. Both
    /// the in-memory map AND the on-disk sidecar are updated so
    /// recovery from a node restart still resolves these CIDs.
    ///
    /// Idempotent for `(cid, root)` pairs already mapped to the same
    /// root; an attempt to remap a CID to a *different* root returns
    /// an error (defensive: no legitimate flow ever does this, and a
    /// silent rebind would let a malicious peer force an existing
    /// chunk into a different file's namespace).
    pub fn record_private_chunk_cid(&mut self, merkle_root: [u8; 32], cid: &str) -> Result<()> {
        if let Some(existing) = self.private_cid_to_root.get(cid) {
            if existing == &merkle_root {
                return Ok(()); // idempotent
            }
            return Err(crate::error::StoreError::Other(format!(
                "cid {cid} already mapped to root 0x{} — refusing rebind to 0x{}",
                hex::encode(existing),
                hex::encode(merkle_root)
            )));
        }
        self.append_private_chunk_cid_to_disk(&merkle_root, cid)?;
        self.private_cid_to_root
            .insert(cid.to_string(), merkle_root);
        Ok(())
    }

    /// Get the CID for a specific chunk within a file.
    pub fn chunk_cid(&self, root: &[u8; 32], chunk_index: u32) -> Option<&str> {
        self.by_root
            .get(root)
            .and_then(|m| m.chunks.get(chunk_index as usize))
            .map(|c| c.cid.as_str())
    }

    /// Reverse lookup: find which file (merkle_root) a chunk CID
    /// belongs to. Consults both Public (`cid_to_root`, populated
    /// from decoded `DataManifest`) and Private (`private_cid_to_root`,
    /// populated by `record_private_chunk_cid` on accepted V2 Push)
    /// indexes. Returns `None` only when neither knows the CID.
    ///
    /// The serving-side ACL gate (`acl::resolve_root_for_cid`) calls
    /// this to map a chunk-pull request to its file root; if neither
    /// index has the CID, ACL denies the pull. For Private files
    /// this would manifest as authorized recipients getting denied
    /// on ciphertext fetches, so the Private branch must be wired
    /// here for Phase 4b downloads to work.
    pub fn merkle_root_for_cid(&self, cid: &str) -> Option<&[u8; 32]> {
        self.cid_to_root
            .get(cid)
            .or_else(|| self.private_cid_to_root.get(cid))
    }

    /// All tracked merkle roots (Public + Private). Useful for the
    /// market-sync sweep, which gates further behavior on whether the
    /// archive holds *any* shape of manifest for a given file.
    pub fn all_merkle_roots(&self) -> Vec<[u8; 32]> {
        let mut out: Vec<[u8; 32]> = self.by_root.keys().copied().collect();
        out.extend(self.private_bytes.keys().copied());
        out
    }

    /// Number of tracked manifests across both Public and Private.
    pub fn len(&self) -> usize {
        self.by_root.len() + self.private_bytes.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty() && self.private_bytes.is_empty()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn add_to_maps(&mut self, manifest: DataManifest) {
        let root = manifest.merkle_root;
        for chunk in &manifest.chunks {
            self.cid_to_root.insert(chunk.cid.clone(), root);
        }
        self.by_root.insert(root, manifest);
    }

    fn write_manifest(&self, manifest: &DataManifest) -> Result<()> {
        let hex_root: String = manifest
            .merkle_root
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = self.manifests_dir.join(format!("{hex_root}.cbor"));
        manifest::write_manifest(manifest, &path)
    }

    fn write_private_bytes(&self, root: &[u8; 32], bytes: &[u8]) -> Result<()> {
        let hex_root: String = root.iter().map(|b| format!("{b:02x}")).collect();
        let path = self.manifests_dir.join(format!("{hex_root}.opaque"));
        std::fs::write(&path, bytes)?;
        Ok(())
    }

    /// Append one CID line to `<hex_root>.private_chunks`. Each line
    /// is independently complete (`<cid>\n`), so a crash mid-write
    /// loses at most the in-flight line — the rest reloads cleanly.
    fn append_private_chunk_cid_to_disk(&self, root: &[u8; 32], cid: &str) -> Result<()> {
        use std::io::Write;
        let hex_root: String = root.iter().map(|b| format!("{b:02x}")).collect();
        let path = self
            .manifests_dir
            .join(format!("{hex_root}.private_chunks"));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(cid.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_data()?; // durability — Private downloads depend on this surviving restart.
        Ok(())
    }
}

/// Decode a 64-character lowercase hex string to `[u8; 32]`.
/// Returns `None` if the string is the wrong length or contains
/// non-hex characters. Used by `ManifestIndex::load` to resolve
/// `<root>.opaque` filenames back to merkle roots.
fn decode_hex_root(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    fn sample_manifest(root_byte: u8) -> DataManifest {
        DataManifest {
            file_name: format!("test_{root_byte}.bin"),
            file_hash: [root_byte; 32],
            total_size_bytes: 2_097_152,
            chunk_count: 2,
            merkle_root: [root_byte; 32],
            chunks: vec![
                ChunkDescriptor {
                    chunk_index: 0,
                    offset: 0,
                    size: 1_048_576,
                    blake3_hash: [root_byte + 1; 32],
                    cid: format!("bafk_chunk0_{root_byte}"),
                    plaintext_blake3_hash: None,
                },
                ChunkDescriptor {
                    chunk_index: 1,
                    offset: 1_048_576,
                    size: 1_048_576,
                    blake3_hash: [root_byte + 2; 32],
                    cid: format!("bafk_chunk1_{root_byte}"),
                    plaintext_blake3_hash: None,
                },
            ],
        }
    }

    #[test]
    fn insert_and_lookup_by_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();

        let m = sample_manifest(0xAA);
        idx.insert(&m).unwrap();

        let found = idx.get_by_merkle_root(&[0xAA; 32]).unwrap();
        assert_eq!(found.file_name, "test_170.bin");
        assert_eq!(found.chunk_count, 2);
    }

    #[test]
    fn lookup_by_cid() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();

        let m = sample_manifest(0xBB);
        idx.insert(&m).unwrap();

        let root = idx.merkle_root_for_cid("bafk_chunk0_187").unwrap();
        assert_eq!(*root, [0xBB; 32]);

        let root1 = idx.merkle_root_for_cid("bafk_chunk1_187").unwrap();
        assert_eq!(*root1, [0xBB; 32]);

        assert!(idx.merkle_root_for_cid("nonexistent").is_none());
    }

    #[test]
    fn chunk_cid_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();

        let m = sample_manifest(0xCC);
        idx.insert(&m).unwrap();

        assert_eq!(idx.chunk_cid(&[0xCC; 32], 0), Some("bafk_chunk0_204"));
        assert_eq!(idx.chunk_cid(&[0xCC; 32], 1), Some("bafk_chunk1_204"));
        assert_eq!(idx.chunk_cid(&[0xCC; 32], 2), None);
    }

    #[test]
    fn persistence_across_reload() {
        let dir = tempfile::tempdir().unwrap();

        // Insert two manifests.
        {
            let mut idx = ManifestIndex::load(dir.path()).unwrap();
            idx.insert(&sample_manifest(0x11)).unwrap();
            idx.insert(&sample_manifest(0x22)).unwrap();
            assert_eq!(idx.len(), 2);
        }

        // Reload from disk.
        {
            let idx = ManifestIndex::load(dir.path()).unwrap();
            assert_eq!(idx.len(), 2);
            assert!(idx.get_by_merkle_root(&[0x11; 32]).is_some());
            assert!(idx.get_by_merkle_root(&[0x22; 32]).is_some());
            assert!(idx.merkle_root_for_cid("bafk_chunk0_17").is_some());
        }
    }

    #[test]
    fn legacy_manifest_migration() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample_manifest(0xDD);

        // Write a legacy manifest.cbor in the store root.
        manifest::write_manifest(&m, &dir.path().join("manifest.cbor")).unwrap();

        // Load should migrate it.
        let idx = ManifestIndex::load(dir.path()).unwrap();
        assert_eq!(idx.len(), 1);
        assert!(idx.get_by_merkle_root(&[0xDD; 32]).is_some());
    }

    #[test]
    fn empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let idx = ManifestIndex::load(dir.path()).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    // ── Phase 4b: Private manifest storage ─────────────────────────────

    /// Round-trip a Private manifest blob: insert → in-memory get → reload from disk → get.
    /// The bytes must come back byte-identical (chain commits to a hash
    /// of these bytes via the per-chunk push hashes; the archive must
    /// not reinterpret them in any way).
    #[test]
    fn private_manifest_round_trip_in_memory_and_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = [0x77u8; 32];
        let opaque: Vec<u8> = (0..200u8).collect();

        {
            let mut idx = ManifestIndex::load(dir.path()).unwrap();
            idx.insert_private(root, opaque.clone()).unwrap();
            assert_eq!(idx.get_private_bytes(&root), Some(opaque.as_slice()));
            assert!(
                idx.get_by_merkle_root(&root).is_none(),
                "Private root must NOT appear in the Public-only index"
            );
            assert_eq!(idx.len(), 1);
            assert!(!idx.is_empty());
        }

        // Reload from disk: the .opaque file must be re-read.
        {
            let idx = ManifestIndex::load(dir.path()).unwrap();
            assert_eq!(
                idx.get_private_bytes(&root),
                Some(opaque.as_slice()),
                "Private bytes must survive a reload byte-identical"
            );
            assert!(idx.get_by_merkle_root(&root).is_none());
        }
    }

    /// Mixed index: a Public file and a Private file with overlapping
    /// hex prefixes. Both must coexist; lookups must not cross.
    #[test]
    fn public_and_private_coexist_without_crosstalk() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();

        // Public: sample_manifest(0xAA) has merkle_root = [0xAA; 32].
        idx.insert(&sample_manifest(0xAA)).unwrap();
        // Private: distinct root.
        let private_root = [0xBBu8; 32];
        let private_bytes = b"opaque encrypted manifest blob".to_vec();
        idx.insert_private(private_root, private_bytes.clone())
            .unwrap();

        // Public-side lookups only see Public.
        assert!(idx.get_by_merkle_root(&[0xAA; 32]).is_some());
        assert!(idx.get_by_merkle_root(&private_root).is_none());

        // Private-side lookups only see Private.
        assert!(idx.get_private_bytes(&[0xAA; 32]).is_none());
        assert_eq!(
            idx.get_private_bytes(&private_root),
            Some(private_bytes.as_slice())
        );

        // Aggregate counters cover both.
        assert_eq!(idx.len(), 2);
        let mut roots = idx.all_merkle_roots();
        roots.sort();
        assert_eq!(roots, vec![[0xAA; 32], [0xBB; 32]]);
    }

    /// Phase 4b chain-of-failure fix: `record_private_chunk_cid`
    /// populates `merkle_root_for_cid` so the ACL gate can resolve a
    /// Private ciphertext chunk pull back to its file root. Without
    /// this, authorized recipients fail at the ACL check.
    #[test]
    fn merkle_root_for_cid_resolves_private_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();
        let root = [0xC0u8; 32];
        let cid = "bafk_private_chunk_xyz";
        idx.record_private_chunk_cid(root, cid).unwrap();
        assert_eq!(idx.merkle_root_for_cid(cid), Some(&root));
    }

    /// Public + Private CID lookups don't cross-contaminate, but a
    /// CID that exists in only one of the two indexes still resolves.
    #[test]
    fn merkle_root_for_cid_consults_both_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();

        // Public file with two chunks.
        let public = sample_manifest(0xAA);
        idx.insert(&public).unwrap();
        let public_cid_0 = public.chunks[0].cid.clone();

        // Private file with one chunk.
        let private_root = [0xBBu8; 32];
        let private_cid = "bafk_private_only";
        idx.record_private_chunk_cid(private_root, private_cid)
            .unwrap();

        // Each CID resolves to its own root, neither leaks into the other.
        assert_eq!(idx.merkle_root_for_cid(&public_cid_0), Some(&[0xAA; 32]));
        assert_eq!(idx.merkle_root_for_cid(private_cid), Some(&private_root));
        assert!(idx.merkle_root_for_cid("unknown").is_none());
    }

    /// Survives a node restart: reload reads the
    /// `<root>.private_chunks` sidecar and re-populates
    /// `private_cid_to_root`. Without this, ACL would deny Private
    /// chunk pulls forever after the first restart.
    #[test]
    fn private_chunk_cids_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = [0xC1u8; 32];
        let cids = ["bafk_p_0", "bafk_p_1", "bafk_p_2"];

        {
            let mut idx = ManifestIndex::load(dir.path()).unwrap();
            for cid in &cids {
                idx.record_private_chunk_cid(root, cid).unwrap();
            }
            for cid in &cids {
                assert_eq!(idx.merkle_root_for_cid(cid), Some(&root));
            }
        }

        // Reload: every CID must resolve back to the same root.
        {
            let idx = ManifestIndex::load(dir.path()).unwrap();
            for cid in &cids {
                assert_eq!(
                    idx.merkle_root_for_cid(cid),
                    Some(&root),
                    "Private CID {cid} must resolve after reload"
                );
            }
        }
    }

    /// Idempotent re-recording for the same `(cid, root)` pair: no
    /// error, no duplicate sidecar lines visible to the in-memory map.
    /// Callers (V2 Push handler) may re-record on retry without
    /// needing to track in-flight state.
    #[test]
    fn record_private_chunk_cid_is_idempotent_for_same_pair() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();
        let root = [0xC2u8; 32];
        let cid = "bafk_idem";
        idx.record_private_chunk_cid(root, cid).unwrap();
        idx.record_private_chunk_cid(root, cid).unwrap(); // no error
        assert_eq!(idx.merkle_root_for_cid(cid), Some(&root));
    }

    /// Defensive: refuse to rebind a CID from one Private root to
    /// another. Same CID with different root is either a chain
    /// glitch (a single ciphertext somehow colliding across two
    /// files) or an attempted attack (peer pushing the same chunk
    /// under a different root to hijack ACL routing). Either way,
    /// an explicit error is safer than a silent overwrite.
    #[test]
    fn record_private_chunk_cid_refuses_rebind_to_different_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();
        let cid = "bafk_clash";
        idx.record_private_chunk_cid([0xAAu8; 32], cid).unwrap();
        let err = idx
            .record_private_chunk_cid([0xBBu8; 32], cid)
            .expect_err("rebind must error");
        let msg = format!("{err}");
        assert!(msg.contains("already mapped"), "got: {msg}");
        // Original mapping still holds.
        assert_eq!(idx.merkle_root_for_cid(cid), Some(&[0xAAu8; 32]));
    }

    /// `insert_private` is idempotent in that a re-insert with the
    /// same root simply overwrites; callers (push handler) usually
    /// gate on `get_private_bytes` first to log idempotency, but the
    /// store doesn't reject re-inserts.
    #[test]
    fn private_manifest_insert_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ManifestIndex::load(dir.path()).unwrap();
        let root = [0xCCu8; 32];
        idx.insert_private(root, b"first".to_vec()).unwrap();
        idx.insert_private(root, b"second".to_vec()).unwrap();
        // Last write wins; no error.
        assert_eq!(idx.get_private_bytes(&root), Some(b"second".as_slice()));
        assert_eq!(idx.len(), 1);
    }
}

#[cfg(test)]
mod network_manifest_ingress_tests {
    use super::*;
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    /// A genuinely well-formed manifest: CIDs derived from real chunk hashes,
    /// root recomputed from the leaves.
    fn valid_manifest(bodies: &[&[u8]]) -> DataManifest {
        let mut chunks = Vec::new();
        let mut leaves = Vec::new();
        let mut offset = 0u64;
        for (i, body) in bodies.iter().enumerate() {
            let hash = blake3::hash(body);
            leaves.push(hash);
            chunks.push(ChunkDescriptor {
                chunk_index: i as u32,
                offset,
                size: body.len() as u64,
                blake3_hash: *hash.as_bytes(),
                cid: crate::content_id::cid_from_blake3_hash(&hash),
                plaintext_blake3_hash: None,
            });
            offset += body.len() as u64;
        }
        DataManifest {
            file_name: "fixture.bin".into(),
            file_hash: [0xAB; 32],
            total_size_bytes: offset,
            chunk_count: chunks.len() as u32,
            merkle_root: *crate::merkle::MerkleTree::build(&leaves).root().as_bytes(),
            chunks,
        }
    }

    fn cbor(m: &DataManifest) -> Vec<u8> {
        let mut b = Vec::new();
        ciborium::ser::into_writer(m, &mut b).unwrap();
        b
    }

    fn idx(dir: &std::path::Path) -> ManifestIndex {
        ManifestIndex::load(dir).unwrap()
    }

    /// The production sequence, exactly: validate with no store in hand, then
    /// commit what validation produced. Written as a helper so every test below
    /// exercises both steps and neither can be skipped by accident.
    fn accept(
        i: &mut ManifestIndex,
        root_hex: &str,
        data: &[u8],
    ) -> std::result::Result<AcceptOutcome, String> {
        let validated = validate_network_manifest(root_hex, data)?;
        i.commit_validated(&validated)
    }

    /// Everything the store owns, so a rejection can be proved to change none
    /// of it — names AND bytes, not just a count.
    fn snapshot(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir.join("manifests"))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().into_owned(),
                            std::fs::read(e.path()).unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn a_valid_manifest_is_indexed() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let m = valid_manifest(&[b"alpha", b"beta"]);
        let root_hex = hex::encode(m.merkle_root);

        let out = accept(&mut i, &root_hex, &cbor(&m)).unwrap();
        assert!(matches!(out, AcceptOutcome::Indexed(_)));
        assert!(i.get_by_merkle_root(&m.merkle_root).is_some());
        for c in &m.chunks {
            assert_eq!(i.merkle_root_for_cid(&c.cid), Some(&m.merkle_root));
        }
    }

    #[test]
    fn a_repeat_of_the_same_manifest_is_already_present() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let m = valid_manifest(&[b"alpha"]);
        let root_hex = hex::encode(m.merkle_root);
        accept(&mut i, &root_hex, &cbor(&m)).unwrap();
        let out = accept(&mut i, &root_hex, &cbor(&m)).unwrap();
        assert!(matches!(out, AcceptOutcome::AlreadyPresent(_)));
    }

    /// The attack this exists to stop: the manifest declares the root we asked
    /// for, but its chunk list is the attacker's.
    #[test]
    fn a_forged_chunk_list_under_the_requested_root_is_rejected_and_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let asked = valid_manifest(&[b"the-real-chunk"]);
        let asked_hex = hex::encode(asked.merkle_root);

        let mut forged = valid_manifest(&[b"attacker-chosen", b"and-another"]);
        let victim_cids: Vec<String> = forged.chunks.iter().map(|c| c.cid.clone()).collect();
        forged.merkle_root = asked.merkle_root; // declare the root we asked for

        let before = snapshot(d.path());
        let err = accept(&mut i, &asked_hex, &cbor(&forged)).unwrap_err();
        assert!(err.contains("merkle root recomputation failed"), "{err}");

        assert_eq!(
            snapshot(d.path()),
            before,
            "disk must be byte-for-byte unchanged"
        );
        assert!(i.get_by_merkle_root(&asked.merkle_root).is_none());
        for cid in victim_cids {
            assert_eq!(i.merkle_root_for_cid(&cid), None, "no cid may be bound");
        }
    }

    #[test]
    fn a_manifest_for_a_different_root_is_rejected_and_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let other = valid_manifest(&[b"some-other-file"]);
        let asked_hex = hex::encode([0x11u8; 32]);

        let before = snapshot(d.path());
        let err = accept(&mut i, &asked_hex, &cbor(&other)).unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(snapshot(d.path()), before);
        assert!(i.get_by_merkle_root(&other.merkle_root).is_none());
    }

    #[test]
    fn malformed_cbor_is_rejected_and_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let before = snapshot(d.path());
        let err = accept(&mut i, &hex::encode([0x22u8; 32]), b"\xff\xff not cbor").unwrap_err();
        assert!(err.contains("deserialization failed"), "{err}");
        assert_eq!(snapshot(d.path()), before);
    }

    #[test]
    fn a_bad_cid_hash_binding_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let mut m = valid_manifest(&[b"alpha"]);
        m.chunks[0].cid = "bafkr4iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        let root_hex = hex::encode(m.merkle_root);
        let err = accept(&mut i, &root_hex, &cbor(&m)).unwrap_err();
        assert!(err.contains("does not match its blake3_hash"), "{err}");
    }

    #[test]
    fn out_of_order_chunks_are_rejected() {
        let d = tempfile::tempdir().unwrap();
        let mut i = idx(d.path());
        let mut m = valid_manifest(&[b"alpha", b"beta"]);
        m.chunks.swap(0, 1);
        let root_hex = hex::encode(m.merkle_root);
        let err = accept(&mut i, &root_hex, &cbor(&m)).unwrap_err();
        assert!(err.contains("out of order"), "{err}");
    }

    #[test]
    fn a_rejected_manifest_does_not_survive_a_reload() {
        let d = tempfile::tempdir().unwrap();
        let asked = valid_manifest(&[b"real"]);
        let asked_hex = hex::encode(asked.merkle_root);
        let mut forged = valid_manifest(&[b"forged"]);
        forged.merkle_root = asked.merkle_root;
        {
            let mut i = idx(d.path());
            assert!(accept(&mut i, &asked_hex, &cbor(&forged)).is_err());
        }
        let reloaded = idx(d.path());
        assert!(reloaded.get_by_merkle_root(&asked.merkle_root).is_none());
        assert_eq!(reloaded.len(), 0);
    }
}
