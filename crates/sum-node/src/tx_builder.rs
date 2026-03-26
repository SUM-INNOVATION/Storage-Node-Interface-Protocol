//! Transaction builder for submitting PoR proofs to the SUM Chain L1.
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

/// Build a hex-encoded `SignedTransaction` for `SubmitStorageProof`.
///
/// The returned hex string can be submitted directly to `send_raw_transaction`.
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
    // Derive keypair from seed.
    let signing_key = SigningKey::from_bytes(ed25519_seed);
    let pubkey_bytes: [u8; 32] = signing_key.verifying_key().to_bytes();

    // Derive L1 address: blake3(pubkey)[12..32]
    let pubkey_hash = blake3::hash(&pubkey_bytes);
    let mut from_addr = [0u8; 20];
    from_addr.copy_from_slice(&pubkey_hash.as_bytes()[12..32]);

    // Build the unsigned transaction.
    let tx = TransactionV2Mirror {
        chain_id,
        from: from_addr,
        fee,
        nonce,
        payload: TxPayloadMirror::StorageMetadata(StorageMetadataTxDataMirror {
            operation: StorageMetadataOperationMirror::SubmitStorageProof {
                challenge_id,
                merkle_root,
                chunk_index,
                chunk_hash,
                merkle_path,
            },
        }),
    };

    // Serialize with bincode v1 to get signing hash.
    let tx_bytes = bincode1::serialize(&tx).context("bincode v1 serialization of tx failed")?;
    let signing_hash = blake3::hash(&tx_bytes);

    // Sign with Ed25519.
    let signature = signing_key.sign(signing_hash.as_bytes());
    let sig_bytes: [u8; 64] = signature.to_bytes();

    // Wrap in SignedTransaction.
    let signed = SignedTransactionMirror {
        inner: TxInnerMirror::V2(tx),
        signature: sig_bytes,
        public_key: pubkey_bytes,
    };

    // Serialize the full signed transaction with bincode v1 and hex-encode.
    let raw_bytes =
        bincode1::serialize(&signed).context("bincode v1 serialization of signed tx failed")?;
    Ok(hex::encode(&raw_bytes))
}

// ── Mirror Types ─────────────────────────────────────────────────────────────
//
// These must match the L1's types exactly in field order and variant indices.
// Source: sum-chain/crates/primitives/src/transaction.rs and storage_metadata.rs

/// Mirror of `StorageMetadataOperation` (variant indices must match L1).
#[derive(Debug, Serialize, Deserialize)]
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

/// Mirror of `TxPayload` — 19 variants, StorageMetadata at index 18.
///
/// We only construct the `StorageMetadata` variant. The other 18 are
/// placeholders that keep the variant indices aligned. Since bincode v1
/// only serializes the active variant, their internal types are irrelevant.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum TxPayloadMirror {
    Transfer { to: [u8; 20], amount: u128 },  // 0
    Nft(Vec<u8>),                              // 1
    Token(Vec<u8>),                            // 2
    ContractDeploy(Vec<u8>),                   // 3
    ContractCall(Vec<u8>),                     // 4
    Staking(Vec<u8>),                          // 5
    Messaging(Vec<u8>),                        // 6
    DocClass(Vec<u8>),                         // 7
    Tax(Vec<u8>),                              // 8
    Equity(Vec<u8>),                           // 9
    Agreement(Vec<u8>),                        // 10
    Legal(Vec<u8>),                            // 11
    Property(Vec<u8>),                         // 12
    Healthcare(Vec<u8>),                       // 13
    Employment(Vec<u8>),                       // 14
    Finance(Vec<u8>),                          // 15
    PolicyAccount(Vec<u8>),                    // 16
    NodeRegistry(Vec<u8>),                     // 17
    StorageMetadata(StorageMetadataTxDataMirror), // 18 — the only variant we use
}

/// Mirror of `TransactionV2`.
/// Fields must be in the same order as the L1's struct.
#[derive(Debug, Serialize, Deserialize)]
struct TransactionV2Mirror {
    chain_id: u64,
    from: [u8; 20],   // Address newtype is transparent under bincode v1
    fee: u128,         // Balance = u128
    nonce: u64,
    payload: TxPayloadMirror,
}

/// Mirror of `TxInner`.
#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
enum TxInnerMirror {
    Legacy(Vec<u8>),           // index 0 — placeholder, never constructed
    V2(TransactionV2Mirror),   // index 1
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
            &seed,
            1,                  // chain_id
            0,                  // nonce
            1_000_000,          // fee
            [0xAA; 32],         // challenge_id
            [0xBB; 32],         // merkle_root
            5,                  // chunk_index
            [0xCC; 32],         // chunk_hash
            vec![[0xDD; 32], [0xEE; 32]], // merkle_path (2 siblings)
        )
        .unwrap();

        // Should be a valid hex string.
        assert!(!hex.is_empty());
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));

        // Should be deserializable back (proves bincode v1 round-trip).
        let bytes = hex::decode(&hex).unwrap();
        let signed: SignedTransactionMirror = bincode1::deserialize(&bytes).unwrap();

        // Verify the transaction contents survived.
        match signed.inner {
            TxInnerMirror::V2(tx) => {
                assert_eq!(tx.chain_id, 1);
                assert_eq!(tx.nonce, 0);
                assert_eq!(tx.fee, 1_000_000);
                match tx.payload {
                    TxPayloadMirror::StorageMetadata(data) => {
                        match data.operation {
                            StorageMetadataOperationMirror::SubmitStorageProof {
                                challenge_id,
                                merkle_root,
                                chunk_index,
                                chunk_hash,
                                merkle_path,
                            } => {
                                assert_eq!(challenge_id, [0xAA; 32]);
                                assert_eq!(merkle_root, [0xBB; 32]);
                                assert_eq!(chunk_index, 5);
                                assert_eq!(chunk_hash, [0xCC; 32]);
                                assert_eq!(merkle_path.len(), 2);
                            }
                            _ => panic!("wrong operation variant"),
                        }
                    }
                    _ => panic!("wrong payload variant"),
                }
            }
            _ => panic!("wrong TxInner variant"),
        }

        // Verify Ed25519 signature.
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        assert_eq!(signed.public_key, verifying_key.to_bytes());
    }

    #[test]
    fn deterministic_tx_hex() {
        let seed = [1u8; 32];
        let hex1 = build_submit_proof_tx(
            &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
        )
        .unwrap();
        let hex2 = build_submit_proof_tx(
            &seed, 1, 0, 100, [0; 32], [1; 32], 0, [2; 32], vec![],
        )
        .unwrap();
        assert_eq!(hex1, hex2, "same inputs must produce same tx hex");
    }
}
