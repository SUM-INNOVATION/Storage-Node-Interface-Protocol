//! WS2a — mock-driven Phase 4 lifecycle integration suite.
//!
//! Eight scenarios exercising the SNIP-side workflow contracts that
//! Phase 4a / 4b / 4c / 4d ship. Each scenario is hermetic: no Docker,
//! no network, no live RPC. All cryptographic + chain-shape primitives
//! are the production helpers (`sum_crypto::encrypt_chunk`, etc.) —
//! tests build chain-shaped fixtures, drive the production decode /
//! decrypt / lookup paths, and assert workflow contracts hold
//! end-to-end.
//!
//! Scope per the WS2a plan:
//!
//!   * Layer 1 only. Layer 2 (real local-mirror, real CLI subprocess
//!     drive-through) lands in WS2b after this is green and the
//!     mirror has been manually smoke-tested.
//!   * No sleeps. Finality is mocked by passing explicit
//!     `finalized_height` values to the helpers under test.
//!   * No real key material. Test seeds in
//!     [`common`](common/mod.rs) are constant byte patterns clearly
//!     labelled as fixtures, never as funded production keys.
//!   * No production-code refactors. Where production orchestrators
//!     are concrete-coupled to `Arc<L1RpcClient>` / `Arc<SumNet>`
//!     (e.g. `run_download_private`, `run_share`), the suite tests
//!     the same stage-level components those orchestrators use, so
//!     each scenario still pins the workflow contract that matters.
//!
//! The scenarios:
//!
//!   1. Public manifest + chunk artifacts round-trip via the public
//!      decode + Merkle-verify path.
//!   2. Private owner-only ingest → owner download round-trip via
//!      access-list lookup + bundle unwrap + decrypt.
//!   3. Private shared (owner + one recipient) ingest → recipient
//!      download round-trip.
//!   4. Share simulated by chain-row mutation → new recipient gains
//!      access using the same K_file.
//!   5. Revoke simulated by chain-row mutation → revoked recipient
//!      hits typed `NoAccess`.
//!   6. Update-access expiry: strict-greater rule against an
//!      explicitly-supplied finalized height.
//!   7. Resume contract: K_file recovery from the owner's on-chain
//!      bundle is deterministic — re-encrypting with the recovered
//!      key reproduces the original Merkle root.
//!   8. Manifest Merkle invariant: a tampered chunk hash inside the
//!      manifest is rejected at `decrypt_and_verify_manifest`.

mod common;

use sum_crypto::{decrypt_chunk, decrypt_manifest, unwrap_for_self};
use sum_node::download_private::{
    PrivateDownloadError, check_access_expiry, decrypt_and_verify_chunk,
    decrypt_and_verify_manifest, find_my_access_entry, parse_bundle_hex,
};
use sum_store::manifest::deserialize_manifest_cbor;
use sum_store::merkle::MerkleTree;
use sum_types::rpc_types::AccessEntryV2;

use common::{
    OWNER_ADDR, OWNER_SEED, RECIPIENT_ADDR, RECIPIENT_SEED, THIRD_PARTY_ADDR, THIRD_PARTY_SEED,
    addr_b58, build_private_test_artifacts, build_public_test_artifacts, make_access_entry,
    make_test_kfile, private_active_file_info, small_plaintext, x25519_pub_for_seed,
    x25519_secret_for_seed,
};

// ── 1. Public artifacts round-trip ──────────────────────────────────────────

/// **Phase 4 public-path contract.** Public V2 ingest emits
/// chain-shaped artifacts (a CBOR-serialized `DataManifest` + raw
/// chunk bytes) that any peer must be able to decode and verify
/// without secrets. This pins:
///
///   * Manifest CBOR round-trips byte-for-byte through
///     `deserialize_manifest_cbor`.
///   * The manifest's `merkle_root` matches a fresh `MerkleTree`
///     rebuild from the per-chunk hashes (chain ⇄ manifest
///     consistency).
///   * The chunk's CID matches `cid_from_data` recomputation.
#[test]
fn public_manifest_round_trip_via_helpers() {
    let plaintext = small_plaintext();
    let arts = build_public_test_artifacts(&plaintext);

    // CBOR round-trip the manifest.
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(&arts.manifest, &mut cbor).unwrap();
    let recovered = deserialize_manifest_cbor(&cbor).expect("manifest CBOR roundtrips");
    assert_eq!(recovered.merkle_root, arts.merkle_root);
    assert_eq!(recovered.chunk_count, 1);
    assert_eq!(recovered.chunks[0].chunk_index, 0);

    // Merkle root rebuild matches.
    let leaves: Vec<blake3::Hash> = arts
        .manifest
        .chunks
        .iter()
        .map(|c| blake3::Hash::from(c.blake3_hash))
        .collect();
    let rebuilt = *MerkleTree::build(&leaves).root().as_bytes();
    assert_eq!(rebuilt, arts.merkle_root);

    // Chunk CID is content-addressable.
    let recomputed_cid = sum_store::cid_from_data(&arts.chunks[0]);
    assert_eq!(recomputed_cid, arts.manifest.chunks[0].cid);
}

// ── 2. Private owner-only round-trip ────────────────────────────────────────

/// **Phase 4a + 4b owner-only contract.** Owner ingests a Private
/// file with no recipients. Their own access entry holds a bundle
/// wrapping `K_file` for their X25519 pubkey. Download: find the
/// owner's entry → unwrap K_file → decrypt manifest → decrypt
/// chunk → plaintext matches. End-to-end workflow contract for the
/// owner-only case.
#[tokio::test]
async fn private_owner_only_round_trip_via_helpers() {
    let plaintext = small_plaintext();
    let k_file = make_test_kfile();
    let arts = build_private_test_artifacts(&plaintext, &k_file);

    // Chain row carries owner-only access list.
    let owner_pk = x25519_pub_for_seed(&OWNER_SEED);
    let owner_entry = make_access_entry(&OWNER_ADDR, &owner_pk, &k_file, None);
    let info = private_active_file_info(arts.merkle_root, &OWNER_ADDR, 1, vec![owner_entry]);

    // Owner's download path: lookup → unwrap → decrypt.
    let root_hex = info.merkle_root.clone();
    let entry = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info.clone(),
        },
        &root_hex,
        &addr_b58(&OWNER_ADDR),
        &info,
    )
    .await
    .expect("owner finds own entry");

    let bundle = parse_bundle_hex(entry.encrypted_key_bundle.as_ref().unwrap())
        .expect("owner bundle parses");
    let recovered_kfile =
        unwrap_for_self(&bundle, &x25519_secret_for_seed(&OWNER_SEED), &OWNER_ADDR)
            .expect("owner unwraps own bundle");
    assert_eq!(
        recovered_kfile,
        *k_file.as_ref(),
        "K_file recovered from owner's chain bundle must equal the K_file used at ingest"
    );

    // Decrypt manifest + chunk → plaintext matches.
    let recovered_kfile_z = zeroize::Zeroizing::new(recovered_kfile);
    let manifest = decrypt_and_verify_manifest(
        &recovered_kfile_z,
        &arts.encrypted_manifest_bytes,
        arts.merkle_root,
    )
    .expect("decrypt manifest");
    assert_eq!(manifest.merkle_root, arts.merkle_root);
    let recovered_pt = decrypt_and_verify_chunk(
        &recovered_kfile_z,
        &manifest.chunks[0],
        &arts.ciphertext_chunks[0],
    )
    .expect("decrypt chunk");
    assert_eq!(recovered_pt, plaintext);
}

// ── 3. Private shared recipient round-trip ──────────────────────────────────

/// **Phase 4a + 4b shared-recipient contract.** Owner ingests
/// Private file with one recipient. Recipient's access entry
/// wraps the SAME K_file for their X25519 pubkey. Recipient (NOT
/// owner) drives the download path; pins that the wrap/unwrap
/// works for non-owners.
#[tokio::test]
async fn private_shared_recipient_round_trip_via_helpers() {
    let plaintext = small_plaintext();
    let k_file = make_test_kfile();
    let arts = build_private_test_artifacts(&plaintext, &k_file);

    let owner_entry = make_access_entry(
        &OWNER_ADDR,
        &x25519_pub_for_seed(&OWNER_SEED),
        &k_file,
        None,
    );
    let recipient_entry = make_access_entry(
        &RECIPIENT_ADDR,
        &x25519_pub_for_seed(&RECIPIENT_SEED),
        &k_file,
        None,
    );
    let info = private_active_file_info(
        arts.merkle_root,
        &OWNER_ADDR,
        1,
        vec![owner_entry, recipient_entry],
    );

    // Recipient's download path.
    let root_hex = info.merkle_root.clone();
    let entry = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info.clone(),
        },
        &root_hex,
        &addr_b58(&RECIPIENT_ADDR),
        &info,
    )
    .await
    .expect("recipient finds their entry");

    let bundle = parse_bundle_hex(entry.encrypted_key_bundle.as_ref().unwrap()).unwrap();
    let recovered_kfile = unwrap_for_self(
        &bundle,
        &x25519_secret_for_seed(&RECIPIENT_SEED),
        &RECIPIENT_ADDR,
    )
    .expect("recipient unwraps their bundle");
    assert_eq!(
        recovered_kfile,
        *k_file.as_ref(),
        "recipient must recover the same K_file the owner used at ingest"
    );

    let recovered_kfile_z = zeroize::Zeroizing::new(recovered_kfile);
    let manifest = decrypt_and_verify_manifest(
        &recovered_kfile_z,
        &arts.encrypted_manifest_bytes,
        arts.merkle_root,
    )
    .unwrap();
    let recovered_pt = decrypt_and_verify_chunk(
        &recovered_kfile_z,
        &manifest.chunks[0],
        &arts.ciphertext_chunks[0],
    )
    .unwrap();
    assert_eq!(recovered_pt, plaintext);
}

// ── 4. Share simulated via chain-row mutation ───────────────────────────────

/// **Phase 4c share contract.** Production `run_share` submits an
/// `AddAccessV2` tx; on finality the chain row gains a new access
/// entry wrapping `K_file` for the new recipient. This scenario
/// simulates the post-finality state by appending the new entry
/// directly, then verifies the new recipient's full download path
/// works using the same K_file. Pins the contract that share is
/// "extend access list with a same-K_file bundle" — chain never
/// sees `K_file`, recipient drives standard 4b download.
#[tokio::test]
async fn share_simulated_admits_new_recipient_via_chain_row_mutation() {
    let plaintext = small_plaintext();
    let k_file = make_test_kfile();
    let arts = build_private_test_artifacts(&plaintext, &k_file);

    // Start: owner-only. Then "share with third party" by appending
    // an entry wrapping the SAME K_file for their pubkey. This is
    // exactly what `run_share` produces post-finality.
    let owner_entry = make_access_entry(
        &OWNER_ADDR,
        &x25519_pub_for_seed(&OWNER_SEED),
        &k_file,
        None,
    );
    let third_party_entry = make_access_entry(
        &THIRD_PARTY_ADDR,
        &x25519_pub_for_seed(&THIRD_PARTY_SEED),
        &k_file,
        None,
    );
    let info = private_active_file_info(
        arts.merkle_root,
        &OWNER_ADDR,
        1,
        vec![owner_entry, third_party_entry],
    );

    // Third party drives the download path.
    let entry = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info.clone(),
        },
        &info.merkle_root,
        &addr_b58(&THIRD_PARTY_ADDR),
        &info,
    )
    .await
    .expect("newly-added recipient finds their entry");

    let bundle = parse_bundle_hex(entry.encrypted_key_bundle.as_ref().unwrap()).unwrap();
    let recovered_kfile = unwrap_for_self(
        &bundle,
        &x25519_secret_for_seed(&THIRD_PARTY_SEED),
        &THIRD_PARTY_ADDR,
    )
    .expect("new recipient unwraps own bundle");
    let recovered_kfile_z = zeroize::Zeroizing::new(recovered_kfile);
    let manifest = decrypt_and_verify_manifest(
        &recovered_kfile_z,
        &arts.encrypted_manifest_bytes,
        arts.merkle_root,
    )
    .unwrap();
    let recovered_pt = decrypt_and_verify_chunk(
        &recovered_kfile_z,
        &manifest.chunks[0],
        &arts.ciphertext_chunks[0],
    )
    .unwrap();
    assert_eq!(
        recovered_pt, plaintext,
        "new recipient must round-trip plaintext using the same K_file the owner used at ingest"
    );
}

// ── 5. Revoke simulated via chain-row mutation ──────────────────────────────

/// **Phase 4c revoke contract.** Production `run_revoke` submits a
/// `RemoveAccessV2` tx; on finality the chain row drops the entry
/// for the revoked address. This scenario simulates the
/// post-finality state by removing the entry directly, then verifies
/// the revoked recipient's `find_my_access_entry` lookup returns
/// the typed `NoAccess` error — NOT a silent fall-through, NOT a
/// successful entry resolution.
///
/// Forward secrecy is explicitly NOT in scope (privacy-audit row
/// #14): the revoked party who cached ciphertext + bundle locally
/// can still decrypt past content. This test pins ONLY the
/// chain-side ACL contract.
#[tokio::test]
async fn revoke_simulated_denies_revoked_recipient_via_chain_row_mutation() {
    let plaintext = small_plaintext();
    let k_file = make_test_kfile();
    let arts = build_private_test_artifacts(&plaintext, &k_file);

    // Start state: owner + recipient. Then revoke recipient by
    // dropping their entry — exactly what `run_revoke` produces
    // post-finality.
    let owner_entry = make_access_entry(
        &OWNER_ADDR,
        &x25519_pub_for_seed(&OWNER_SEED),
        &k_file,
        None,
    );
    // Note: recipient_entry intentionally NOT included.
    let info_post_revoke =
        private_active_file_info(arts.merkle_root, &OWNER_ADDR, 1, vec![owner_entry]);

    let result = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info_post_revoke.clone(),
        },
        &info_post_revoke.merkle_root,
        &addr_b58(&RECIPIENT_ADDR),
        &info_post_revoke,
    )
    .await;

    let err = result.expect_err("revoked recipient must NOT find an entry");
    assert!(
        matches!(err, PrivateDownloadError::NoAccess { .. }),
        "revoked recipient must surface as NoAccess, not Rpc/Other; got: {err:?}"
    );

    // Belt-and-suspenders: even if the test fixture got confused,
    // the owner's lookup should still succeed (fixture sanity).
    let owner_lookup = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info_post_revoke.clone(),
        },
        &info_post_revoke.merkle_root,
        &addr_b58(&OWNER_ADDR),
        &info_post_revoke,
    )
    .await;
    assert!(
        owner_lookup.is_ok(),
        "owner must still resolve their own entry after a recipient revoke"
    );
}

// ── 6. Expiry strict-greater rule ───────────────────────────────────────────

/// **Phase 4c update-access expiry contract.** `expires_at` is
/// strict-greater against the finalized height: an entry with
/// `expires_at = N` is still valid at finalized_height = N, and
/// expires at N+1. This is the same rule the chain ACL applies on
/// the serve side; the client-side `check_access_expiry` mirrors it
/// so a download can fail closed before any chunk request goes out.
///
/// Mocked finality: the test passes explicit finalized heights, no
/// wall-clock waits.
#[test]
fn expiry_strict_greater_rule_against_finalized_height() {
    let entry_with_expiry = AccessEntryV2 {
        address: addr_b58(&RECIPIENT_ADDR),
        encrypted_key_bundle: Some(format!("0x{}", "00".repeat(80))),
        expires_at: Some(100),
    };

    // finalized = expires_at: still valid (strict-greater rule).
    assert!(
        check_access_expiry(&entry_with_expiry, 100).is_ok(),
        "finalized_height == expires_at must still pass"
    );

    // finalized = expires_at - 1: well within validity.
    assert!(check_access_expiry(&entry_with_expiry, 99).is_ok());

    // finalized = expires_at + 1: expired.
    let err = check_access_expiry(&entry_with_expiry, 101).expect_err("expired");
    assert!(
        matches!(
            err,
            PrivateDownloadError::AccessExpired {
                expires_at: 100,
                current: 101,
            }
        ),
        "post-expiry must surface AccessExpired with the exact heights, got: {err:?}"
    );

    // None expires_at = never expires.
    let perpetual_entry = AccessEntryV2 {
        address: addr_b58(&RECIPIENT_ADDR),
        encrypted_key_bundle: Some(format!("0x{}", "00".repeat(80))),
        expires_at: None,
    };
    assert!(check_access_expiry(&perpetual_entry, u64::MAX).is_ok());
}

// ── 7. Resume — K_file recovery determinism ─────────────────────────────────

/// **Phase 4d resume contract.** `IngestPipeline::resume` recovers
/// `K_file` from the owner's on-chain access bundle (that's what
/// the chain stores for every Private file's owner) and re-derives
/// the file's encrypted artifacts deterministically. This test
/// pins the load-bearing property: the K_file recovered from the
/// chain bundle MUST equal the K_file used at ingest, and
/// re-encrypting the same plaintext with the recovered key MUST
/// produce the same Merkle root.
///
/// If this property breaks, resume cannot match its chunks against
/// the chain row's `merkle_root`, and the pipeline would surface
/// `RootMismatch` — even though the recipient + owner workflow is
/// otherwise correct.
#[tokio::test]
async fn resume_recovers_kfile_from_chain_bundle_deterministically() {
    let plaintext = small_plaintext();
    let k_file_at_ingest = make_test_kfile();

    // 1. Original ingest: encrypt → chain row → owner bundle.
    let arts_at_ingest = build_private_test_artifacts(&plaintext, &k_file_at_ingest);
    let owner_pk = x25519_pub_for_seed(&OWNER_SEED);
    let owner_entry = make_access_entry(&OWNER_ADDR, &owner_pk, &k_file_at_ingest, None);
    let info = private_active_file_info(
        arts_at_ingest.merkle_root,
        &OWNER_ADDR,
        1,
        vec![owner_entry],
    );

    // 2. Simulate "lost local state": forget K_file_at_ingest.
    //    Resume's only inputs from this point forward are: the chain
    //    row (which the test still has) and the owner's seed (which
    //    the operator persists out-of-band). NOT the original K_file.

    // 3. Resume: find owner entry on chain → unwrap to K_file_recovered.
    let entry = find_my_access_entry(
        &common::StaticAccessRpc {
            first_page: info.clone(),
        },
        &info.merkle_root,
        &addr_b58(&OWNER_ADDR),
        &info,
    )
    .await
    .expect("owner finds own entry");
    let bundle = parse_bundle_hex(entry.encrypted_key_bundle.as_ref().unwrap()).unwrap();
    let k_file_recovered =
        unwrap_for_self(&bundle, &x25519_secret_for_seed(&OWNER_SEED), &OWNER_ADDR)
            .expect("recover K_file from chain bundle");

    // 4. The load-bearing assertion: same K_file.
    assert_eq!(
        k_file_recovered,
        *k_file_at_ingest.as_ref(),
        "recovered K_file must equal the K_file used at ingest — \
         resume's whole correctness story rides on this"
    );

    // 5. Deterministic re-derivation: re-encrypt the same plaintext
    //    with the recovered key → same merkle_root. If this
    //    property breaks, resume's re-derived chunks would diverge
    //    from the chain root and surface as RootMismatch.
    let k_file_recovered_z = zeroize::Zeroizing::new(k_file_recovered);
    let arts_after_resume = build_private_test_artifacts(&plaintext, &k_file_recovered_z);
    assert_eq!(
        arts_after_resume.merkle_root, arts_at_ingest.merkle_root,
        "re-encrypting plaintext with the recovered K_file must reproduce the original merkle_root"
    );
    // Belt-and-suspenders: ciphertext bytes are byte-identical too
    // (encrypt_chunk derives per-chunk key + nonce via HKDF over
    // (K_file, chunk_index) — deterministic given the same inputs).
    assert_eq!(
        arts_after_resume.ciphertext_chunks, arts_at_ingest.ciphertext_chunks,
        "deterministic encryption must yield byte-identical ciphertext on resume"
    );
}

// ── 8. Manifest Merkle invariant ────────────────────────────────────────────

/// **Phase 4 manifest integrity contract.** The manifest's per-chunk
/// `blake3_hash` field is what the manifest's own `merkle_root`
/// commits to — and that root is what the CHAIN commits to via
/// `RegisterFilePendingV2.merkle_root`. If a peer ships a manifest
/// whose chunk hashes don't rebuild to the chain's root,
/// `decrypt_and_verify_manifest` MUST reject it with
/// `ManifestMerkleMismatch`. Otherwise a tampered manifest (e.g.,
/// swapping in a chunk-hash that points to malicious ciphertext)
/// would be accepted, and downstream chunk-fetch-by-CID would pull
/// + accept the wrong bytes.
#[tokio::test]
async fn manifest_merkle_invariant_rejects_tampered_chunk_hash() {
    let plaintext = small_plaintext();
    let k_file = make_test_kfile();
    let arts = build_private_test_artifacts(&plaintext, &k_file);

    // Tamper: modify the (only) chunk's blake3_hash, re-encrypt the
    // manifest, but keep the chain root the same. The tampered
    // manifest still decrypts cleanly (AEAD tag is over the new
    // CBOR), but its chunks-rebuild now != chain root.
    let mut tampered_manifest = arts.manifest.clone();
    tampered_manifest.chunks[0].blake3_hash = [0xFFu8; 32];

    let mut tampered_cbor = Vec::new();
    ciborium::ser::into_writer(&tampered_manifest, &mut tampered_cbor).unwrap();
    let tampered_encrypted = sum_crypto::encrypt_manifest(&k_file, &tampered_cbor);

    // Decrypt-and-verify against the ORIGINAL chain root must reject.
    // Note: the manifest's INTERNAL merkle_root field is also
    // attacker-controlled (whatever the tampered manifest contains),
    // so we may hit `ManifestRootMismatch` first if the attacker
    // didn't bother to update the field, or `ManifestMerkleMismatch`
    // if they did. Either is a correct rejection — both pin the
    // contract "tampered manifest is rejected, not silently
    // accepted." The test accepts either to remain robust to the
    // tamper-strategy choice.
    let result = decrypt_and_verify_manifest(&k_file, &tampered_encrypted, arts.merkle_root);
    let err = result.expect_err("tampered manifest must be rejected");
    assert!(
        matches!(
            err,
            PrivateDownloadError::ManifestRootMismatch { .. }
                | PrivateDownloadError::ManifestMerkleMismatch
        ),
        "expected ManifestRootMismatch or ManifestMerkleMismatch, got: {err:?}"
    );

    // Sanity: the un-tampered artifact path round-trips cleanly. If
    // this fails the test fixture is broken, not the production
    // path.
    let recovered =
        decrypt_and_verify_manifest(&k_file, &arts.encrypted_manifest_bytes, arts.merkle_root);
    assert!(
        recovered.is_ok(),
        "fixture sanity: original manifest must verify against original root"
    );

    // Plaintext round-trip via decrypt_manifest (lower-level: no
    // root check). Pins that the AEAD layer is functioning — if THIS
    // fails, the test fixture's encryption is broken.
    let plain = decrypt_manifest(&k_file, &arts.encrypted_manifest_bytes).unwrap();
    let _: sum_types::storage::DataManifest =
        ciborium::de::from_reader(&plain[..]).expect("plain CBOR roundtrips");

    // Plaintext chunk round-trip via the lower-level decrypt_chunk.
    // Pins the chunk-level AEAD layer end-to-end against the
    // fixture, independent of the manifest layer.
    let pt = decrypt_chunk(&k_file, 0, &arts.ciphertext_chunks[0]).unwrap();
    assert_eq!(pt, plaintext);
}
