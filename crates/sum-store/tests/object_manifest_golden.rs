//! SNIP #40 Phase 1 cross-validation tests (pinned `sumchain-wire = "=0.2.1"`).
//!
//! These extend the `wire_equivalence.rs` cross-validation pattern to the `b0`
//! object/manifest commitment surface. They prove that SNIP's adopted shared
//! object/manifest types reproduce the frozen B0-PRE contract byte-for-byte,
//! and that SNIP's own producer surfaces (`sum_store::BinaryChunker::chunk_file`,
//! `sum_store::MerkleTree::build`, and the `sum_types` `DataManifest`/`ChunkDescriptor`
//! storage types) agree with the shared commitment's Merkle root.
//!
//! Authoritative vectors are vendored VERBATIM from the single cross-validated
//! upstream source `sum-chain docs/b0-pre/fixtures/encoding-golden/vectors.json`
//! into `tests/fixtures/b0-encoding-golden-vectors.json`, hash-locked below.
//!
//! Scope note (do NOT read this as #40 closure): this is the =0.2.1 Phase-1
//! track only. It covers the five frozen cases: empty object, single-chunk
//! object, two-slot output manifest, three-slot input manifest, and the
//! three-chunk (chunk-boundary) Merkle root. Explicitly DEFERRED to a future
//! `sumchain-wire 0.2.2` track (no authoritative bytes exist on =0.2.1 yet):
//!   * a frozen one-slot output/input manifest vector (bytes + commitment_identity),
//!   * a frozen full multi-chunk `ObjectCommitmentV1` vector (bytes + identity)
//!     over a >=2-chunk buffer — today only the bare `merkle_multichunk_root` is
//!     frozen, so the multi-chunk object case anchors ONLY root/byte_len/chunk_count.
//! The one-slot authoritative bytes, the full multi-chunk commitment vector, and
//! the scorer-duplicate deletion land after 0.2.2. #40 closure is NOT claimed here.
//!
//! Frozen type references use the crate-root re-exports
//! (`sumchain_wire::{ObjectCommitmentV1, OutputManifestV1, InputManifestV1}`) and
//! the `b0::` path for the rest — exactly as the crate's own golden test imports
//! them. The rejected stale names `StateObjectV1` / `UnitOutputManifestV1` are NOT
//! aliased anywhere.

use sum_store::BinaryChunker;
use sum_store::MerkleTree;

use sumchain_wire::b0::enums::{InputSlotKind, ObjectKind, SlotKind};
use sumchain_wire::b0::manifest::{InputSlotDescriptorV1, SlotDescriptorV1};
use sumchain_wire::b0::merkle::CHUNK;
use sumchain_wire::b0::tags::{INPUT_MANIFEST_TAG, OBJECT_TAG, OUTPUT_MANIFEST_TAG};
use sumchain_wire::{InputManifestV1, ObjectCommitmentV1, OutputManifestV1};

// ── Vendored authoritative fixture + hardcoded upstream digest ────────────────
//
// Vendored VERBATIM from sum-chain docs/b0-pre/fixtures/encoding-golden/vectors.json
// (cross-validated by two independent B0-PRE encoders). The mutable `.sha256`
// sidecar next to the vendored file is NOT the tripwire — this hardcoded const is.

const V: &str = include_str!("fixtures/b0-encoding-golden-vectors.json");

/// SHA-256 of the authoritative upstream fixture, hardcoded as a drift tripwire.
/// The test recomputes the digest of the vendored bytes at runtime and asserts
/// equality BEFORE consuming any vector, so any silent edit fails loudly.
const EXPECTED_SHA256: &str = "26a6338e3572384adfc4e0aa379f4501cb1c350a4195ce85f8056b2f378875c1";

// ── Minimal self-contained SHA-256 (FIPS 180-4) ───────────────────────────────
// Kept in-test so this closure adds ONLY the three intended files (test + vendored
// fixture + sidecar) and touches no production source / Cargo manifest. Used solely
// as an integrity tripwire over a known fixture; validated against the hardcoded
// digest (which itself was produced by `shasum -a 256`).

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            *word = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

// ── Fixture / hex helpers ─────────────────────────────────────────────────────

fn assert_digest_locked() {
    let got = sha256_hex(V.as_bytes());
    assert_eq!(
        got, EXPECTED_SHA256,
        "vendored b0 encoding-golden fixture digest drift: the vendored file no \
         longer matches the hardcoded upstream SHA-256"
    );
}

/// Assert the hardcoded digest lock, THEN parse. Every vector-consuming test
/// starts here so the tripwire runs before any vector is read.
fn locked_fixture() -> serde_json::Value {
    assert_digest_locked();
    serde_json::from_str(V).expect("parse vendored fixture json")
}

fn jstr(j: &serde_json::Value, key: &str, field: &str) -> String {
    j[key][field]
        .as_str()
        .unwrap_or_else(|| panic!("missing string {key}.{field}"))
        .to_string()
}

fn jbare(j: &serde_json::Value, key: &str) -> String {
    j[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing bare string {key}"))
        .to_string()
}

fn hx(b: &[u8]) -> String {
    hex::encode(b)
}

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).expect("valid hex")
}

// ── SNIP-native producer helpers (chunk_file + MerkleTree) ────────────────────

/// Run a buffer through SNIP's real producer surface: write it to a temp file,
/// mmap + chunk it into 1 MiB pieces via `BinaryChunker::chunk_file`, and return
/// the resulting `sum_types` `DataManifest`.
fn snip_manifest(buf: &[u8]) -> sum_types::storage::DataManifest {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("golden-object.bin");
    std::fs::write(&path, buf).unwrap();
    let (_mmap, manifest) = BinaryChunker::chunk_file(&path).unwrap();
    manifest
}

/// Per-chunk BLAKE3 leaves that SNIP computed, ready for `MerkleTree::build`.
fn snip_leaves(manifest: &sum_types::storage::DataManifest) -> Vec<blake3::Hash> {
    manifest
        .chunks
        .iter()
        .map(|c| blake3::Hash::from(c.blake3_hash))
        .collect()
}

/// Buffer reconstructed from the DOCUMENTED frozen rule (sum-chain golden.rs:30-37):
/// `n = 2*CHUNK + 7`, `byte[i] = (i*31 + 7) & 0xff`. NOT derived from SNIP object
/// code — only the buffer spec is re-derived; the anchor value (the root) stays
/// authoritative.
fn multichunk_buf() -> Vec<u8> {
    let n = 2 * CHUNK + 7;
    let mut buf = vec![0u8; n];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((i as u64 * 31 + 7) & 0xff) as u8;
    }
    buf
}

fn assert_output_slots_ascending(slots: &[SlotDescriptorV1]) {
    for w in slots.windows(2) {
        let a = (w[0].slot_kind.to_repr(), w[0].slot_index);
        let b = (w[1].slot_kind.to_repr(), w[1].slot_index);
        assert!(
            a < b,
            "output manifest slots must be strictly ascending (slot_kind, slot_index): \
             {a:?} !< {b:?}"
        );
    }
}

fn assert_input_slots_ascending(slots: &[InputSlotDescriptorV1]) {
    for w in slots.windows(2) {
        let a = (w[0].slot_kind.to_repr(), w[0].slot_index);
        let b = (w[1].slot_kind.to_repr(), w[1].slot_index);
        assert!(
            a < b,
            "input manifest slots must be strictly ascending (slot_kind, slot_index): \
             {a:?} !< {b:?}"
        );
    }
}

// ── 0. Digest lock (standalone tripwire) ──────────────────────────────────────

#[test]
fn vendored_fixture_digest_matches_hardcoded_upstream_sha256() {
    assert_eq!(
        sha256_hex(V.as_bytes()),
        EXPECTED_SHA256,
        "vendored fixture must hash to the hardcoded upstream digest"
    );
}

// ── 1. empty object — `empty_prior_kv` (bytes + identity) ─────────────────────

#[test]
fn empty_object_matches_frozen_bytes_and_snip_merkle() {
    let j = locked_fixture();
    let want_bytes = jstr(&j, "empty_prior_kv", "bytes");
    let want_id = jstr(&j, "empty_prior_kv", "identity");

    // SNIP-native producer: chunk an empty buffer -> 0 chunks, zero root.
    let manifest = snip_manifest(&[]);
    assert_eq!(manifest.chunk_count, 0);
    assert_eq!(manifest.merkle_root, [0u8; 32]);
    let snip_root = MerkleTree::build(&snip_leaves(&manifest)).root();
    assert_eq!(snip_root.as_bytes(), &[0u8; 32]);

    // Shared commitment SNIP adopts.
    let oc = ObjectCommitmentV1::empty(ObjectKind::PriorKv);
    let bytes = oc.encode();

    // KIND
    assert_eq!(oc.object_kind(), ObjectKind::PriorKv);
    assert_eq!(&bytes[..32], &OBJECT_TAG[..]);
    // BYTE LENGTH (== 80 == vendored length)
    assert_eq!(bytes.len(), 80);
    assert_eq!(bytes.len(), unhex(&want_bytes).len());
    // CHUNK COUNT (empty -> 0)
    assert_eq!(oc.chunk_count(), 0);
    assert_eq!(oc.byte_len(), 0);
    // MERKLE ROOT (empty -> [0;32]) + #40 tie-in: shared root == SNIP MerkleTree root
    assert_eq!(oc.merkle_root(), [0u8; 32]);
    assert_eq!(oc.merkle_root(), manifest.merkle_root);
    assert_eq!(&oc.merkle_root(), snip_root.as_bytes());
    // EXACT ENCODED BYTES (hex vs vendored)
    assert_eq!(hx(&bytes), want_bytes);
    // FINAL COMMITMENT IDENTITY
    assert_eq!(hx(&oc.identity()), want_id);

    // Strict decode -> re-encode round-trip identity.
    let decoded = ObjectCommitmentV1::decode_exact(&bytes).unwrap();
    assert_eq!(decoded, oc);
    assert_eq!(decoded.encode(), bytes);
    assert_eq!(decoded.identity(), oc.identity());
}

// ── 2. single-chunk object — `object_commitment_model_golden` ─────────────────

#[test]
fn single_chunk_object_matches_frozen_bytes_and_snip_merkle() {
    let j = locked_fixture();
    let want_bytes = jstr(&j, "object_commitment_model_golden", "bytes");
    let want_id = jstr(&j, "object_commitment_model_golden", "identity");

    // Provenance: commit(Model, b"golden-model") — 12 bytes -> 1 chunk.
    let data = b"golden-model";

    // SNIP-native producer path.
    let manifest = snip_manifest(data);
    assert_eq!(manifest.chunk_count, 1);
    assert_eq!(manifest.total_size_bytes, data.len() as u64);
    let snip_root = MerkleTree::build(&snip_leaves(&manifest)).root();
    assert_eq!(snip_root.as_bytes(), &manifest.merkle_root);

    // Shared commitment SNIP adopts.
    let oc = ObjectCommitmentV1::commit(ObjectKind::Model, data).unwrap();
    let bytes = oc.encode();

    // KIND
    assert_eq!(oc.object_kind(), ObjectKind::Model);
    assert_eq!(&bytes[..32], &OBJECT_TAG[..]);
    // BYTE LENGTH
    assert_eq!(bytes.len(), 80);
    assert_eq!(bytes.len(), unhex(&want_bytes).len());
    // CHUNK COUNT (1) + byte_len
    assert_eq!(oc.chunk_count(), 1);
    assert_eq!(oc.chunk_count(), manifest.chunk_count);
    assert_eq!(oc.byte_len(), data.len() as u64);
    // MERKLE ROOT — #40 tie-in: shared commitment root == SNIP MerkleTree root
    assert_eq!(oc.merkle_root(), manifest.merkle_root);
    assert_eq!(&oc.merkle_root(), snip_root.as_bytes());
    // EXACT ENCODED BYTES
    assert_eq!(hx(&bytes), want_bytes);
    // FINAL COMMITMENT IDENTITY
    assert_eq!(hx(&oc.identity()), want_id);

    // Strict decode -> re-encode round-trip identity.
    let decoded = ObjectCommitmentV1::decode_exact(&bytes).unwrap();
    assert_eq!(decoded, oc);
    assert_eq!(decoded.encode(), bytes);
    assert_eq!(decoded.identity(), oc.identity());
}

// ── 3. three-chunk (chunk-boundary) Merkle root — `merkle_multichunk_root` ─────
//
// The fixture freezes ONLY the bare 32-byte root, not a full ObjectCommitmentV1
// over this buffer. A full multi-chunk ObjectCommitmentV1 byte/identity vector is
// DEFERRED to after 0.2.2; here we anchor only merkle_root / byte_len / chunk_count.

#[test]
fn three_chunk_merkle_root_matches_frozen_and_snip_merkle() {
    let j = locked_fixture();
    let want_root = jbare(&j, "merkle_multichunk_root");

    let buf = multichunk_buf();
    assert_eq!(buf.len(), 2 * CHUNK + 7);

    // SNIP-native producer path: chunk_file -> DataManifest -> MerkleTree::build.
    let manifest = snip_manifest(&buf);
    assert_eq!(manifest.chunk_count, 3);
    assert_eq!(manifest.total_size_bytes, buf.len() as u64);
    let snip_root = MerkleTree::build(&snip_leaves(&manifest)).root();
    assert_eq!(snip_root.as_bytes(), &manifest.merkle_root);
    // SNIP-native Merkle root == frozen authoritative root.
    assert_eq!(hx(snip_root.as_bytes()), want_root);

    // Shared commitment over the SAME buffer (root/byte_len/chunk_count only).
    let oc = ObjectCommitmentV1::commit(ObjectKind::Model, &buf).unwrap();
    assert_eq!(oc.object_kind(), ObjectKind::Model);
    assert_eq!(oc.byte_len(), (2 * CHUNK + 7) as u64);
    assert_eq!(oc.chunk_count(), 3);
    assert_eq!(oc.chunk_count(), manifest.chunk_count);
    // MERKLE ROOT — frozen anchor + #40 tie-in.
    assert_eq!(hx(&oc.merkle_root()), want_root);
    assert_eq!(oc.merkle_root(), manifest.merkle_root);
    assert_eq!(&oc.merkle_root(), snip_root.as_bytes());
}

// ── 4. two-slot output manifest — `output_manifest_2slot` ─────────────────────

#[test]
fn two_slot_output_manifest_matches_frozen_bytes_and_commitment() {
    let j = locked_fixture();
    let want_bytes = jstr(&j, "output_manifest_2slot", "bytes");
    let want_commit_id = jstr(&j, "output_manifest_2slot", "commitment_identity");

    // SNIP-side slot data (documented golden inputs). Strict ascending
    // (slot_kind, slot_index): (ResidualStream=0, 7) < (KvCache=1, 7).
    let manifest = OutputManifestV1 {
        slots: vec![
            SlotDescriptorV1 {
                slot_kind: SlotKind::ResidualStream,
                slot_index: 7,
                commitment: ObjectCommitmentV1::commit(ObjectKind::ResidualState, b"g").unwrap(),
            },
            SlotDescriptorV1 {
                slot_kind: SlotKind::KvCache,
                slot_index: 7,
                commitment: ObjectCommitmentV1::commit(ObjectKind::KvState, b"g").unwrap(),
            },
        ],
    };
    let bytes = manifest.try_encode().unwrap();

    // KIND: leading tag + slot_kind <-> embedded object_kind binding.
    assert_eq!(&bytes[..32], &OUTPUT_MANIFEST_TAG[..]);
    for slot in &manifest.slots {
        assert_eq!(slot.commitment.object_kind(), slot.slot_kind.object_kind());
    }
    // BYTE LENGTH (== 38 + 85*n == vendored length)
    assert_eq!(bytes.len(), 38 + 2 * 85);
    assert_eq!(bytes.len(), unhex(&want_bytes).len());
    // CHUNK COUNT via each embedded commitment (1-byte "g" -> 1 chunk).
    for slot in &manifest.slots {
        assert_eq!(slot.commitment.chunk_count(), 1);
    }
    // SLOT ORDERING: strict ascending.
    assert_output_slots_ascending(&manifest.slots);
    // EXACT ENCODED BYTES
    assert_eq!(hx(&bytes), want_bytes);
    // FINAL COMMITMENT IDENTITY
    let commitment = manifest.try_commitment().unwrap();
    assert_eq!(commitment.object_kind(), ObjectKind::OutputManifest);
    assert_eq!(hx(&commitment.identity()), want_commit_id);

    // Strict decode -> re-encode round-trip identity.
    let decoded = OutputManifestV1::decode_exact(&bytes).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(decoded.try_encode().unwrap(), bytes);
    assert_eq!(
        decoded.try_commitment().unwrap().identity(),
        commitment.identity()
    );
    assert_output_slots_ascending(&decoded.slots);
}

// ── 5. three-slot input manifest — `input_manifest_3slot` ─────────────────────

#[test]
fn three_slot_input_manifest_matches_frozen_bytes_and_commitment() {
    let j = locked_fixture();
    let want_bytes = jstr(&j, "input_manifest_3slot", "bytes");
    let want_commit_id = jstr(&j, "input_manifest_3slot", "commitment_identity");

    // SNIP-side slot data (documented golden inputs). Strict ascending by
    // slot_kind: PriorResidual=0 < PriorKv=1 < TokenPrefix=2, all slot_index 0.
    let manifest = InputManifestV1 {
        slots: vec![
            InputSlotDescriptorV1 {
                slot_kind: InputSlotKind::PriorResidual,
                slot_index: 0,
                commitment: ObjectCommitmentV1::commit(ObjectKind::PriorResidual, b"g").unwrap(),
            },
            InputSlotDescriptorV1 {
                slot_kind: InputSlotKind::PriorKv,
                slot_index: 0,
                commitment: ObjectCommitmentV1::commit(ObjectKind::PriorKv, b"g").unwrap(),
            },
            InputSlotDescriptorV1 {
                slot_kind: InputSlotKind::TokenPrefix,
                slot_index: 0,
                commitment: ObjectCommitmentV1::commit(ObjectKind::TokenPrefix, b"g").unwrap(),
            },
        ],
    };
    let bytes = manifest.try_encode().unwrap();

    // KIND: leading tag + slot_kind <-> embedded object_kind binding.
    assert_eq!(&bytes[..32], &INPUT_MANIFEST_TAG[..]);
    for slot in &manifest.slots {
        assert_eq!(slot.commitment.object_kind(), slot.slot_kind.object_kind());
    }
    // BYTE LENGTH (== 38 + 85*n == vendored length)
    assert_eq!(bytes.len(), 38 + 3 * 85);
    assert_eq!(bytes.len(), unhex(&want_bytes).len());
    // CHUNK COUNT via each embedded commitment (1-byte "g" -> 1 chunk).
    for slot in &manifest.slots {
        assert_eq!(slot.commitment.chunk_count(), 1);
    }
    // SLOT ORDERING: strict ascending.
    assert_input_slots_ascending(&manifest.slots);
    // EXACT ENCODED BYTES
    assert_eq!(hx(&bytes), want_bytes);
    // FINAL COMMITMENT IDENTITY
    let commitment = manifest.try_commitment().unwrap();
    assert_eq!(commitment.object_kind(), ObjectKind::InputManifest);
    assert_eq!(hx(&commitment.identity()), want_commit_id);

    // Strict decode -> re-encode round-trip identity.
    let decoded = InputManifestV1::decode_exact(&bytes).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(decoded.try_encode().unwrap(), bytes);
    assert_eq!(
        decoded.try_commitment().unwrap().identity(),
        commitment.identity()
    );
    assert_input_slots_ascending(&decoded.slots);
}
