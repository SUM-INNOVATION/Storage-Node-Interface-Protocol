//! The network → manifest-index ingress, as two separable phases.
//!
//! This module exists because the ingress has two properties that are easy to
//! lose and hard to notice losing:
//!
//! 1. **The root is decided by the request, never the response.** A peer that
//!    answers a manifest pull supplies both the manifest bytes and, in the V1
//!    shape, the CID they arrive under. Validating one against the other proves
//!    only that the peer was internally consistent. [`plan_manifest_ingress`]
//!    reads the root out of [`OutboundOrigin`] — the identity `sum-net`
//!    retained when it sent the request — and nothing else.
//!
//! 2. **Validation does not run under the store's write lock.** CBOR decoding,
//!    Merkle reconstruction over an attacker-chosen chunk list and per-chunk CID
//!    derivation are all work whose cost the peer influences. Phase one takes no
//!    lock and needs none; the caller acquires the write guard only for phase
//!    two, which is map and file writes.
//!
//! The split is enforced by types rather than by convention:
//! [`ManifestIngress::Commit`] carries a [`ValidatedManifest`], and
//! `ManifestIndex::commit_validated` accepts nothing else. There is no path
//! from *response* bytes to the index that does not pass through
//! [`plan_manifest_ingress`].
//!
//! # What this module does not secure
//!
//! This is the ingress for manifests arriving as answers to pulls this node
//! issued. It says nothing about **inbound pushes**, where a peer arrives
//! unbidden with a manifest and names the root itself
//! (`sum_store::serve::handle_manifest_push` for V1,
//! `inbound_v2::handle_manifest_push` for V2). That path is not authenticated
//! and the pusher is not authorized for the root it names; validation there
//! proves the chunk list produces the named root, which a self-created
//! manifest satisfies trivially. Retiring the unauthenticated V1 push is
//! WP-B's work. Nothing here makes it safe, and nothing here should be read
//! as claiming to.

use sum_net::{OutboundOrigin, ShardResponse};
use sum_store::ManifestIndex;
use sum_store::manifest_index::{AcceptOutcome, ValidatedManifest, validate_network_manifest};

/// What phase one decided. Returned by value, holding no lock and no borrow of
/// the store, so the caller is free to acquire the write guard afterwards.
#[derive(Debug)]
pub enum ManifestIngress {
    /// Validation failed. Nothing has been read from or written to the store.
    Rejected { root_hex: String, error: String },
    /// Validation passed against the requested root. Ready for phase two.
    Commit {
        root_hex: String,
        validated: ValidatedManifest,
    },
}

/// Phase one — decide and validate. No store, no lock, no I/O.
///
/// Returns `None` when this response does not answer a manifest pull, in which
/// case the caller should treat it as chunk data.
///
/// `response.cid` is deliberately unused. `sum-net` has already refused any
/// response whose CID differs from the one this node sent, so re-deriving the
/// root from it would at best be redundant — and at worst, if that guarantee
/// ever weakened, would hand a peer control of which root its manifest is
/// indexed under.
pub fn plan_manifest_ingress(
    origin: &OutboundOrigin,
    response: &ShardResponse,
) -> Option<ManifestIngress> {
    let root_hex = origin.requested_manifest_root_hex()?;

    Some(match validate_network_manifest(root_hex, &response.data) {
        Ok(validated) => ManifestIngress::Commit {
            root_hex: root_hex.to_string(),
            validated,
        },
        Err(error) => ManifestIngress::Rejected {
            root_hex: root_hex.to_string(),
            error,
        },
    })
}

/// Phase two — commit, under the caller's write guard.
///
/// Takes the validated value, not bytes: this function is structurally
/// incapable of accepting an unvalidated network manifest.
pub fn commit_manifest_ingress(
    index: &mut ManifestIndex,
    validated: &ValidatedManifest,
) -> Result<AcceptOutcome, String> {
    index.commit_validated(validated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sum_net::{OutboundRequestKind, PeerId};
    use sum_types::storage::{ChunkDescriptor, DataManifest};

    /// A genuinely well-formed manifest: real chunk hashes, CIDs derived from
    /// them, root recomputed from the leaves. Passes `validate_manifest_push`
    /// when checked against its own root — which is exactly what makes it a
    /// useful forgery when checked against a different one.
    fn well_formed(bodies: &[&[u8]]) -> DataManifest {
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
                cid: sum_store::content_id::cid_from_blake3_hash(&hash),
                plaintext_blake3_hash: None,
            });
            offset += body.len() as u64;
        }
        DataManifest {
            file_name: "fixture.bin".into(),
            file_hash: [0xAB; 32],
            total_size_bytes: offset,
            chunk_count: chunks.len() as u32,
            merkle_root: *sum_store::merkle::MerkleTree::build(&leaves)
                .root()
                .as_bytes(),
            chunks,
        }
    }

    fn cbor(m: &DataManifest) -> Vec<u8> {
        let mut b = Vec::new();
        ciborium::ser::into_writer(m, &mut b).unwrap();
        b
    }

    fn manifest_pull_origin(root_hex: &str) -> OutboundOrigin {
        OutboundOrigin::new(
            PeerId::random(),
            OutboundRequestKind::V1Manifest {
                root_hex: root_hex.to_string(),
                cid: format!("{}{root_hex}", sum_net::MANIFEST_CID_PREFIX),
            },
        )
    }

    fn response(cid: &str, data: Vec<u8>) -> ShardResponse {
        ShardResponse {
            cid: cid.to_string(),
            offset: 0,
            total_bytes: data.len() as u64,
            data,
            error: None,
        }
    }

    /// Everything the index owns on disk — names and bytes — so "changed
    /// nothing" can be asserted rather than assumed.
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

    /// The attack, at the handler level.
    ///
    /// This node pulled root A. The peer answers with a manifest that is
    /// *internally perfect* — every CID matches its hash, the indices are
    /// ordered, the recomputed Merkle root equals the declared one — for root B,
    /// and labels the response `manifest:<B>` so that every field it controls
    /// agrees with every other field it controls.
    ///
    /// The only thing it cannot make agree is the root this node actually asked
    /// for. That is the value the handler validates against, and it is the
    /// reason this is rejected.
    #[test]
    fn a_self_consistent_manifest_for_a_different_root_is_rejected_before_any_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ManifestIndex::load(dir.path()).unwrap();

        let asked = well_formed(&[b"the file we asked for"]);
        let asked_hex = hex::encode(asked.merkle_root);

        let returned = well_formed(&[b"attacker chunk one", b"attacker chunk two"]);
        let returned_hex = hex::encode(returned.merkle_root);
        assert_ne!(asked_hex, returned_hex);

        // The returned manifest is valid *for its own root* — the forgery is
        // self-consistent, not malformed.
        assert!(
            validate_network_manifest(&returned_hex, &cbor(&returned)).is_ok(),
            "fixture must be a genuinely well-formed manifest for root B"
        );

        let before = snapshot(dir.path());

        let origin = manifest_pull_origin(&asked_hex);
        let resp = response(
            &format!("{}{returned_hex}", sum_net::MANIFEST_CID_PREFIX),
            cbor(&returned),
        );

        let plan = plan_manifest_ingress(&origin, &resp).expect("this answers a manifest pull");
        match plan {
            ManifestIngress::Rejected { root_hex, error } => {
                assert_eq!(root_hex, asked_hex, "must report the root we asked for");
                assert!(
                    error.contains("merkle_root mismatch"),
                    "unexpected rejection reason: {error}"
                );
            }
            ManifestIngress::Commit { .. } => panic!(
                "a manifest for root {returned_hex} was accepted for a pull of root {asked_hex}"
            ),
        }

        // Nothing was persisted and no map was mutated.
        assert_eq!(
            snapshot(dir.path()),
            before,
            "disk must be byte-for-byte unchanged"
        );
        assert!(index.get_by_merkle_root(&returned.merkle_root).is_none());
        assert!(index.get_by_merkle_root(&asked.merkle_root).is_none());
        for c in &returned.chunks {
            assert_eq!(
                index.merkle_root_for_cid(&c.cid),
                None,
                "no chunk CID from the forged manifest may be bound"
            );
        }
        assert_eq!(index.len(), 0);

        // And it does not become true on reload either.
        let reloaded = ManifestIndex::load(dir.path()).unwrap();
        assert_eq!(reloaded.len(), 0);
        let _ = &mut index;
    }

    #[test]
    fn the_manifest_we_asked_for_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ManifestIndex::load(dir.path()).unwrap();

        let m = well_formed(&[b"alpha", b"beta"]);
        let root_hex = hex::encode(m.merkle_root);
        let origin = manifest_pull_origin(&root_hex);
        let resp = response(
            &format!("{}{root_hex}", sum_net::MANIFEST_CID_PREFIX),
            cbor(&m),
        );

        let plan = plan_manifest_ingress(&origin, &resp).expect("answers a manifest pull");
        let ManifestIngress::Commit { validated, .. } = plan else {
            panic!("a manifest for the root we asked for must validate");
        };
        let outcome = commit_manifest_ingress(&mut index, &validated).unwrap();
        assert!(matches!(outcome, AcceptOutcome::Indexed(_)));
        assert!(index.get_by_merkle_root(&m.merkle_root).is_some());
        for c in &m.chunks {
            assert_eq!(index.merkle_root_for_cid(&c.cid), Some(&m.merkle_root));
        }
    }

    #[test]
    fn a_second_delivery_of_the_same_manifest_is_already_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ManifestIndex::load(dir.path()).unwrap();
        let m = well_formed(&[b"alpha"]);
        let root_hex = hex::encode(m.merkle_root);
        let origin = manifest_pull_origin(&root_hex);
        let resp = response(
            &format!("{}{root_hex}", sum_net::MANIFEST_CID_PREFIX),
            cbor(&m),
        );

        for expect_indexed in [true, false] {
            let ManifestIngress::Commit { validated, .. } =
                plan_manifest_ingress(&origin, &resp).unwrap()
            else {
                panic!("must validate");
            };
            let outcome = commit_manifest_ingress(&mut index, &validated).unwrap();
            assert_eq!(matches!(outcome, AcceptOutcome::Indexed(_)), expect_indexed);
        }
    }

    /// A response to a chunk pull is not a manifest, no matter what CID the
    /// peer writes on it. Without this the `manifest:` prefix would be a
    /// peer-selectable route into the index.
    #[test]
    fn a_chunk_pull_response_is_never_treated_as_a_manifest() {
        let m = well_formed(&[b"alpha"]);
        let root_hex = hex::encode(m.merkle_root);

        let origin = OutboundOrigin::new(
            PeerId::random(),
            OutboundRequestKind::V1Chunk {
                cid: "bafkr4iaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            },
        );
        // The peer labels its reply as a manifest and supplies a manifest that
        // would validate under its own root.
        let resp = response(
            &format!("{}{root_hex}", sum_net::MANIFEST_CID_PREFIX),
            cbor(&m),
        );

        assert!(
            plan_manifest_ingress(&origin, &resp).is_none(),
            "a chunk pull must never route into the manifest index"
        );
    }

    /// Malformed bytes are rejected without touching the store, and the
    /// rejection is reported against the root we asked for.
    #[test]
    fn malformed_bytes_are_rejected_against_the_requested_root() {
        let asked_hex = hex::encode([0x11u8; 32]);
        let origin = manifest_pull_origin(&asked_hex);
        let resp = response(
            &format!("{}{asked_hex}", sum_net::MANIFEST_CID_PREFIX),
            b"\xff\xff not cbor".to_vec(),
        );

        match plan_manifest_ingress(&origin, &resp).unwrap() {
            ManifestIngress::Rejected { root_hex, error } => {
                assert_eq!(root_hex, asked_hex);
                assert!(error.contains("deserialization failed"), "{error}");
            }
            ManifestIngress::Commit { .. } => panic!("garbage must not validate"),
        }
    }
}
