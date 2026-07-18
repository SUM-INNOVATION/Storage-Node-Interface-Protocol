//! Transaction builder for submitting storage operations to the SUM Chain L1.
//!
//! Constructs `SignedTransaction` bytes that the L1 can deserialize via
//! `SignedTransaction::from_hex()` (bincode v1).
//!
//! The transaction types are the byte-frozen production wire types from the
//! `sumchain-wire 0.1.1` crate (`SignedTransaction`, `TxInner`,
//! `TransactionV2`, `TxPayload`, and the `node_registry` / `storage_metadata`
//! operation enums). There are no locally-maintained mirror types: the wire
//! crate is the single source of truth for variant ordering, field order, and
//! bincode-v1 layout, so there is nothing here to drift against the chain.

use anyhow::Result;
use ed25519_dalek::{Signer, SigningKey};
use sumchain_wire::{
    Address, Hash,
    node_registry::{
        NodeRegistryOperation, NodeRegistryOperationV2, NodeRegistryTxData, NodeRegistryV2TxData,
        NodeRole,
    },
    storage_metadata::{
        AccessEntryV2, StorageMetadataOperation, StorageMetadataOperationV2, StorageMetadataTxData,
        StorageMetadataV2TxData,
    },
    transaction::{SignedTransaction, TransactionV2, TxPayload},
};

// ── Compatibility surface for callers ─────────────────────────────────────────

/// Public alias for the wire encrypted-key-bundle type
/// (`sumchain_wire::storage_metadata::EncryptedKeyBundleV2`).
///
/// Retained under the historical name `Bundle80` so callers keep constructing
/// `Bundle80(bytes)` and reading `.0` unchanged. The 80-byte length invariant
/// is enforced by the type (`EncryptedKeyBundleV2(pub [u8; 80])`); there is no
/// second serializer here — the wire type owns the (BigArray) encoding.
pub use sumchain_wire::storage_metadata::EncryptedKeyBundleV2 as Bundle80;

/// Compatibility-only INPUT adapter for `sumchain_wire::…::AccessEntryV2`.
///
/// This is **not** a wire representation and intentionally does **not**
/// implement `Serialize`/`Deserialize`: it exists solely so existing callers
/// can keep building access entries from raw `[u8; 20]` addresses and
/// `Option<Bundle80>` bundles. Every builder converts it into the production
/// `AccessEntryV2` (via [`AccessEntryV2Mirror::into_wire`]) *before* any
/// payload construction, so the compatibility boundary never touches the wire.
#[derive(Debug, Clone)]
pub struct AccessEntryV2Mirror {
    pub address: [u8; 20],
    pub encrypted_key_bundle: Option<Bundle80>,
    pub expires_at: Option<u64>,
}

impl AccessEntryV2Mirror {
    /// Convert this compatibility adapter into the production wire type.
    /// The `encrypted_key_bundle` field is already the wire bundle type
    /// (`Bundle80` == `EncryptedKeyBundleV2`), so it moves across verbatim.
    fn into_wire(self) -> AccessEntryV2 {
        AccessEntryV2 {
            address: Address::new(self.address),
            encrypted_key_bundle: self.encrypted_key_bundle,
            expires_at: self.expires_at,
        }
    }
}

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
    let payload = TxPayload::StorageMetadata(StorageMetadataTxData {
        operation: StorageMetadataOperation::SubmitStorageProof {
            challenge_id: Hash::new(challenge_id),
            merkle_root: Hash::new(merkle_root),
            chunk_index,
            chunk_hash: Hash::new(chunk_hash),
            merkle_path: merkle_path.into_iter().map(Hash::new).collect(),
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
    let payload = TxPayload::NodeRegistryV2(NodeRegistryV2TxData {
        operation: NodeRegistryOperationV2::RegisterEncryptionKey { encryption_pubkey },
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
    let payload = TxPayload::NodeRegistry(NodeRegistryTxData {
        operation: NodeRegistryOperation::Register {
            role: NodeRole::ArchiveNode,
            stake,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

// ── V2 Builders (chain plan v3.2) ─────────────────────────────────────────────
//
// These target the `sumchain-wire 0.1.1` production `StorageMetadataOperationV2`
// / `NodeRegistryOperationV2` enums, so their variant ordering and field layout
// are guaranteed by the wire crate's own byte-frozen fixtures — there is no
// SNIP-local mirror to keep in sync with the chain.

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
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::RegisterFilePendingV2 {
            merkle_root: Hash::new(merkle_root),
            plaintext_size_bytes,
            stored_size_bytes,
            chunk_count,
            fee_deposit,
            visibility,
            initial_access: initial_access
                .into_iter()
                .map(AccessEntryV2Mirror::into_wire)
                .collect(),
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
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::ActivateFileV2 {
            merkle_root: Hash::new(merkle_root),
        },
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
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::AbandonFileV2 {
            merkle_root: Hash::new(merkle_root),
        },
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
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::AcceptAssignmentV2 {
            merkle_root: Hash::new(merkle_root),
            chunk_indices,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `AddAccessV2`
/// (chain plan §3.1 access-list mutation; Phase 4c).
///
/// The chain enforces that only the file's owner can submit access
/// mutations. Caller is responsible for that pre-check (in
/// `access::run_share`); this builder only signs+encodes the tx.
///
/// `entry.encrypted_key_bundle` carries the recipient's wrapped
/// `K_file` (Phase 4a `wrap_for_recipient`) so the recipient can
/// later decrypt the file's chunks. The chain stores the bundle
/// verbatim — it never sees `K_file`.
pub fn build_add_access_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    entry: AccessEntryV2Mirror,
) -> Result<String> {
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::AddAccessV2 {
            merkle_root: Hash::new(merkle_root),
            entry: entry.into_wire(),
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `RemoveAccessV2`
/// (chain plan §3.1; Phase 4c).
///
/// Removing the address takes effect at chain finalization: the
/// chain-side ACL gate immediately denies that address on chunk and
/// manifest pulls. The recipient still holds their old
/// `encrypted_key_bundle` locally — Phase 4c does NOT rotate `K_file`
/// (see the operator-message warning in `run_revoke` for forward-
/// secrecy implications; chain-level revocation is sufficient for
/// Phase 4c's threat model).
pub fn build_remove_access_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    address: [u8; 20],
) -> Result<String> {
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::RemoveAccessV2 {
            merkle_root: Hash::new(merkle_root),
            address: Address::new(address),
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

/// Build a hex-encoded `SignedTransaction` for `UpdateAccessV2`
/// (chain plan §3.1; Phase 4c).
///
/// First-cut Phase 4c uses this only to update `expires_at` (set or
/// clear). Callers MUST preserve the existing `encrypted_key_bundle`
/// on the `new_entry` — passing a bundle wrapped under a different
/// `K_file` would silently break the recipient's downloads. Future
/// flavors (key rotation: re-wrap `K_file` for the same recipient
/// because they registered a new X25519 key) are deferred to
/// Phase 5+; for now the operator does revoke→share.
pub fn build_update_access_v2_tx(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    merkle_root: [u8; 32],
    address: [u8; 20],
    new_entry: AccessEntryV2Mirror,
) -> Result<String> {
    let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::UpdateAccessV2 {
            merkle_root: Hash::new(merkle_root),
            address: Address::new(address),
            new_entry: new_entry.into_wire(),
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
    let payload = TxPayload::StorageMetadata(StorageMetadataTxData {
        operation: StorageMetadataOperation::RegisterFile {
            merkle_root: Hash::new(merkle_root),
            total_size_bytes,
            access_list: access_list.into_iter().map(Address::new).collect(),
            fee_deposit,
        },
    });
    sign_and_encode(ed25519_seed, chain_id, nonce, fee, payload)
}

// ── Shared Signing Logic ──────────────────────────────────────────────────────

/// Assemble a `TransactionV2` for `payload`, sign it, and return the
/// hex-encoded production `SignedTransaction`. Every builder funnels through
/// here, so every builder emits exactly `TxInner::V2` (there is no `Legacy`
/// construction path anywhere in this module).
fn sign_and_encode(
    ed25519_seed: &[u8; 32],
    chain_id: u64,
    nonce: u64,
    fee: u128,
    payload: TxPayload,
) -> Result<String> {
    let signing_key = SigningKey::from_bytes(ed25519_seed);
    let pubkey_bytes: [u8; 32] = signing_key.verifying_key().to_bytes();

    // L1 address derivation: blake3(pubkey)[12..32], via the wire helper.
    let from = Address::from_public_key(&pubkey_bytes);

    let tx = TransactionV2 {
        chain_id,
        from,
        fee,
        nonce,
        payload,
    };

    // Signing hash: blake3(bincode-v1(TransactionV2)). `TransactionV2::signing_hash`
    // performs exactly that.
    let signing_hash = tx.signing_hash();

    // Sign with Ed25519 over the 32-byte signing hash.
    let signature = signing_key.sign(signing_hash.as_bytes());

    let signed = SignedTransaction::new_v2(tx, signature.to_bytes(), pubkey_bytes);
    Ok(signed.to_hex())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, VerifyingKey};
    use sumchain_wire::transaction::TxInner;

    // ── Fixed TEST_ONLY inputs for the full-signed byte locks ────────────────
    //
    // These reproduce, byte-for-byte, the inputs used to capture the baseline
    // hex constants below from the PRE-CHANGE private-mirror builders. The
    // expected side of every byte-lock assertion is the hardcoded hex captured
    // from that old mirror implementation — NOT a fresh call into the wire
    // implementation — so the check is non-circular.
    const LOCK_SEED: [u8; 32] = [7u8; 32];
    const LOCK_CHAIN_ID: u64 = 1337;
    const LOCK_NONCE: u64 = 3;
    const LOCK_FEE: u128 = 500_000;
    const LOCK_MERKLE_ROOT: [u8; 32] = [0x22; 32];
    const LOCK_ADDR: [u8; 20] = [0x55; 20];
    const LOCK_BUNDLE: [u8; 80] = [0xCC; 80];

    // Baseline full signed hex, captured from the pre-change mirror path.
    const BL_SUBMIT_PROOF: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a1070000000000000000000000000003000000000000001200000005000000111111111111111111111111111111111111111111111111111111111111111122222222222222222222222222222222222222222222222222222222222222220900000033333333333333333333333333333333333333333333333333333333333333330200000000000000444444444444444444444444444444444444444444444444444444444444444455555555555555555555555555555555555555555555555555555555555555555de00e7199f6bf5e26574dba37958e8bece77cc891b7f666fe2b11b7d0f0040e9be5290de75de493eb380918f4624c632a98aeb82da5bd711749bccdf5bc9c02ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_REGISTER_ENC_KEY: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a1070000000000000000000000000003000000000000001300000000000000ababababababababababababababababababababababababababababababababa0c35a617c1a32313055e1a1af23a3d2ceb315f970f3fc58d3e69c1e10c45f83d57bb3d738e4daa53cedeec9cfdb64691662324fe6e4e81b4f18fb341506c00eea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_REGISTER_ARCHIVE: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a10700000000000000000000000000030000000000000011000000000000000100000000ca9a3b000000009e0078eae15f8c48e95a7ce8b60207f7b56db7fc0be93e31607c4a24c7dab7d7a3ad8b20aacb1dc81162ae7e0865f3a4229aa1af7c706c8b33a8894d74fad90cea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_REGISTER_FILE_PENDING_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a10700000000000000000000000000030000000000000014000000000000002222222222222222222222222222222222222222222222222222222222222222000400000000000010040000000000000200000020a1070000000000010100000000000000555555555555555555555555555555555555555501cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01e8030000000000000fe01ee71dbd3527964b0b63fa9f90e448728d43010e80eaf3cff5e8c6591e309348b896f164904a710fd91f6ce79a01a05a786f7fbf87d72e80c12d29d97808ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_ACTIVATE_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a107000000000000000000000000000300000000000000140000000100000022222222222222222222222222222222222222222222222222222222222222222baa39dee06c6245c097390c0d90a281b404609cb0685f8e734fc5c731fe31e029605647fbb847ef1bb8ca5331c3f5946013585e1db9c093c17ed4fead33d403ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_ABANDON_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a1070000000000000000000000000003000000000000001400000002000000222222222222222222222222222222222222222222222222222222222222222254f422e29e42a059136fec8f3fc8c2754b9ae6154cb33fcdd34a878b51a7323bff50b061ee15c029c6754708563fcc7e56921c4c0610815947e75c71e741e50aea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_ACCEPT_ASSIGNMENT_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a107000000000000000000000000000300000000000000140000000300000022222222222222222222222222222222222222222222222222222222222222220300000000000000010000000200000003000000856762f1cd10694ea3859186a042e46fd2aa8666f889b32c7b8dd3662755ceed98c35960a65d31f5da92a9e15c5abddce92c8dcb8e124706f36b703a2160b50cea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_ADD_ACCESS_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a10700000000000000000000000000030000000000000014000000040000002222222222222222222222222222222222222222222222222222222222222222555555555555555555555555555555555555555501cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01e8030000000000001184de5eab1daa78e532b0c8aeeb4d6e0d9ac06e156cb73a29b1210dc7adda52ef461e0d9e87cb9eb5effeb6e6873feb71cf7cc3c38666ffd70f375a1fd9bf09ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_REMOVE_ACCESS_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a1070000000000000000000000000003000000000000001400000005000000222222222222222222222222222222222222222222222222222222222222222255555555555555555555555555555555555555556f590955d94b868c80cec8a74bd109b7d86e2d7d3cf39fb0c71556e6f9cbf7f89163f0c6c07b7d505d96890a2d35f788ce81e442fa2109d06c44c05c78eb3e09ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_UPDATE_ACCESS_V2: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a107000000000000000000000000000300000000000000140000000600000022222222222222222222222222222222222222222222222222222222222222225555555555555555555555555555555555555555555555555555555555555555555555555555555501cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc00dee804e6381a2fe00a6b85d1191930e5ca60da834ced5c42c3bfb3430108ca68179a30ee3acbf1e2bf85b6b0ad7773f2c524bb5433861f220bcc08657ba65205ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const BL_REGISTER_FILE: &str = "010000003905000000000000c03884e6a96f0989d1dd8cfb49cd17ed2579243320a10700000000000000000000000000030000000000000012000000000000006666666666666666666666666666666666666666666666666666666666666666000020000000000002000000000000007777777777777777777777777777777777777777888888888888888888888888888888888888888800e1f50500000000dcaf1fe7b58801d36849f0e6086ff1808f732eceb261e10eb312f72504848b827f738e0f0e99fef672db3a277e475cac32d3fbcb069255f07ab47bde6a17410aea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";

    // ── Shared test helpers ──────────────────────────────────────────────────

    /// Decode our own output back through the `sumchain-wire 0.1.1` decoder.
    fn decode(hex_str: &str) -> SignedTransaction {
        SignedTransaction::from_hex(hex_str).expect("wire decoder must accept our own output")
    }

    /// Extract the inner V2 transaction, asserting the builder emitted V2
    /// (never `Legacy`).
    fn v2(signed: &SignedTransaction) -> &TransactionV2 {
        match &signed.inner {
            TxInner::V2(tx) => tx,
            TxInner::Legacy(_) => panic!("builder must emit TxInner::V2, got Legacy"),
        }
    }

    fn expected_from(seed: &[u8; 32]) -> Address {
        Address::from_public_key(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
    }

    /// Locally recompute the signing hash (blake3 of the `TransactionV2`
    /// bytes) and cryptographically verify the ed25519 signature over it.
    /// The wire decoder does NOT verify signatures, so this is an explicit
    /// ed25519 check, not a side effect of deserialization.
    fn assert_cryptographically_signed(signed: &SignedTransaction, seed: &[u8; 32]) {
        let tx = v2(signed);
        // Public key must correspond to the signing seed.
        let sk = SigningKey::from_bytes(seed);
        assert_eq!(
            signed.public_key,
            sk.verifying_key().to_bytes(),
            "embedded public key must match the signer seed"
        );
        // `from` address must be derived from that public key.
        assert_eq!(
            tx.from,
            expected_from(seed),
            "from address must derive from pubkey"
        );
        // Locally recompute blake3 over the exact TransactionV2 bytes.
        let recomputed = blake3::hash(&tx.to_bytes());
        assert_eq!(
            recomputed.as_bytes(),
            tx.signing_hash().as_bytes(),
            "local blake3(tx bytes) must equal wire signing_hash()"
        );
        // Explicit ed25519 verification over the signing hash.
        assert!(
            signature_verifies(signed),
            "ed25519 signature must verify over the signing hash"
        );
    }

    /// Boolean ed25519 verification used by the negative tests. Returns
    /// `false` on any structural or cryptographic failure.
    fn signature_verifies(signed: &SignedTransaction) -> bool {
        let tx = match &signed.inner {
            TxInner::V2(tx) => tx,
            TxInner::Legacy(_) => return false,
        };
        let hash = blake3::hash(&tx.to_bytes());
        let vk = match VerifyingKey::from_bytes(&signed.public_key) {
            Ok(vk) => vk,
            Err(_) => return false,
        };
        let sig = Signature::from_bytes(&signed.signature);
        vk.verify_strict(hash.as_bytes(), &sig).is_ok()
    }

    // ── Full-signed byte locks + decode/verify/determinism (all 11 builders) ──

    #[test]
    fn builder_submit_storage_proof() {
        let call = || {
            build_submit_proof_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                [0x11; 32],
                LOCK_MERKLE_ROOT,
                9,
                [0x33; 32],
                vec![[0x44; 32], [0x55; 32]],
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_SUBMIT_PROOF,
            "SubmitStorageProof full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic: same inputs → same bytes");

        let signed = decode(&hex);
        let tx = v2(&signed);
        assert_eq!(tx.chain_id, LOCK_CHAIN_ID);
        assert_eq!(tx.nonce, LOCK_NONCE);
        assert_eq!(tx.fee, LOCK_FEE);
        match &tx.payload {
            TxPayload::StorageMetadata(d) => match &d.operation {
                StorageMetadataOperation::SubmitStorageProof {
                    challenge_id,
                    merkle_root,
                    chunk_index,
                    chunk_hash,
                    merkle_path,
                } => {
                    assert_eq!(challenge_id.as_bytes(), &[0x11; 32]);
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(*chunk_index, 9);
                    assert_eq!(chunk_hash.as_bytes(), &[0x33; 32]);
                    assert_eq!(merkle_path.len(), 2);
                    assert_eq!(merkle_path[0].as_bytes(), &[0x44; 32]);
                    assert_eq!(merkle_path[1].as_bytes(), &[0x55; 32]);
                }
                other => panic!("wrong operation variant: {other:?}"),
            },
            other => panic!("wrong payload variant: {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_register_encryption_key() {
        let call = || {
            build_register_encryption_key_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                [0xAB; 32],
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_REGISTER_ENC_KEY,
            "RegisterEncryptionKey full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        let tx = v2(&signed);
        match &tx.payload {
            TxPayload::NodeRegistryV2(d) => match &d.operation {
                NodeRegistryOperationV2::RegisterEncryptionKey { encryption_pubkey } => {
                    assert_eq!(*encryption_pubkey, [0xAB; 32]);
                }
            },
            other => panic!("expected NodeRegistryV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_register_archive_node() {
        let call = || {
            build_register_archive_node_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                1_000_000_000,
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_REGISTER_ARCHIVE,
            "Register(ArchiveNode) full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        let tx = v2(&signed);
        match &tx.payload {
            TxPayload::NodeRegistry(d) => match &d.operation {
                NodeRegistryOperation::Register { role, stake } => {
                    assert!(matches!(role, NodeRole::ArchiveNode));
                    assert_eq!(*stake, 1_000_000_000);
                }
                other => panic!("wrong NodeRegistry operation: {other:?}"),
            },
            other => panic!("expected NodeRegistry payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_register_file_pending_v2() {
        let entry = || AccessEntryV2Mirror {
            address: LOCK_ADDR,
            encrypted_key_bundle: Some(Bundle80(LOCK_BUNDLE)),
            expires_at: Some(1000),
        };
        let call = || {
            build_register_file_pending_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
                1024,
                1040,
                2,
                500_000,
                1,
                vec![entry()],
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_REGISTER_FILE_PENDING_V2,
            "RegisterFilePendingV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        let tx = v2(&signed);
        match &tx.payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::RegisterFilePendingV2 {
                    merkle_root,
                    plaintext_size_bytes,
                    stored_size_bytes,
                    chunk_count,
                    fee_deposit,
                    visibility,
                    initial_access,
                } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(*plaintext_size_bytes, 1024);
                    assert_eq!(*stored_size_bytes, 1040);
                    assert_eq!(*chunk_count, 2);
                    assert_eq!(*fee_deposit, 500_000);
                    assert_eq!(*visibility, 1);
                    assert_eq!(initial_access.len(), 1);
                    assert_eq!(initial_access[0].address.as_bytes(), &LOCK_ADDR);
                    assert_eq!(
                        initial_access[0].encrypted_key_bundle.unwrap().0,
                        LOCK_BUNDLE
                    );
                    assert_eq!(initial_access[0].expires_at, Some(1000));
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_activate_file_v2() {
        let call = || {
            build_activate_file_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_ACTIVATE_V2,
            "ActivateFileV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::ActivateFileV2 { merkle_root } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_abandon_file_v2() {
        let call = || {
            build_abandon_file_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_ABANDON_V2,
            "AbandonFileV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::AbandonFileV2 { merkle_root } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_accept_assignment_v2() {
        let call = || {
            build_accept_assignment_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
                vec![1, 2, 3],
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_ACCEPT_ASSIGNMENT_V2,
            "AcceptAssignmentV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::AcceptAssignmentV2 {
                    merkle_root,
                    chunk_indices,
                } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(chunk_indices, &vec![1, 2, 3]);
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_add_access_v2() {
        let entry = || AccessEntryV2Mirror {
            address: LOCK_ADDR,
            encrypted_key_bundle: Some(Bundle80(LOCK_BUNDLE)),
            expires_at: Some(1000),
        };
        let call = || {
            build_add_access_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
                entry(),
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_ADD_ACCESS_V2,
            "AddAccessV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::AddAccessV2 { merkle_root, entry } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(entry.address.as_bytes(), &LOCK_ADDR);
                    assert_eq!(entry.encrypted_key_bundle.unwrap().0, LOCK_BUNDLE);
                    assert_eq!(entry.expires_at, Some(1000));
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_remove_access_v2() {
        let call = || {
            build_remove_access_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
                LOCK_ADDR,
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_REMOVE_ACCESS_V2,
            "RemoveAccessV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::RemoveAccessV2 {
                    merkle_root,
                    address,
                } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(address.as_bytes(), &LOCK_ADDR);
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_update_access_v2() {
        // Clear-expiry flavor (expires_at = None), matching the baseline capture.
        let new_entry = || AccessEntryV2Mirror {
            address: LOCK_ADDR,
            encrypted_key_bundle: Some(Bundle80(LOCK_BUNDLE)),
            expires_at: None,
        };
        let call = || {
            build_update_access_v2_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                LOCK_MERKLE_ROOT,
                LOCK_ADDR,
                new_entry(),
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_UPDATE_ACCESS_V2,
            "UpdateAccessV2 full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        match &v2(&signed).payload {
            TxPayload::StorageMetadataV2(d) => match &d.operation {
                StorageMetadataOperationV2::UpdateAccessV2 {
                    merkle_root,
                    address,
                    new_entry,
                } => {
                    assert_eq!(merkle_root.as_bytes(), &LOCK_MERKLE_ROOT);
                    assert_eq!(address.as_bytes(), &LOCK_ADDR);
                    assert_eq!(new_entry.address.as_bytes(), &LOCK_ADDR);
                    assert_eq!(new_entry.encrypted_key_bundle.unwrap().0, LOCK_BUNDLE);
                    assert_eq!(
                        new_entry.expires_at, None,
                        "clear-expiry path must round-trip as None"
                    );
                }
                other => panic!("wrong V2 op variant: {other:?}"),
            },
            other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    #[test]
    fn builder_register_file() {
        let call = || {
            build_register_file_tx(
                &LOCK_SEED,
                LOCK_CHAIN_ID,
                LOCK_NONCE,
                LOCK_FEE,
                [0x66; 32],
                2_097_152,
                vec![[0x77; 20], [0x88; 20]],
                100_000_000,
            )
            .unwrap()
        };
        let hex = call();
        assert_eq!(
            hex, BL_REGISTER_FILE,
            "RegisterFile full signed bytes drifted"
        );
        assert_eq!(hex, call(), "deterministic");

        let signed = decode(&hex);
        let tx = v2(&signed);
        match &tx.payload {
            TxPayload::StorageMetadata(d) => match &d.operation {
                StorageMetadataOperation::RegisterFile {
                    merkle_root,
                    total_size_bytes,
                    access_list,
                    fee_deposit,
                } => {
                    assert_eq!(merkle_root.as_bytes(), &[0x66; 32]);
                    assert_eq!(*total_size_bytes, 2_097_152);
                    assert_eq!(access_list.len(), 2);
                    assert_eq!(access_list[0].as_bytes(), &[0x77; 20]);
                    assert_eq!(access_list[1].as_bytes(), &[0x88; 20]);
                    assert_eq!(*fee_deposit, 100_000_000);
                }
                other => panic!("wrong StorageMetadata operation: {other:?}"),
            },
            other => panic!("expected StorageMetadata payload, got {other:?}"),
        }
        assert_cryptographically_signed(&signed, &LOCK_SEED);
    }

    /// Every builder must emit `TxInner::V2` (no `Legacy` path exists).
    #[test]
    fn every_builder_emits_v2() {
        let hexes = [
            BL_SUBMIT_PROOF,
            BL_REGISTER_ENC_KEY,
            BL_REGISTER_ARCHIVE,
            BL_REGISTER_FILE_PENDING_V2,
            BL_ACTIVATE_V2,
            BL_ABANDON_V2,
            BL_ACCEPT_ASSIGNMENT_V2,
            BL_ADD_ACCESS_V2,
            BL_REMOVE_ACCESS_V2,
            BL_UPDATE_ACCESS_V2,
            BL_REGISTER_FILE,
        ];
        for h in hexes {
            let signed = decode(h);
            assert!(
                matches!(signed.inner, TxInner::V2(_)),
                "expected V2 inner for {h}"
            );
        }
    }

    /// No builder emits `ReassignChunksV2` (#62 stays a separate PR). The
    /// variant exists inside the imported upstream enum, but none of the seven
    /// V2 builders produces it.
    #[test]
    fn no_builder_emits_reassign_chunks_v2() {
        let v2_hexes = [
            BL_REGISTER_FILE_PENDING_V2,
            BL_ACTIVATE_V2,
            BL_ABANDON_V2,
            BL_ACCEPT_ASSIGNMENT_V2,
            BL_ADD_ACCESS_V2,
            BL_REMOVE_ACCESS_V2,
            BL_UPDATE_ACCESS_V2,
        ];
        for h in v2_hexes {
            let signed = decode(h);
            match &v2(&signed).payload {
                TxPayload::StorageMetadataV2(d) => assert!(
                    !matches!(
                        d.operation,
                        StorageMetadataOperationV2::ReassignChunksV2 { .. }
                    ),
                    "builder unexpectedly emitted ReassignChunksV2"
                ),
                other => panic!("expected StorageMetadataV2 payload, got {other:?}"),
            }
        }
    }

    // ── Negative coverage ────────────────────────────────────────────────────

    #[test]
    fn truncated_signed_bytes_are_rejected() {
        let bytes = hex::decode(BL_SUBMIT_PROOF).unwrap();
        assert!(
            SignedTransaction::from_bytes(&bytes[..bytes.len() - 1]).is_err(),
            "truncated signed bytes must be rejected by the wire decoder"
        );
        assert!(
            SignedTransaction::from_bytes(&bytes[..4]).is_err(),
            "severely truncated signed bytes must be rejected"
        );
    }

    #[test]
    fn malformed_signed_bytes_are_rejected() {
        let mut bytes = hex::decode(BL_SUBMIT_PROOF).unwrap();
        // The first 4 bytes are the u32-LE `TxInner` variant tag; only 0
        // (Legacy) and 1 (V2) are valid. Force an out-of-range tag.
        bytes[0] = 0x07;
        bytes[1] = 0x00;
        bytes[2] = 0x00;
        bytes[3] = 0x00;
        assert!(
            SignedTransaction::from_bytes(&bytes).is_err(),
            "an out-of-range TxInner variant tag must be rejected"
        );
    }

    /// The wire decoder (`bincode::deserialize`, `allow_trailing_bytes`)
    /// tolerates trailing bytes rather than rejecting them — so this test
    /// pins the honest behavior: decoding succeeds, but the canonical
    /// re-serialization is strictly shorter than the padded input, making the
    /// junk detectable by length. (We deliberately do NOT claim the decoder
    /// rejects trailing bytes.)
    #[test]
    fn trailing_bytes_are_tolerated_but_not_canonical() {
        let mut bytes = hex::decode(BL_SUBMIT_PROOF).unwrap();
        let canonical_len = bytes.len();
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let signed = SignedTransaction::from_bytes(&bytes).expect("trailing bytes are tolerated");
        assert_eq!(
            signed.to_bytes().len(),
            canonical_len,
            "canonical re-encoding must not include the trailing junk"
        );
    }

    /// Asserting the wrong operation expectation must not silently succeed:
    /// a SubmitStorageProof tx is not a RegisterFile tx.
    #[test]
    fn wrong_operation_expectation_fails() {
        let signed = decode(BL_SUBMIT_PROOF);
        match &v2(&signed).payload {
            TxPayload::StorageMetadata(d) => {
                assert!(
                    !matches!(d.operation, StorageMetadataOperation::RegisterFile { .. }),
                    "SubmitStorageProof must not match a RegisterFile expectation"
                );
                assert!(matches!(
                    d.operation,
                    StorageMetadataOperation::SubmitStorageProof { .. }
                ));
            }
            other => panic!("expected StorageMetadata payload, got {other:?}"),
        }
    }

    #[test]
    fn mutated_public_key_fails_verification() {
        let good = decode(BL_SUBMIT_PROOF);
        assert!(signature_verifies(&good), "baseline signature must verify");
        let mut bad = good.clone();
        bad.public_key[0] ^= 0xFF;
        assert!(
            !signature_verifies(&bad),
            "a mutated public key must fail ed25519 verification"
        );
    }

    #[test]
    fn mutated_signature_fails_verification() {
        let good = decode(BL_SUBMIT_PROOF);
        let mut bad = good.clone();
        bad.signature[0] ^= 0xFF;
        assert!(
            !signature_verifies(&bad),
            "a mutated signature must fail ed25519 verification"
        );
    }

    /// The `Bundle80` compatibility constructor enforces the 80-byte length
    /// at the type level, so a wrong-length bundle cannot be built. This pins
    /// that the boundary rejects any non-80-byte input before it could reach a
    /// payload.
    #[test]
    fn bundle80_wrong_length_rejected_at_boundary() {
        let short: &[u8] = &[0xCC; 79];
        assert!(
            <[u8; 80]>::try_from(short).is_err(),
            "79 bytes must not convert into a Bundle80's [u8; 80]"
        );
        let long: &[u8] = &[0xCC; 81];
        assert!(
            <[u8; 80]>::try_from(long).is_err(),
            "81 bytes must not convert into a Bundle80's [u8; 80]"
        );
        // Exactly 80 bytes is the only accepted length.
        let ok: &[u8] = &[0xCC; 80];
        let arr: [u8; 80] = ok.try_into().unwrap();
        assert_eq!(Bundle80(arr).0, LOCK_BUNDLE);
    }

    // ── Preserved wire fixtures (byte-identical expected hex) ─────────────────
    //
    // These pin the EXACT bincode-v1 op-level byte layout each V2 operation
    // emits, using the same fixed inputs the chain-side fixture tests use.
    // They are unchanged from the pre-PR-B fixtures (identical expected hex);
    // only the constructed types moved from private mirrors to the production
    // `sumchain-wire` types, which serialize byte-for-byte identically.

    const FIXTURE_MERKLE_ROOT: [u8; 32] = [0x42; 32];
    const FIXTURE_ENCRYPTION_PUBKEY: [u8; 32] = [0x11; 32];
    const FIXTURE_RECIPIENT_ADDR: [u8; 20] = [0x55; 20];
    const FIXTURE_BUNDLE: [u8; 80] = [0xCC; 80];
    const FIXTURE_EXPIRES_AT: u64 = 1000;

    fn fixture_chunk_indices() -> Vec<u32> {
        vec![1, 2, 3]
    }

    fn op_hex(op: StorageMetadataOperationV2) -> String {
        hex::encode(bincode1::serialize(&op).unwrap())
    }
    fn nr_op_hex(op: NodeRegistryOperationV2) -> String {
        hex::encode(bincode1::serialize(&op).unwrap())
    }

    /// Payload-level variant-index pin: SNIP and L1 must agree on which
    /// `TxPayload` variant is `StorageMetadataV2` (and its neighbours).
    /// Bincode v1 encodes enum variants as u32 little-endian.
    #[test]
    fn payload_v2_variant_indices_are_stable() {
        fn variant_index(p: TxPayload) -> u32 {
            let bytes = bincode1::serialize(&p).unwrap();
            assert!(
                bytes.len() >= 4,
                "expected at least 4 bytes for payload tag"
            );
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }

        assert_eq!(
            variant_index(TxPayload::NodeRegistry(NodeRegistryTxData {
                operation: NodeRegistryOperation::Register {
                    role: NodeRole::ArchiveNode,
                    stake: 0,
                },
            })),
            17,
            "V1 NodeRegistry payload index drift"
        );
        assert_eq!(
            variant_index(TxPayload::StorageMetadata(StorageMetadataTxData {
                operation: StorageMetadataOperation::RegisterFile {
                    merkle_root: Hash::new([0; 32]),
                    total_size_bytes: 0,
                    access_list: vec![],
                    fee_deposit: 0,
                },
            })),
            18,
            "V1 StorageMetadata payload index drift"
        );
        assert_eq!(
            variant_index(TxPayload::NodeRegistryV2(NodeRegistryV2TxData {
                operation: NodeRegistryOperationV2::RegisterEncryptionKey {
                    encryption_pubkey: [0; 32],
                },
            })),
            19,
            "NodeRegistryV2 payload index must be 19"
        );
        assert_eq!(
            variant_index(TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
                operation: StorageMetadataOperationV2::ActivateFileV2 {
                    merkle_root: Hash::new([0; 32]),
                },
            })),
            20,
            "StorageMetadataV2 payload index must be 20 — slot 19 is NodeRegistryV2"
        );
    }

    /// V2 op variant-index pin (indices 0..=6, unchanged). Bincode v1 emits
    /// enum variants as u32 little-endian.
    #[test]
    fn v2_op_variant_indices_are_stable() {
        fn variant_index(op: StorageMetadataOperationV2) -> u32 {
            let bytes = bincode1::serialize(&op).unwrap();
            assert!(
                bytes.len() >= 4,
                "expected at least 4 bytes for variant tag"
            );
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }
        assert_eq!(
            variant_index(StorageMetadataOperationV2::RegisterFilePendingV2 {
                merkle_root: Hash::new([0; 32]),
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
            variant_index(StorageMetadataOperationV2::ActivateFileV2 {
                merkle_root: Hash::new([0; 32])
            }),
            1
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2::AbandonFileV2 {
                merkle_root: Hash::new([0; 32])
            }),
            2
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2::AcceptAssignmentV2 {
                merkle_root: Hash::new([0; 32]),
                chunk_indices: vec![],
            }),
            3
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2::AddAccessV2 {
                merkle_root: Hash::new([0; 32]),
                entry: AccessEntryV2 {
                    address: Address::new([0; 20]),
                    encrypted_key_bundle: None,
                    expires_at: None,
                },
            }),
            4
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2::RemoveAccessV2 {
                merkle_root: Hash::new([0; 32]),
                address: Address::new([0; 20]),
            }),
            5
        );
        assert_eq!(
            variant_index(StorageMetadataOperationV2::UpdateAccessV2 {
                merkle_root: Hash::new([0; 32]),
                address: Address::new([0; 20]),
                new_entry: AccessEntryV2 {
                    address: Address::new([0; 20]),
                    encrypted_key_bundle: None,
                    expires_at: None,
                },
            }),
            6
        );
    }

    #[test]
    fn fixture_register_encryption_key_bytes() {
        let expected = "00000000".to_string() + &"11".repeat(32);
        let actual = nr_op_hex(NodeRegistryOperationV2::RegisterEncryptionKey {
            encryption_pubkey: FIXTURE_ENCRYPTION_PUBKEY,
        });
        assert_eq!(
            actual, expected,
            "RegisterEncryptionKey wire bytes diverged from chain fixture"
        );
    }

    #[test]
    fn fixture_register_file_pending_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2::RegisterFilePendingV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            plaintext_size_bytes: 1024,
            stored_size_bytes: 1040,
            chunk_count: 1,
            fee_deposit: 0,
            visibility: 1,
            initial_access: vec![],
        });
        let shared_prefix = "00000000".to_string() + &"42".repeat(32);
        assert!(
            actual.starts_with(&shared_prefix),
            "RegisterFilePendingV2 prefix mismatch. expected_prefix={shared_prefix} actual={actual}"
        );
        let aux = "0004000000000000".to_string() // plaintext_size_bytes=1024
            + "1004000000000000" // stored_size_bytes=1040
            + "01000000" // chunk_count=1
            + "0000000000000000" // fee_deposit=0
            + "01" // visibility=1
            + "0000000000000000"; // empty initial_access Vec length
        let expected = shared_prefix + &aux;
        assert_eq!(actual, expected, "RegisterFilePendingV2 fixture drift");
    }

    #[test]
    fn fixture_activate_file_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2::ActivateFileV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
        });
        let expected = "01000000".to_string() + &"42".repeat(32);
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_abandon_file_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2::AbandonFileV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
        });
        let expected = "02000000".to_string() + &"42".repeat(32);
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_accept_assignment_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2::AcceptAssignmentV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            chunk_indices: fixture_chunk_indices(),
        });
        let expected = "03000000".to_string()
            + &"42".repeat(32)
            + "0300000000000000"
            + "01000000"
            + "02000000"
            + "03000000";
        assert_eq!(
            actual, expected,
            "AcceptAssignmentV2 wire bytes diverged from chain fixture"
        );
    }

    #[test]
    fn fixture_tx_payload_node_registry_v2_register_encryption_key() {
        let payload = TxPayload::NodeRegistryV2(NodeRegistryV2TxData {
            operation: NodeRegistryOperationV2::RegisterEncryptionKey {
                encryption_pubkey: FIXTURE_ENCRYPTION_PUBKEY,
            },
        });
        let actual = hex::encode(bincode1::serialize(&payload).unwrap());
        let expected = "13000000".to_string() + "00000000" + &"11".repeat(32);
        assert_eq!(actual, expected, "TxPayload::NodeRegistryV2 != index 19");
    }

    #[test]
    fn fixture_tx_payload_storage_metadata_v2_accept_assignment() {
        let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
            operation: StorageMetadataOperationV2::AcceptAssignmentV2 {
                merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                chunk_indices: fixture_chunk_indices(),
            },
        });
        let actual = hex::encode(bincode1::serialize(&payload).unwrap());
        let expected = "14000000".to_string()
            + "03000000"
            + &"42".repeat(32)
            + "0300000000000000"
            + "01000000"
            + "02000000"
            + "03000000";
        assert_eq!(
            actual, expected,
            "TxPayload::StorageMetadataV2 != 20 OR AcceptAssignmentV2 != 3"
        );
    }

    #[test]
    fn fixture_tx_payload_storage_metadata_v2_activate_and_abandon() {
        for (op, inner_tag, label) in [
            (
                StorageMetadataOperationV2::ActivateFileV2 {
                    merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                },
                "01000000",
                "activate",
            ),
            (
                StorageMetadataOperationV2::AbandonFileV2 {
                    merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                },
                "02000000",
                "abandon",
            ),
        ] {
            let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData { operation: op });
            let actual = hex::encode(bincode1::serialize(&payload).unwrap());
            let expected = "14000000".to_string() + inner_tag + &"42".repeat(32);
            assert_eq!(actual, expected, "{label} wrapper bytes diverged");
        }
    }

    #[test]
    fn fixture_add_access_v2_bytes() {
        let entry = AccessEntryV2 {
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            encrypted_key_bundle: Some(Bundle80(FIXTURE_BUNDLE)),
            expires_at: Some(FIXTURE_EXPIRES_AT),
        };
        let actual = op_hex(StorageMetadataOperationV2::AddAccessV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            entry,
        });
        let expected = "04000000".to_string()
            + &"42".repeat(32)
            + &"55".repeat(20)
            + "01"
            + &"cc".repeat(80)
            + "01"
            + "e803000000000000";
        assert_eq!(
            actual, expected,
            "AddAccessV2 wire bytes diverged from chain fixture"
        );
    }

    #[test]
    fn fixture_remove_access_v2_bytes() {
        let actual = op_hex(StorageMetadataOperationV2::RemoveAccessV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
        });
        let expected = "05000000".to_string() + &"42".repeat(32) + &"55".repeat(20);
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_update_access_v2_bytes() {
        let new_entry = AccessEntryV2 {
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            encrypted_key_bundle: Some(Bundle80(FIXTURE_BUNDLE)),
            expires_at: Some(FIXTURE_EXPIRES_AT),
        };
        let actual = op_hex(StorageMetadataOperationV2::UpdateAccessV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            new_entry,
        });
        let expected = "06000000".to_string()
            + &"42".repeat(32)
            + &"55".repeat(20)
            + &"55".repeat(20)
            + "01"
            + &"cc".repeat(80)
            + "01"
            + "e803000000000000";
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_update_access_v2_clear_expires_at_bytes() {
        let new_entry = AccessEntryV2 {
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            encrypted_key_bundle: Some(Bundle80(FIXTURE_BUNDLE)),
            expires_at: None,
        };
        let actual = op_hex(StorageMetadataOperationV2::UpdateAccessV2 {
            merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            new_entry,
        });
        let expected = "06000000".to_string()
            + &"42".repeat(32)
            + &"55".repeat(20)
            + &"55".repeat(20)
            + "01"
            + &"cc".repeat(80)
            + "00"; // Option<u64>::None
        assert_eq!(actual, expected);
    }

    #[test]
    fn fixture_tx_payload_storage_metadata_v2_add_remove_update() {
        let entry = AccessEntryV2 {
            address: Address::new(FIXTURE_RECIPIENT_ADDR),
            encrypted_key_bundle: Some(Bundle80(FIXTURE_BUNDLE)),
            expires_at: Some(FIXTURE_EXPIRES_AT),
        };
        for (op, inner_tag, label) in [
            (
                StorageMetadataOperationV2::AddAccessV2 {
                    merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                    entry: entry.clone(),
                },
                "04000000",
                "add",
            ),
            (
                StorageMetadataOperationV2::RemoveAccessV2 {
                    merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                    address: Address::new(FIXTURE_RECIPIENT_ADDR),
                },
                "05000000",
                "remove",
            ),
            (
                StorageMetadataOperationV2::UpdateAccessV2 {
                    merkle_root: Hash::new(FIXTURE_MERKLE_ROOT),
                    address: Address::new(FIXTURE_RECIPIENT_ADDR),
                    new_entry: entry.clone(),
                },
                "06000000",
                "update",
            ),
        ] {
            let payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData { operation: op });
            let actual = hex::encode(bincode1::serialize(&payload).unwrap());
            assert!(
                actual.starts_with(&format!("14000000{inner_tag}")),
                "{label}: outer (20) + inner tag prefix mismatch — got {actual}"
            );
        }
    }
}
