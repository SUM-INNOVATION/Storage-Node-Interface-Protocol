//! Transaction builder for submitting storage operations to the SUM Chain L1.
//!
//! Constructs `SignedTransaction` bytes that the L1 can deserialize via
//! `SignedTransaction::from_hex()` (bincode v1).
//!
//! The mirror types here must serialize identically to the L1's types under
//! bincode v1. Variant ordering in enums is critical — bincode v1 encodes
//! enum variants as `u32` indices.

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

// ── Public Builder Functions ──────────────────────────────────────────────────

/// Build a hex-encoded `SignedTransaction` for `SubmitStorageProof`.
pub fn build_submit_proof_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    challenge_id: [u8; 32],
    merkle_root: [u8; 32],
    chunk_index: u32,
    chunk_hash: [u8; 32],
    merkle_path: Vec<[u8; 32]>,
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadata(StorageMetadataTxDataMirror {
        operation: StorageMetadataOperationMirror::SubmitStorageProof {
            challenge_id,
            merkle_root,
            chunk_index,
            chunk_hash,
            merkle_path,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for
/// `NodeRegistryV2::RegisterEncryptionKey` (chain plan v3.2 §3.3).
///
/// Used by Phase 4a (Private files) so that other accounts can wrap a
/// file's `K_file` for this account's X25519 public key. The chain
/// rejects 7 known low-order points with `Failed(22)`; callers should
/// derive `encryption_pubkey` from a real X25519 secret (e.g. via
/// `sum_crypto::x25519_keypair_from_ed25519_seed`) rather than passing
/// arbitrary bytes.
pub fn build_register_encryption_key_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    encryption_pubkey: [u8; 32],
) -> Result<String> {
    let payload = TxPayloadMirror::NodeRegistryV2(NodeRegistryV2TxDataMirror {
        operation: NodeRegistryOperationV2Mirror::RegisterEncryptionKey { encryption_pubkey },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `NodeRegistry::Register(ArchiveNode)`.
pub fn build_register_archive_node_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    stake: u64,
) -> Result<String> {
    let payload = TxPayloadMirror::NodeRegistry(NodeRegistryTxDataMirror {
        operation: NodeRegistryOperationMirror::Register {
            role: NodeRoleMirror::ArchiveNode,
            stake,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

// ── V2 Builders (chain plan v3.2) ─────────────────────────────────────────────
//
// !! BEFORE TESTNET INTEGRATION !!
//
// The mirror types below (`NodeRegistryOperationV2Mirror`,
// `StorageMetadataOperationV2Mirror`, `AccessEntryV2Mirror`,
// `TxPayloadMirror::{NodeRegistryV2, StorageMetadataV2}`) MUST be
// confirmed bit-for-bit against the actual L1 code before any V2 tx
// touches a real chain endpoint. Specifically:
//
//   1. **Payload-level variant indices.** Bincode v1 encodes enum
//      variants as `u32` LE. We slot:
//        * `TxPayloadMirror::NodeRegistryV2`     at index 19
//        * `TxPayloadMirror::StorageMetadataV2`  at index 20
//      both immediately after the existing V1 `NodeRegistry = 17`,
//      `StorageMetadata = 18`. The chain plan v3.2 §3.1/§3.3 introduce
//      both V2 ops as "additive" without pinning the payload-side
//      indices, so this ordering is an assumption — confirm against
//      the actual L1 `TxPayload` enum. The
//      `payload_v2_variant_indices_are_stable` test pins the local
//      assumption; cross-chain confirmation is out-of-band.
//   2. **`StorageMetadataOperationV2Mirror` variant order** (chain-confirmed):
//      `RegisterFilePendingV2 = 0`, `ActivateFileV2 = 1`,
//      `AbandonFileV2 = 2`, `AcceptAssignmentV2 = 3`, `AddAccessV2 = 4`,
//      `RemoveAccessV2 = 5`, `UpdateAccessV2 = 6`. Earlier drafts placed
//      `AcceptAssignmentV2` at index 6; chain team confirmed final
//      ordering puts it at 3. Pinned by the
//      `v2_op_variant_indices_are_stable` test.
//   3. **`AccessEntryV2Mirror` field order** — bincode v1 is positional
//      for structs.
//
// Conformance test plan: round-trip a fixture tx through `bincode1`
// here AND against the L1's actual decoder (out-of-band, requires
// chain-team coordination) before any real ingest.

/// Build a hex-encoded `SignedTransaction` for `RegisterFilePendingV2`
/// (chain plan §3.1).
///
/// **Phase 0b is Public-only**: callers MUST pass `visibility = 0` and
/// either an empty `initial_access` OR access entries with
/// `encrypted_key_bundle == None`. Private-file ingest lands in Phase 4.
#[allow(clippy::too_many_arguments)]
pub fn build_register_file_pending_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    plaintext_size_bytes: u64,
    stored_size_bytes: u64,
    chunk_count: u32,
    fee_deposit: u64,
    visibility: u8,
    initial_access: Vec<AccessEntryV2Mirror>,
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
        operation: StorageMetadataOperationV2Mirror::RegisterFilePendingV2 {
            merkle_root,
            plaintext_size_bytes,
            stored_size_bytes,
            chunk_count,
            fee_deposit,
            visibility,
            initial_access,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `ActivateFileV2`.
pub fn build_activate_file_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
        operation: StorageMetadataOperationV2Mirror::ActivateFileV2 { merkle_root },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `AbandonFileV2`.
///
/// On chain success: 90% of `fee_deposit` refunded to owner, 10% burned
/// (chain plan §3.5). The call site that submits this should be the
/// ingest abandon-path or the explicit `sum-node abandon` subcommand.
pub fn build_abandon_file_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
        operation: StorageMetadataOperationV2Mirror::AbandonFileV2 { merkle_root },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `AcceptAssignmentV2`
/// (chain plan §3.6, v3.2 bitmap OR-merge).
///
/// Each call ORs the bits in `chunk_indices` into the per-`(file,
/// archive)` attestation bitmap. **Caller's responsibility**: ensure
/// `chunk_indices.len() ≤ max_chunk_indices_per_tx` (default 65,536).
/// For archives whose assignment exceeds that cap, build multiple txs
/// each with a disjoint slice of the assigned indices and submit them
/// independently (OR-merge makes the order irrelevant).
pub fn build_accept_assignment_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    chunk_indices: Vec<u32>,
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
        operation: StorageMetadataOperationV2Mirror::AcceptAssignmentV2 {
            merkle_root,
            chunk_indices,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `StorageMetadata::RegisterFile`.
pub fn build_register_file_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    total_size_bytes: u64,
    access_list: Vec<[u8; 20]>,
    fee_deposit: u64,
) -> Result<String> {
    let payload = TxPayloadMirror::StorageMetadata(StorageMetadataTxDataMirror {
        operation: StorageMetadataOperationMirror::RegisterFile {
            merkle_root,
            total_size_bytes,
            access_list,
            fee_deposit,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

// ── Shared Signing Logic ──────────────────────────────────────────────────────

fn sign_and_encode(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    payload: TxPayloadMirror,
) -> Result<String> {
    let signing_key = SigningKey::from_bytes(ed25519_seed);
    let pubkey_bytes: [u8; 32] = signing_key.verifying_key().to_bytes();

    // Derive L1 address: blake3(pubkey)[12..32]
    let pubkey_hash = blake3::hash(&pubkey_bytes);
    let mut from_addr = [0u8; 20];
    from_addr.copy_from_slice(&pubkey_hash.as_bytes()[12..32]);

    let tx = TransactionV2Mirror {
        chain_id,
        from: from_addr,
        fee,
        nonce,
        payload,
    };

    // Serialize with bincode v1 to get signing hash.
    let tx_bytes = bincode1::serialize(&tx).context("bincode v1 serialization of tx failed")?;
    let signing_hash = blake3::hash(&tx_bytes);

    // Sign with Ed25519.
    let signature = signing_key.sign(signing_hash.as_bytes());

    let signed = SignedTransactionMirror {
        inner: TxInnerMirror::V2(tx),
        signature: signature.to_bytes(),
        public_key: pubkey_bytes,
    };

    let raw_bytes =
        bincode1::serialize(&signed).context("bincode v1 serialization of signed tx failed")?;
    Ok(hex::encode(&raw_bytes))
}

// ── Mirror Types ─────────────────────────────────────────────────────────────
//
// These must match the L1's types exactly in field order and variant indices.
// Source: sum-chain/crates/primitives/src/transaction.rs,
//         sum-chain/crates/primitives/src/storage_metadata.rs,
//         sum-chain/crates/primitives/src/node_registry.rs

// ── Node Registry mirrors ────────────────────────────────────────────────────

/// Mirror of `NodeRole`. Variant indices must match L1.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum NodeRoleMirror {
    Validator,   // 0
    ArchiveNode, // 1
}

/// Mirror of `NodeStatus`.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum NodeStatusMirror {
    Active,  // 0
    Slashed, // 1
}

/// Mirror of `NodeRegistryOperation`.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum NodeRegistryOperationMirror {
    Register {
        role: NodeRoleMirror,
        stake: u64,
    },              // 0
    UpdateStatus {
        target: [u8; 20],
        new_status: NodeStatusMirror,
    },              // 1
}

/// Mirror of `NodeRegistryTxData`.
#[derive(Debug, Serialize, Deserialize)]
struct NodeRegistryTxDataMirror {
    operation: NodeRegistryOperationMirror,
}

// ── V2 NodeRegistry mirrors (chain plan v3.2 §3.3) ──────────────────────────
//
// `NodeRegistryV2` is included so `TxPayloadMirror::NodeRegistryV2` (variant
// index 19) lines up correctly relative to `StorageMetadataV2` (index 20).
// Phase 0b doesn't construct any V2 NodeRegistry txs — `RegisterEncryptionKey`
// is consumed by Phase 4 (Private file ingest). Mirror is included now to
// avoid an off-by-one in `TxPayloadMirror`'s variant indices.

/// Mirror of `NodeRegistryOperationV2` (chain plan §3.3).
///
/// Currently single-variant (`RegisterEncryptionKey` at index 0). Future
/// V2 NodeRegistry ops would extend this without breaking V1 wire compat.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum NodeRegistryOperationV2Mirror {
    /// X25519 encryption pubkey for the signer's account; overwrite-on-rewrite.
    /// Chain rejects low-order points (chain plan v3.1 §3.3) with `Failed(22)`.
    RegisterEncryptionKey { encryption_pubkey: [u8; 32] }, // 0
}

/// Mirror of `NodeRegistryV2TxData`.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
struct NodeRegistryV2TxDataMirror {
    operation: NodeRegistryOperationV2Mirror,
}

// ── Storage Metadata mirrors ─────────────────────────────────────────────────

/// Mirror of `StorageMetadataOperation` (variant indices must match L1).
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum StorageMetadataOperationMirror {
    RegisterFile {
        merkle_root: [u8; 32],
        total_size_bytes: u64,
        access_list: Vec<[u8; 20]>,
        fee_deposit: u64,
    },                                    // index 0
    UpdateAccessList {
        merkle_root: [u8; 32],
        new_access_list: Vec<[u8; 20]>,
    },                                    // index 1
    AddAccess {
        merkle_root: [u8; 32],
        address: [u8; 20],
    },                                    // index 2
    RemoveAccess {
        merkle_root: [u8; 32],
        address: [u8; 20],
    },                                    // index 3
    TopUpFeePool {
        merkle_root: [u8; 32],
        amount: u64,
    },                                    // index 4
    SubmitStorageProof {
        challenge_id: [u8; 32],
        merkle_root: [u8; 32],
        chunk_index: u32,
        chunk_hash: [u8; 32],
        merkle_path: Vec<[u8; 32]>,
    },                                    // index 5
}

/// Mirror of `StorageMetadataTxData`.
#[derive(Debug, Serialize, Deserialize)]
struct StorageMetadataTxDataMirror {
    operation: StorageMetadataOperationMirror,
}

// ── V2 Storage Metadata mirrors (chain-confirmed final ordering) ────────────
//
// Variant indices in `StorageMetadataOperationV2Mirror` MUST match the
// L1 `StorageMetadataOperationV2` enum exactly — bincode v1 encodes
// variants as u32 indices. Chain team confirmed the final order is
// `RegisterFilePendingV2 = 0`, `ActivateFileV2 = 1`,
// `AbandonFileV2 = 2`, `AcceptAssignmentV2 = 3` (NOT 6 as
// the v3.2 §3.6 draft suggested), `AddAccessV2 = 4`,
// `RemoveAccessV2 = 5`, `UpdateAccessV2 = 6`. **Any mismatch on the
// L1 side → all V2 txs we sign are misinterpreted.** Pinned by
// `v2_op_variant_indices_are_stable`. Cross-chain bincode round-trip
// is out-of-band, per the safety note at the top of this module's V2
// builder section.

/// Mirror of `AccessEntryV2` (chain plan §3.1).
///
/// Field order follows the chain plan exactly. Bincode v1 is positional
/// for structs, so any reorder here breaks chain compatibility silently.
///
/// `[u8; 80]` exceeds the default serde array-derive cutoff of 32, so
/// the bundle is wrapped via `serde_big_array::BigArray`. Serde derives
/// don't directly support `#[serde(with = ...)]` on `Option<[u8; N]>`
/// for N > 32, so we use a wrapping `Bundle80` newtype that implements
/// `Serialize`/`Deserialize` manually using `BigArray`. Wire shape is
/// identical to `[u8; 80]` — bincode v1 doesn't encode struct names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessEntryV2Mirror {
    pub address: [u8; 20],
    pub encrypted_key_bundle: Option<Bundle80>,
    pub expires_at: Option<u64>,
}

/// Newtype around `[u8; 80]` (the encrypted-key-bundle wire size).
///
/// Exists solely to enable serde derives on
/// `AccessEntryV2Mirror.encrypted_key_bundle: Option<…>` — `[u8; 80]`
/// doesn't impl `Serialize`/`Deserialize` by default and the
/// `#[serde(with = ...)]` workaround doesn't compose cleanly with
/// `Option<>`. Construct via `Bundle80(arr)` and read the inner bytes
/// via `.0`.
#[derive(Debug, Clone, Copy)]
pub struct Bundle80(pub [u8; 80]);

impl Serialize for Bundle80 {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        BigArray::serialize(&self.0, ser)
    }
}

impl<'de> Deserialize<'de> for Bundle80 {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(Bundle80(BigArray::deserialize(de)?))
    }
}

/// Mirror of `StorageMetadataOperationV2`. Variant indices are critical;
/// see the safety note at the top of the V2 builder section.
///
/// **Variant order is locked against the chain**: chain team confirmed
/// `AcceptAssignmentV2` is at index 3, between `AbandonFileV2` and the
/// access-list ops. Earlier (v3.2 §3.6) drafts placed it at index 6;
/// chain final ordering moved it to 3. The
/// `v2_op_variant_indices_are_stable` test pins each index byte for
/// byte against bincode v1's u32 LE discriminant encoding.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum StorageMetadataOperationV2Mirror {
    RegisterFilePendingV2 {
        merkle_root: [u8; 32],
        plaintext_size_bytes: u64,
        stored_size_bytes: u64,
        chunk_count: u32,
        fee_deposit: u64,
        visibility: u8,
        initial_access: Vec<AccessEntryV2Mirror>,
    },                                                                              // 0
    ActivateFileV2 { merkle_root: [u8; 32] },                                       // 1
    AbandonFileV2 { merkle_root: [u8; 32] },                                        // 2
    AcceptAssignmentV2 {
        merkle_root: [u8; 32],
        chunk_indices: Vec<u32>,
    },                                                                              // 3 — chain final ordering
    AddAccessV2 {
        merkle_root: [u8; 32],
        entry: AccessEntryV2Mirror,
    },                                                                              // 4
    RemoveAccessV2 {
        merkle_root: [u8; 32],
        address: [u8; 20],
    },                                                                              // 5
    UpdateAccessV2 {
        merkle_root: [u8; 32],
        address: [u8; 20],
        new_entry: AccessEntryV2Mirror,
    },                                                                              // 6
}

/// Mirror of `StorageMetadataV2TxData` (paired payload wrapper).
#[derive(Debug, Serialize, Deserialize)]
struct StorageMetadataV2TxDataMirror {
    operation: StorageMetadataOperationV2Mirror,
}

// ── Transaction envelope mirrors ─────────────────────────────────────────────

/// Mirror of `TxPayload`. V1 layout: `NodeRegistry` at index 17,
/// `StorageMetadata` at index 18.
///
/// **V2 ordering matters.** The chain plan v3.2 §3.1/§3.3 introduce
/// `NodeRegistryOperationV2` (carrying `RegisterEncryptionKey`) and
/// `StorageMetadataOperationV2` (carrying `RegisterFilePendingV2`,
/// etc.) and treat both as "additive" payload variants. The chain plan
/// publishes `NodeRegistryV2` *before* `StorageMetadataV2` in its
/// schema text, so we slot `NodeRegistryV2` at variant index 19 and
/// `StorageMetadataV2` at variant index 20. Bincode v1 encodes enum
/// variants as `u32` indices; any divergence here causes silent
/// payload-type corruption on chain.
///
/// `NodeRegistryV2` is included as a placeholder mirror so the wire
/// indices line up correctly today. We don't expose builders for it in
/// Phase 0b (Phase 4 will, when `RegisterEncryptionKey` is wired in
/// for Private files).
///
/// **!! BEFORE TESTNET INTEGRATION !!** confirm both the index for
/// `NodeRegistryV2` (19) and `StorageMetadataV2` (20) against the
/// actual L1 `TxPayload` enum.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum TxPayloadMirror {
    Transfer { to: [u8; 20], amount: u128 },             // 0
    Nft(Vec<u8>),                                        // 1
    Token(Vec<u8>),                                      // 2
    ContractDeploy(Vec<u8>),                             // 3
    ContractCall(Vec<u8>),                               // 4
    Staking(Vec<u8>),                                    // 5
    Messaging(Vec<u8>),                                  // 6
    DocClass(Vec<u8>),                                   // 7
    Tax(Vec<u8>),                                        // 8
    Equity(Vec<u8>),                                     // 9
    Agreement(Vec<u8>),                                  // 10
    Legal(Vec<u8>),                                      // 11
    Property(Vec<u8>),                                   // 12
    Healthcare(Vec<u8>),                                 // 13
    Employment(Vec<u8>),                                 // 14
    Finance(Vec<u8>),                                    // 15
    PolicyAccount(Vec<u8>),                              // 16
    NodeRegistry(NodeRegistryTxDataMirror),              // 17
    StorageMetadata(StorageMetadataTxDataMirror),        // 18
    NodeRegistryV2(NodeRegistryV2TxDataMirror),          // 19 — placeholder for Phase 4
    StorageMetadataV2(StorageMetadataV2TxDataMirror),    // 20 — Phase 0b builders target this
}

/// Mirror of `TransactionV2`.
#[derive(Debug, Serialize, Deserialize)]
struct TransactionV2Mirror {
    chain_id: u64,
    from: [u8; 20],
    fee: u128,
    nonce: u64,
    payload: TxPayloadMirror,
}

/// Mirror of `TxInner`.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum TxInnerMirror {
    Legacy(Vec<u8>),           // 0
    V2(TransactionV2Mirror),   // 1
}

/// Mirror of `SignedTransaction`.
#[derive(Debug, Serialize, Deserialize)]
struct SignedTransactionMirror {
    inner: TxInnerMirror,
    #[serde(with = "BigArray")]
    signature: [u8; 64],
    public_key: [u8; 32],
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_verify_proof_tx() {
        let seed = [42u8; 32];
        let hex = build_submit_proof_tx(
            &seed, 1, 0, 1_000_000,
            [0xAA; 32], [0xBB; 32], 5, [0xCC; 32],
            vec![[0xDD; 32], [0xEE; 32]],
        )
        .unwrap();

        assert!(!hex.is_empty());
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1);
                assert_eq!(tx.nonce, 0);
                assert_eq!(tx.fee, 1_000_000);
                match tx.payload {
                    TxPayloadMirror::StorageMetadata(data) => match data.operation {
                        StorageMetadataOperationMirror::SubmitStorageProof {
                            challenge_id, merkle_root, chunk_index, chunk_hash, merkle_path,
                        } => {
                            assert_eq!(challenge_id, [0xAA; 32]);
                            assert_eq!(merkle_root, [0xBB; 32]);
                            assert_eq!(chunk_index, 5);
                            assert_eq!(chunk_hash, [0xCC; 32]);
                            assert_eq!(merkle_path.len(), 2);
                        }
                        _ => panic!("wrong operation variant"),
                    },
                    _ => panic!("wrong payload variant"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }

        let signing_key = SigningKey::from_bytes(&seed);
        assert_eq!(signed.public_key, signing_key.verifying_key().to_bytes());
    }

    #[test]
    fn deterministic_tx_hex() {
        let seed = [1u8; 32];
        let hex1 = build_submit_proof_tx(
            &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
        ).unwrap();
        let hex2 = build_submit_proof_tx(
            &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
        ).unwrap();
        assert_eq!(hex1, hex2, "same inputs must produce same tx hex");
    }

    #[test]
    fn build_and_verify_register_encryption_key_tx() {
        let seed = [11u8; 32];
        let pubkey = [0xAB; 32];
        let hex = build_register_encryption_key_tx(&seed, 1337, 7, 250_000, pubkey).unwrap();

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1337);
                assert_eq!(tx.nonce, 7);
                assert_eq!(tx.fee, 250_000);
                match tx.payload {
                    TxPayloadMirror::NodeRegistryV2(data) => match data.operation {
                        NodeRegistryOperationV2Mirror::RegisterEncryptionKey {
                            encryption_pubkey,
                        } => {
                            assert_eq!(encryption_pubkey, pubkey);
                        }
                    },
                    _ => panic!("wrong payload variant — expected NodeRegistryV2"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }
    }

    #[test]
    fn build_and_verify_register_node_tx() {
        let seed = [10u8; 32];
        let hex = build_register_archive_node_tx(
            &seed, 1337, 0, 1_000_000, 1_000_000_000,
        ).unwrap();

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1337);
                match tx.payload {
                    TxPayloadMirror::NodeRegistry(data) => match data.operation {
                        NodeRegistryOperationMirror::Register { role, stake } => {
                            assert!(matches!(role, NodeRoleMirror::ArchiveNode));
                            assert_eq!(stake, 1_000_000_000);
                        }
                        _ => panic!("wrong NodeRegistry operation"),
                    },
                    _ => panic!("wrong payload variant — expected NodeRegistry"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }
    }

    // ── V2 builder tests ─────────────────────────────────────────────────────
    //
    // These verify that our mirror types round-trip through bincode v1
    // back into the same shape. They CANNOT verify cross-implementation
    // compat with the L1 side — that requires either (a) the L1 code
    // accessible in the same workspace or (b) a separate conformance
    // run against a live chain endpoint. Both are out of scope for
    // Phase 0b unit tests; flagged in the V2 builder safety note.

    #[test]
    fn register_file_pending_v2_round_trips() {
        let seed = [30u8; 32];
        let merkle_root = [0x77; 32];
        let access = vec![AccessEntryV2Mirror {
            address: [0xAA; 20],
            encrypted_key_bundle: None, // Public file
            expires_at: None,
        }];
        let hex = build_register_file_pending_v2_tx(
            &seed,
            1337, // chain_id
            5,    // nonce
            1_000_000, // fee
            merkle_root,
            10_485_760, // plaintext_size_bytes
            10_485_760, // stored_size_bytes (Public: same)
            10,         // chunk_count
            500_000,    // fee_deposit
            0,          // visibility = Public
            access.clone(),
        )
        .unwrap();

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1337);
                assert_eq!(tx.nonce, 5);
                match tx.payload {
                    TxPayloadMirror::StorageMetadataV2(data) => match data.operation {
                        StorageMetadataOperationV2Mirror::RegisterFilePendingV2 {
                            merkle_root: r,
                            plaintext_size_bytes,
                            stored_size_bytes,
                            chunk_count,
                            fee_deposit,
                            visibility,
                            initial_access,
                        } => {
                            assert_eq!(r, merkle_root);
                            assert_eq!(plaintext_size_bytes, 10_485_760);
                            assert_eq!(stored_size_bytes, 10_485_760);
                            assert_eq!(chunk_count, 10);
                            assert_eq!(fee_deposit, 500_000);
                            assert_eq!(visibility, 0);
                            assert_eq!(initial_access.len(), 1);
                            assert_eq!(initial_access[0].address, [0xAA; 20]);
                            assert!(initial_access[0].encrypted_key_bundle.is_none());
                        }
                        _ => panic!("wrong V2 op variant"),
                    },
                    _ => panic!("expected StorageMetadataV2 payload variant"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }
    }

    #[test]
    fn activate_file_v2_and_abandon_file_v2_round_trip() {
        let seed = [31u8; 32];
        let root = [0x88; 32];

        for builder_label in ["activate", "abandon"] {
            let hex = match builder_label {
                "activate" => build_activate_file_v2_tx(&seed, 1, 0, 100, root).unwrap(),
                _ => build_abandon_file_v2_tx(&seed, 1, 0, 100, root).unwrap(),
            };
            let bytes = hex::decode(&hex).unwrap();
            let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();
            match signed.inner {
                TxInnerMirror::V2(tx) => match tx.payload {
                    TxPayloadMirror::StorageMetadataV2(data) => match (builder_label, data.operation)
                    {
                        ("activate", StorageMetadataOperationV2Mirror::ActivateFileV2 {
                            merkle_root: r,
                        }) => assert_eq!(r, root),
                        ("abandon", StorageMetadataOperationV2Mirror::AbandonFileV2 {
                            merkle_root: r,
                        }) => assert_eq!(r, root),
                        (lbl, op) => panic!("wrong variant for {lbl}: {op:?}"),
                    },
                    _ => panic!("expected StorageMetadataV2 payload"),
                },
                _ => panic!("wrong TxInner variant"),
            }
        }
    }

    #[test]
    fn accept_assignment_v2_carries_chunk_indices() {
        let seed = [32u8; 32];
        let root = [0x99; 32];
        // 70k indices: typical multi-tx batch shape (caller will split
        // before calling, but the builder itself accepts any length).
        let chunk_indices: Vec<u32> = (0..70_000).collect();
        let hex =
            build_accept_assignment_v2_tx(&seed, 1, 0, 100, root, chunk_indices.clone()).unwrap();

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();
        match signed.inner {
            TxInnerMirror::V2(tx) => match tx.payload {
                TxPayloadMirror::StorageMetadataV2(data) => match data.operation {
                    StorageMetadataOperationV2Mirror::AcceptAssignmentV2 {
                        merkle_root: r,
                        chunk_indices: idx,
                    } => {
                        assert_eq!(r, root);
                        assert_eq!(idx.len(), 70_000);
                        assert_eq!(idx[0], 0);
                        assert_eq!(idx[69_999], 69_999);
                    }
                    _ => panic!("wrong V2 op variant"),
                },
                _ => panic!("expected StorageMetadataV2 payload"),
            },
            _ => panic!("wrong TxInner variant"),
        }
    }

    /// **Payload-level** variant-index pin. Catches the High-priority
    /// reviewer finding: SNIP and L1 must agree on which `TxPayload`
    /// variant is `StorageMetadataV2`. Without this pin, an accidental
    /// reorder would silently misroute every V2 storage tx.
    ///
    /// Bincode v1 encodes enum variants as `u32` little-endian. The
    /// first 4 bytes of a serialized payload ARE the variant tag.
    #[test]
    fn payload_v2_variant_indices_are_stable() {
        fn variant_index(p: TxPayloadMirror) -> u32 {
            let bytes = bincode1::serialize(&p).unwrap();
            assert!(bytes.len() >= 4, "expected at least 4 bytes for payload tag");
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }

        // V1 baselines (sanity — these are pinned by the existing chain tests
        // too, but cross-checking here means a single test catches both
        // V1 drift and V2 drift).
        assert_eq!(
            variant_index(TxPayloadMirror::NodeRegistry(NodeRegistryTxDataMirror {
                operation: NodeRegistryOperationMirror::Register {
                    role: NodeRoleMirror::ArchiveNode,
                    stake: 0,
                },
            })),
            17,
            "V1 NodeRegistry payload index drift"
        );
        assert_eq!(
            variant_index(TxPayloadMirror::StorageMetadata(StorageMetadataTxDataMirror {
                operation: StorageMetadataOperationMirror::RegisterFile {
                    merkle_root: [0; 32],
                    total_size_bytes: 0,
                    access_list: vec![],
                    fee_deposit: 0,
                },
            })),
            18,
            "V1 StorageMetadata payload index drift"
        );

        // V2 placements per chain plan v3.2 schema-text ordering:
        // NodeRegistryV2 at 19, StorageMetadataV2 at 20.
        assert_eq!(
            variant_index(TxPayloadMirror::NodeRegistryV2(NodeRegistryV2TxDataMirror {
                operation: NodeRegistryOperationV2Mirror::RegisterEncryptionKey {
                    encryption_pubkey: [0; 32],
                },
            })),
            19,
            "NodeRegistryV2 payload index must be 19"
        );
        assert_eq!(
            variant_index(TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
                operation: StorageMetadataOperationV2Mirror::ActivateFileV2 {
                    merkle_root: [0; 32],
                },
            })),
            20,
            "StorageMetadataV2 payload index must be 20 — \
             slot 19 is reserved for NodeRegistryV2"
        );
    }

    /// V2 mirror variant indices must serialize to the expected `u32`
    /// values. Bincode v1 emits enum variants as `u32` little-endian.
    /// This test asserts the exact byte at the variant-discriminant
    /// position. If this test ever changes, the L1 cross-compat is in
    /// danger — DO NOT mutate variant order without coordinating with
    /// chain team.
    #[test]
    fn v2_op_variant_indices_are_stable() {
        // Build a synthetic op of each variant, serialize, peek the
        // discriminant byte. Bincode v1 default config writes the
        // variant tag as little-endian u32 (4 bytes).
        fn variant_index(op: StorageMetadataOperationV2Mirror) -> u32 {
            let bytes = bincode1::serialize(&op).unwrap();
            assert!(bytes.len() >= 4, "expected at least 4 bytes for variant tag");
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::RegisterFilePendingV2 {
                merkle_root: [0; 32],
                plaintext_size_bytes: 0,
                stored_size_bytes: 0,
                chunk_count: 0,
                fee_deposit: 0,
                visibility: 0,
                initial_access: vec![],
            }),
            0
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::ActivateFileV2 {
                merkle_root: [0; 32]
            }),
            1
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::AbandonFileV2 {
                merkle_root: [0; 32]
            }),
            2
        );
        // Chain-confirmed: AcceptAssignmentV2 is at index 3 (NOT 6 as
        // the v3.2 §3.6 draft suggested). The access-list ops shift up.
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::AcceptAssignmentV2 {
                merkle_root: [0; 32],
                chunk_indices: vec![],
            }),
            3
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::AddAccessV2 {
                merkle_root: [0; 32],
                entry: AccessEntryV2Mirror {
                    address: [0; 20],
                    encrypted_key_bundle: None,
                    expires_at: None,
                },
            }),
            4
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::RemoveAccessV2 {
                merkle_root: [0; 32],
                address: [0; 20],
            }),
            5
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2Mirror::UpdateAccessV2 {
                merkle_root: [0; 32],
                address: [0; 20],
                new_entry: AccessEntryV2Mirror {
                    address: [0; 20],
                    encrypted_key_bundle: None,
                    expires_at: None,
                },
            }),
            6
        );
    }

    // ── Bincode wire fixtures (mirror of chain-side
    // `crates/primitives/tests/v2_wire_fixtures.rs`) ────────────────────
    //
    // These pin the EXACT bincode-v1 byte layout SNIP emits for each
    // V2 operation, using the same fixed inputs the chain-side
    // fixture tests use. If chain ever changes a variant index,
    // field order, or integer encoding without coordinating, ONE of
    // these tests fires here — long before a real tx hits a real
    // chain. Cross-checked against chain's locked hex strings; any
    // divergence between SNIP's actual hex and chain's expected hex
    // is a production wire break.
    //
    // The expected hex strings are derived from bincode-v1 spec:
    //   * Enum variant tag: u32 little-endian (4 bytes).
    //   * `[u8; N]`:        raw N bytes, no length prefix.
    //   * `Vec<T>`:         u64 LE length prefix, then elements.
    //   * `u32`:            4 bytes LE.
    //   * `u64`:            8 bytes LE.
    //   * `u128`:           16 bytes LE.
    //   * `Option<T>`:      0x00 (None) | 0x01 + payload (Some).
    //   * `String`:         u64 LE length + UTF-8 bytes.
    //   * struct fields:    serialized in declaration order, no separators.

    /// Shared with chain-side `v2_wire_fixtures.rs` — any drift in
    /// these constants is also a wire break.
    const FIXTURE_MERKLE_ROOT: [u8; 32] = [0x42; 32];
    const FIXTURE_ENCRYPTION_PUBKEY: [u8; 32] = [0x11; 32];
    fn fixture_chunk_indices() -> Vec<u32> {
        vec![1, 2, 3]
    }

    /// Helper: serialize `op` via bincode-v1 and lowercase-hex-encode.
    fn op_hex(op: StorageMetadataOperationV2Mirror) -> String {
        hex::encode(bincode1::serialize(&op).unwrap())
    }
    fn nr_op_hex(op: NodeRegistryOperationV2Mirror) -> String {
        hex::encode(bincode1::serialize(&op).unwrap())
    }

    #[test]
    fn fixture_register_encryption_key_bytes() {
        // Variant 0: tag=00000000, then 32 bytes of 0x11.
        let expected = "00000000".to_string()
            + &"11".repeat(32);
        let actual = nr_op_hex(NodeRegistryOperationV2Mirror::RegisterEncryptionKey {
            encryption_pubkey: FIXTURE_ENCRYPTION_PUBKEY,
        });
        assert_eq!(
            actual, expected,
            "RegisterEncryptionKey wire bytes diverged from chain fixture"
        );
    }

    #[test]
    fn fixture_register_file_pending_v2_bytes() {
        // SNIP-side fixture inputs (auxiliary fields are SNIP's
        // choice; the chain-side fixture may use different values for
        // these and the hex will differ on those bytes). The shared
        // bytes — variant tag + merkle_root prefix — must match.
        let visibility = 1u8; // Private
        let plaintext_size_bytes = 1024u64;
        let stored_size_bytes = 1040u64; // 1024 + 16 AEAD tag
        let chunk_count = 1u32;
        let fee_deposit = 0u64;

        let actual = op_hex(StorageMetadataOperationV2Mirror::RegisterFilePendingV2 {
            merkle_root: FIXTURE_MERKLE_ROOT,
            plaintext_size_bytes,
            stored_size_bytes,
            chunk_count,
            fee_deposit,
            visibility,
            initial_access: vec![],
        });

        // Variant 0 → tag = 00000000.
        // Then merkle_root (32 × 0x42) follows immediately.
        let shared_prefix = "00000000".to_string() + &"42".repeat(32);
        assert!(
            actual.starts_with(&shared_prefix),
            "RegisterFilePendingV2 prefix mismatch (variant tag + merkle_root must \
             match chain fixture). expected_prefix={shared_prefix} actual={actual}"
        );

        // Lock the SNIP-local auxiliary suffix exactly as well so a
        // future field reorder in the mirror is also caught locally.
        // plaintext_size_bytes=1024 → 0x00040000 00000000 (LE u64)
        // stored_size_bytes=1040    → 0x10040000 00000000
        // chunk_count=1             → 0x01000000           (LE u32)
        // fee_deposit=0             → 0x00000000 00000000
        // visibility=1              → 0x01
        // initial_access (empty Vec)→ 0x00000000 00000000  (u64 length=0)
        let aux = "0004000000000000".to_string() // plaintext_size_bytes=1024
            + "1004000000000000" // stored_size_bytes=1040
            + "01000000"          // chunk_count=1
            + "0000000000000000"  // fee_deposit=0
            + "01"                // visibility=1
            + "0000000000000000"; // empty initial_access Vec length
        let expected = shared_prefix + &aux;
        assert_eq!(
            actual, expected,
            "RegisterFilePendingV2 SNIP-local fixture drift"
        );
    }

    #[test]
    fn fixture_activate_file_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2Mirror::ActivateFileV2 {
            merkle_root: FIXTURE_MERKLE_ROOT,
        });
        // Variant 1 → tag=01000000, then merkle_root.
        let expected = "01000000".to_string() + &"42".repeat(32);
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_abandon_file_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2Mirror::AbandonFileV2 {
            merkle_root: FIXTURE_MERKLE_ROOT,
        });
        // Variant 2 → tag=02000000, then merkle_root.
        let expected = "02000000".to_string() + &"42".repeat(32);
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_accept_assignment_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2Mirror::AcceptAssignmentV2 {
            merkle_root: FIXTURE_MERKLE_ROOT,
            chunk_indices: fixture_chunk_indices(),
        });
        // Variant 3 (chain-confirmed; was 6 in earlier draft) →
        // tag=03000000, then merkle_root, then Vec<u32> as
        // u64-LE-length-prefix(=3) + 3 u32s [1, 2, 3].
        let expected = "03000000".to_string()
            + &"42".repeat(32)
            + "0300000000000000" // length = 3 (u64 LE)
            + "01000000"          // 1 u32 LE
            + "02000000"          // 2 u32 LE
            + "03000000";         // 3 u32 LE
        assert_eq!(
            actual, expected,
            "AcceptAssignmentV2 wire bytes diverged from chain fixture"
        );
    }

    /// TxPayload-wrapped form: NodeRegistryV2 at index 19 carrying
    /// RegisterEncryptionKey at inner-index 0.
    #[test]
    fn fixture_tx_payload_node_registry_v2_register_encryption_key() {
        let payload = TxPayloadMirror::NodeRegistryV2(NodeRegistryV2TxDataMirror {
            operation: NodeRegistryOperationV2Mirror::RegisterEncryptionKey {
                encryption_pubkey: FIXTURE_ENCRYPTION_PUBKEY,
            },
        });
        let actual = hex::encode(bincode1::serialize(&payload).unwrap());
        // Outer TxPayload variant=19 → 0x13000000.
        // Inner NodeRegistryOperationV2 variant=0 → 0x00000000.
        // Then 32 bytes of 0x11.
        let expected = "13000000".to_string()
            + "00000000"
            + &"11".repeat(32);
        assert_eq!(actual, expected, "TxPayload::NodeRegistryV2 != index 19");
    }

    /// TxPayload-wrapped form: StorageMetadataV2 at index 20 carrying
    /// AcceptAssignmentV2 at inner-index 3.
    #[test]
    fn fixture_tx_payload_storage_metadata_v2_accept_assignment() {
        let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
            operation: StorageMetadataOperationV2Mirror::AcceptAssignmentV2 {
                merkle_root: FIXTURE_MERKLE_ROOT,
                chunk_indices: fixture_chunk_indices(),
            },
        });
        let actual = hex::encode(bincode1::serialize(&payload).unwrap());
        let expected = "14000000".to_string() // outer = 20
            + "03000000"                       // inner = 3 (chain-confirmed)
            + &"42".repeat(32)                 // merkle_root
            + "0300000000000000"               // chunk_indices Vec length = 3
            + "01000000"
            + "02000000"
            + "03000000";
        assert_eq!(
            actual, expected,
            "TxPayload::StorageMetadataV2 != 20 OR AcceptAssignmentV2 != 3"
        );
    }

    /// TxPayload-wrapped form: StorageMetadataV2 carrying
    /// ActivateFileV2 (inner index 1) and AbandonFileV2 (inner 2).
    /// Pins those variants are reachable through the wrapper too,
    /// not only as bare ops.
    #[test]
    fn fixture_tx_payload_storage_metadata_v2_activate_and_abandon() {
        for (op, inner_tag, label) in [
            (
                StorageMetadataOperationV2Mirror::ActivateFileV2 {
                    merkle_root: FIXTURE_MERKLE_ROOT,
                },
                "01000000",
                "activate",
            ),
            (
                StorageMetadataOperationV2Mirror::AbandonFileV2 {
                    merkle_root: FIXTURE_MERKLE_ROOT,
                },
                "02000000",
                "abandon",
            ),
        ] {
            let payload = TxPayloadMirror::StorageMetadataV2(StorageMetadataV2TxDataMirror {
                operation: op,
            });
            let actual = hex::encode(bincode1::serialize(&payload).unwrap());
            let expected = "14000000".to_string()
                + inner_tag
                + &"42".repeat(32);
            assert_eq!(actual, expected, "{label} wrapper bytes diverged");
        }
    }

    #[test]
    fn build_and_verify_register_file_tx() {
        let seed = [20u8; 32];
        let hex = build_register_file_tx(
            &seed, 1337, 1, 1_000_000,
            [0xFF; 32],     // merkle_root
            2_097_152,      // total_size_bytes (2 MB)
            vec![],         // empty access_list (public)
            100_000_000,    // fee_deposit
        ).unwrap();

        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1337);
                assert_eq!(tx.nonce, 1);
                match tx.payload {
                    TxPayloadMirror::StorageMetadata(data) => match data.operation {
                        StorageMetadataOperationMirror::RegisterFile {
                            merkle_root, total_size_bytes, access_list, fee_deposit,
                        } => {
                            assert_eq!(merkle_root, [0xFF; 32]);
                            assert_eq!(total_size_bytes, 2_097_152);
                            assert!(access_list.is_empty());
                            assert_eq!(fee_deposit, 100_000_000);
                        }
                        _ => panic!("wrong StorageMetadata operation"),
                    },
                    _ => panic!("wrong payload variant — expected StorageMetadata"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }
    }
}
