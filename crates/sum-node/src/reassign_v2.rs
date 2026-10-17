//! Owner-triggered `ReassignChunksV2` for a file this operator owns.
//!
//! The chain decides when a file needs a new assignment epoch: its
//! `storage_getAssignmentCoverageV2` response sets `reassignment_needed`
//! once an archive assigned under the latest epoch has left the active set,
//! and the executor accepts `ReassignChunksV2` only from the file's owner and
//! only in that state.
//!
//! This module adds no signing authority. The flow is:
//!
//! 1. Read the file row and refuse unless its owner is this key's address.
//! 2. Read coverage and refuse unless the chain reports `reassignment_needed`.
//! 3. Ask the chain's no-key builder, `storage_buildReassignChunksV2`, for the
//!    unsigned transaction.
//! 4. Verify the returned bytes against the request: canonical encoding, chain
//!    id, sender, fee, the exact `ReassignChunksV2 { merkle_root }` payload,
//!    and that every echoed field and the signing hash agree with the decoded
//!    transaction. `ReassignChunksV2` carries no epoch field; the executor
//!    stamps the new epoch at the current height.
//! 5. With `dry_run`, stop here: nothing is signed or sent.
//! 6. Otherwise sign with the same owner key and signing path every other owner
//!    transaction uses ([`crate::tx_builder::sign_transaction_v2`]), submit, and
//!    wait for finality. A chain refusal is reported with its receipt reason.
//!
//! Every check fails closed: an RPC error, a missing row, a foreign owner, a
//! `reassignment_needed: false` response or any mismatch in the built bytes
//! ends the call before anything is signed.
//!
//! Scope: this submits the chain transaction only. SNIP's push, attestation
//! and download paths still use the file's original assignment snapshot, so
//! a reassignment does not by itself restore coverage under the new epoch.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use sum_types::rpc_types::{AssignmentCoverageV2, StorageBuildResponse, StorageFileInfoV2};
use sumchain_wire::{
    Address, Hash,
    storage_metadata::{StorageMetadataOperationV2, StorageMetadataV2TxData},
    transaction::{TransactionV2, TxPayload},
};
use tracing::info;

use crate::rpc_client::L1RpcClient;
use crate::tx_wait::{TxStatusSource, TxWaitError, wait_for_finalized};

/// Chain calls the reassignment flow needs. Production impl on
/// [`L1RpcClient`]; tests provide a scripted mock.
#[async_trait]
pub trait ReassignRpc: TxStatusSource + Send + Sync {
    /// `Ok(None)` — the chain has no V2 row for this root.
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>>;
    /// `Ok(None)` — the chain has no coverage row for this root.
    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<AssignmentCoverageV2>>;
    async fn storage_build_reassign_chunks_v2(
        &self,
        from_base58: &str,
        merkle_root_hex: &str,
        fee: u128,
    ) -> Result<StorageBuildResponse>;
    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String>;
}

#[async_trait]
impl ReassignRpc for L1RpcClient {
    async fn storage_get_file_info_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<StorageFileInfoV2>> {
        L1RpcClient::storage_get_file_info_v2(self, merkle_root_hex, None, None).await
    }

    async fn storage_get_assignment_coverage_v2(
        &self,
        merkle_root_hex: &str,
    ) -> Result<Option<AssignmentCoverageV2>> {
        // The flow reads only the flags, so ask for an empty missing-index window.
        L1RpcClient::storage_get_assignment_coverage_v2(self, merkle_root_hex, None, Some(0)).await
    }

    async fn storage_build_reassign_chunks_v2(
        &self,
        from_base58: &str,
        merkle_root_hex: &str,
        fee: u128,
    ) -> Result<StorageBuildResponse> {
        L1RpcClient::storage_build_reassign_chunks_v2(self, from_base58, merkle_root_hex, fee).await
    }

    async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
        L1RpcClient::send_raw_transaction(self, signed_tx_hex).await
    }
}

/// Inputs for one [`reassign_chunks_v2`] call.
#[derive(Debug, Clone)]
pub struct ReassignRequest {
    /// The file's 32-byte Merkle root.
    pub merkle_root: [u8; 32],
    /// Chain id the built transaction must carry.
    pub chain_id: u64,
    /// Fee to request from the builder; the built transaction must carry
    /// exactly this fee.
    pub fee: u128,
    /// Verify the built transaction, then stop without signing or sending.
    pub dry_run: bool,
    pub poll_interval: Duration,
    pub finality_timeout: Duration,
}

/// Terminal outcome of [`reassign_chunks_v2`].
#[derive(Debug)]
pub enum ReassignOutcome {
    /// `ReassignChunksV2` finalized at `finalized_at_height`.
    Reassigned {
        tx_hash: String,
        finalized_at_height: u64,
    },
    /// `dry_run`: the built transaction verified; nothing was signed or sent.
    DryRun {
        nonce: u64,
        fee: u128,
        signing_hash: String,
    },
    /// The chain has no V2 row (or no coverage row) for this root.
    NotRegistered,
    /// The file belongs to another address; this key never signs for it.
    NotOwner { owner: String },
    /// The chain does not report `reassignment_needed`; submitting would only
    /// burn a fee.
    NotNeeded {
        latest_assignment_epoch: Option<u64>,
    },
    /// The builder's output does not match the request; nothing was signed.
    BuildMismatch { reason: String },
    /// The chain executed and refused the transaction. `reason` is the
    /// chain's receipt reason, which carries its refusal code.
    Refused {
        tx_hash: String,
        block_height: Option<u64>,
        reason: String,
    },
    /// An RPC, signing, submission or finality-wait failure.
    Failed { source: anyhow::Error },
}

/// Verify a `storage_buildReassignChunksV2` response against the request
/// and return the decoded unsigned transaction. Fails closed on any
/// disagreement; the error names the first mismatch found.
pub fn verify_unsigned_reassign(
    resp: &StorageBuildResponse,
    expected_from: &Address,
    merkle_root: &[u8; 32],
    chain_id: u64,
    fee: u128,
) -> std::result::Result<TransactionV2, String> {
    let bytes = decode_hex_field("unsigned_tx", &resp.unsigned_tx)?;
    let tx = TransactionV2::from_bytes(&bytes)
        .map_err(|e| format!("unsigned_tx does not decode as one TransactionV2: {e}"))?;
    if tx.to_bytes() != bytes {
        return Err("unsigned_tx is not the canonical encoding of its transaction".into());
    }
    if tx.chain_id != chain_id {
        return Err(format!("chain_id is {}, expected {chain_id}", tx.chain_id));
    }
    if tx.from != *expected_from {
        return Err(format!(
            "sender is {}, expected {}",
            tx.from.to_base58(),
            expected_from.to_base58()
        ));
    }
    if tx.fee != fee {
        return Err(format!("fee is {}, expected {fee}", tx.fee));
    }
    let expected_payload = TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
        operation: StorageMetadataOperationV2::ReassignChunksV2 {
            merkle_root: Hash::new(*merkle_root),
        },
    });
    if tx.payload != expected_payload {
        return Err(format!(
            "payload is not ReassignChunksV2 for 0x{}",
            hex::encode(merkle_root)
        ));
    }
    if resp.from != tx.from.to_base58() {
        return Err(format!(
            "response `from` {} disagrees with the transaction sender",
            resp.from
        ));
    }
    if resp.nonce != tx.nonce {
        return Err(format!(
            "response nonce {} disagrees with the transaction nonce {}",
            resp.nonce, tx.nonce
        ));
    }
    if resp.fee != tx.fee {
        return Err(format!(
            "response fee {} disagrees with the transaction fee {}",
            resp.fee, tx.fee
        ));
    }
    if resp.chain_id != tx.chain_id {
        return Err(format!(
            "response chain_id {} disagrees with the transaction chain_id {}",
            resp.chain_id, tx.chain_id
        ));
    }
    let signing_hash = decode_hex_field("signing_hash", &resp.signing_hash)?;
    if signing_hash.as_slice() != tx.signing_hash().as_bytes() {
        return Err("response signing_hash is not the hash of unsigned_tx".into());
    }
    Ok(tx)
}

fn decode_hex_field(name: &str, value: &str) -> std::result::Result<Vec<u8>, String> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    hex::decode(digits).map_err(|e| format!("{name} is not hex: {e}"))
}

/// Run the owner-triggered reassignment for one file. See the module docs
/// for the flow and its fail-closed checks.
pub async fn reassign_chunks_v2<R: ReassignRpc + ?Sized>(
    rpc: &R,
    signing_key_seed: &[u8; 32],
    req: &ReassignRequest,
) -> ReassignOutcome {
    let own = Address::from_public_key(
        &SigningKey::from_bytes(signing_key_seed)
            .verifying_key()
            .to_bytes(),
    );
    let own_b58 = own.to_base58();
    let root_hex = format!("0x{}", hex::encode(req.merkle_root));

    let info = match rpc.storage_get_file_info_v2(&root_hex).await {
        Ok(Some(info)) => info,
        Ok(None) => return ReassignOutcome::NotRegistered,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    if info.owner != own_b58 {
        return ReassignOutcome::NotOwner { owner: info.owner };
    }

    let coverage = match rpc.storage_get_assignment_coverage_v2(&root_hex).await {
        Ok(Some(c)) => c,
        Ok(None) => return ReassignOutcome::NotRegistered,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    if !coverage.reassignment_needed {
        return ReassignOutcome::NotNeeded {
            latest_assignment_epoch: coverage.resolved_latest_epoch(),
        };
    }

    let resp = match rpc
        .storage_build_reassign_chunks_v2(&own_b58, &root_hex, req.fee)
        .await
    {
        Ok(r) => r,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    let tx = match verify_unsigned_reassign(&resp, &own, &req.merkle_root, req.chain_id, req.fee) {
        Ok(tx) => tx,
        Err(reason) => return ReassignOutcome::BuildMismatch { reason },
    };

    if req.dry_run {
        return ReassignOutcome::DryRun {
            nonce: tx.nonce,
            fee: tx.fee,
            signing_hash: resp.signing_hash,
        };
    }

    let signed_hex = match crate::tx_builder::sign_transaction_v2(signing_key_seed, tx) {
        Ok(h) => h,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    let tx_hash = match rpc.send_raw_transaction(&signed_hex).await {
        Ok(h) => h,
        Err(e) => return ReassignOutcome::Failed { source: e },
    };
    match wait_for_finalized(rpc, &tx_hash, req.poll_interval, req.finality_timeout).await {
        Ok(height) => {
            info!(root = %root_hex, %tx_hash, finalized_at_height = height, "ReassignChunksV2 finalized");
            ReassignOutcome::Reassigned {
                tx_hash,
                finalized_at_height: height,
            }
        }
        Err(TxWaitError::Failed {
            block_height,
            reason,
        }) => ReassignOutcome::Refused {
            tx_hash,
            block_height,
            reason,
        },
        Err(TxWaitError::Dropped) => ReassignOutcome::Failed {
            source: anyhow::anyhow!(
                "ReassignChunksV2 {tx_hash} dropped before inclusion; re-run the command"
            ),
        },
        Err(TxWaitError::Timeout { last_status }) => ReassignOutcome::Failed {
            source: anyhow::anyhow!(
                "ReassignChunksV2 {tx_hash}: finality timeout; last status {last_status:?}"
            ),
        },
        Err(TxWaitError::Rpc(e)) => ReassignOutcome::Failed { source: e },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use std::sync::Mutex;
    use sum_types::rpc_types::{LifecycleV2, TxStatusV2};
    use sumchain_wire::transaction::{SignedTransaction, TxInner};

    const SEED: [u8; 32] = [0x11; 32];
    const OTHER_SEED: [u8; 32] = [0x22; 32];
    const ROOT: [u8; 32] = [0x5a; 32];
    const CHAIN_ID: u64 = 1;
    const FEE: u128 = 1_000;
    const NONCE: u64 = 9;

    fn addr(seed: &[u8; 32]) -> Address {
        Address::from_public_key(&SigningKey::from_bytes(seed).verifying_key().to_bytes())
    }

    fn root_hex() -> String {
        format!("0x{}", hex::encode(ROOT))
    }

    fn file_info(owner: &Address) -> StorageFileInfoV2 {
        serde_json::from_value(serde_json::json!({
            "merkle_root": root_hex(),
            "owner": owner.to_base58(),
            "plaintext_size_bytes": 2048,
            "stored_size_bytes": 2048,
            "chunk_count": 2,
            "fee_pool": 5000,
            "created_at": 100,
            "activated_at_height": 120,
            "abandoned_at_height": null,
            "assignment_height": 100,
            "lifecycle": LifecycleV2::ACTIVE.0,
            "visibility": 0,
            "access_list": [],
        }))
        .expect("file info fixture")
    }

    fn coverage(needed: bool) -> AssignmentCoverageV2 {
        serde_json::from_value(serde_json::json!({
            "chunk_count": 2,
            "covered_count": 2,
            "can_activate_now": false,
            "missing_total": 0,
            "missing_offset": 0,
            "missing_indices": [],
            "per_archive": [],
            "assignment_epochs": [100],
            "latest_assignment_epoch": 100,
            "reassignment_needed": needed,
            "per_epoch": [],
        }))
        .expect("coverage fixture")
    }

    /// What an honest chain builder returns for `from`/`root`.
    fn honest_build(
        from: &Address,
        root: [u8; 32],
        chain_id: u64,
        fee: u128,
    ) -> StorageBuildResponse {
        build_response(&TransactionV2 {
            chain_id,
            from: *from,
            fee,
            nonce: NONCE,
            payload: TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
                operation: StorageMetadataOperationV2::ReassignChunksV2 {
                    merkle_root: Hash::new(root),
                },
            }),
        })
    }

    fn build_response(tx: &TransactionV2) -> StorageBuildResponse {
        StorageBuildResponse {
            unsigned_tx: format!("0x{}", hex::encode(tx.to_bytes())),
            signing_hash: format!("0x{}", hex::encode(tx.signing_hash().as_bytes())),
            from: tx.from.to_base58(),
            nonce: tx.nonce,
            fee: tx.fee,
            chain_id: tx.chain_id,
        }
    }

    fn request(dry_run: bool) -> ReassignRequest {
        ReassignRequest {
            merkle_root: ROOT,
            chain_id: CHAIN_ID,
            fee: FEE,
            dry_run,
            poll_interval: Duration::from_millis(1),
            finality_timeout: Duration::from_millis(200),
        }
    }

    enum Reply<T> {
        Ok(T),
        Err(&'static str),
    }

    impl<T: Clone> Reply<T> {
        fn get(&self) -> Result<T> {
            match self {
                Reply::Ok(v) => Ok(v.clone()),
                Reply::Err(e) => Err(anyhow::anyhow!(*e)),
            }
        }
    }

    struct MockRpc {
        info: Reply<Option<StorageFileInfoV2>>,
        coverage: Reply<Option<AssignmentCoverageV2>>,
        build: Reply<StorageBuildResponse>,
        send: Reply<String>,
        status: TxStatusV2,
        build_calls: Mutex<Vec<(String, String, u128)>>,
        sent: Mutex<Vec<String>>,
    }

    impl MockRpc {
        fn happy() -> Self {
            let own = addr(&SEED);
            MockRpc {
                info: Reply::Ok(Some(file_info(&own))),
                coverage: Reply::Ok(Some(coverage(true))),
                build: Reply::Ok(honest_build(&own, ROOT, CHAIN_ID, FEE)),
                send: Reply::Ok("0xfeed".into()),
                status: TxStatusV2::Finalized { block_height: 777 },
                build_calls: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            }
        }
        fn built(&self) -> usize {
            self.build_calls.lock().unwrap().len()
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TxStatusSource for MockRpc {
        async fn get_transaction_status(&self, _tx_hash: &str) -> Result<TxStatusV2> {
            Ok(self.status.clone())
        }
    }

    #[async_trait]
    impl ReassignRpc for MockRpc {
        async fn storage_get_file_info_v2(&self, _: &str) -> Result<Option<StorageFileInfoV2>> {
            self.info.get()
        }
        async fn storage_get_assignment_coverage_v2(
            &self,
            _: &str,
        ) -> Result<Option<AssignmentCoverageV2>> {
            self.coverage.get()
        }
        async fn storage_build_reassign_chunks_v2(
            &self,
            from_base58: &str,
            merkle_root_hex: &str,
            fee: u128,
        ) -> Result<StorageBuildResponse> {
            self.build_calls.lock().unwrap().push((
                from_base58.to_string(),
                merkle_root_hex.to_string(),
                fee,
            ));
            self.build.get()
        }
        async fn send_raw_transaction(&self, signed_tx_hex: &str) -> Result<String> {
            self.sent.lock().unwrap().push(signed_tx_hex.to_string());
            self.send.get()
        }
    }

    #[tokio::test]
    async fn happy_path_signs_the_built_tx_with_the_owner_key_and_waits_for_finality() {
        let rpc = MockRpc::happy();
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        match out {
            ReassignOutcome::Reassigned {
                tx_hash,
                finalized_at_height,
            } => {
                assert_eq!(tx_hash, "0xfeed");
                assert_eq!(finalized_at_height, 777);
            }
            other => panic!("expected Reassigned, got {other:?}"),
        }

        // The builder was asked for this owner, this root and this fee.
        let calls = rpc.build_calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(addr(&SEED).to_base58(), root_hex(), FEE)]);

        // Exactly one transaction went out: the built one, signed by SEED.
        let sent = rpc.sent();
        assert_eq!(sent.len(), 1);
        let signed = SignedTransaction::from_hex(&sent[0]).expect("signed tx decodes");
        let tx = match &signed.inner {
            TxInner::V2(tx) => tx,
            other => panic!("expected a V2 signed transaction, got {other:?}"),
        };
        let (sig, pubkey) = (signed.signature, signed.public_key);
        let expected = honest_build(&addr(&SEED), ROOT, CHAIN_ID, FEE);
        assert_eq!(
            format!("0x{}", hex::encode(tx.to_bytes())),
            expected.unsigned_tx,
            "the signed transaction is the built one, unchanged"
        );
        assert_eq!(
            pubkey,
            SigningKey::from_bytes(&SEED).verifying_key().to_bytes()
        );
        VerifyingKey::from_bytes(&pubkey)
            .unwrap()
            .verify(tx.signing_hash().as_bytes(), &Signature::from_bytes(&sig))
            .expect("signature verifies over the signing hash");
    }

    #[tokio::test]
    async fn not_needed_is_refused_before_building_or_signing() {
        let rpc = MockRpc {
            coverage: Reply::Ok(Some(coverage(false))),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(
                out,
                ReassignOutcome::NotNeeded {
                    latest_assignment_epoch: Some(100)
                }
            ),
            "got {out:?}"
        );
        assert_eq!(rpc.built(), 0);
        assert!(rpc.sent().is_empty());
    }

    #[tokio::test]
    async fn a_file_owned_by_someone_else_is_never_signed_for() {
        let rpc = MockRpc {
            info: Reply::Ok(Some(file_info(&addr(&OTHER_SEED)))),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(
            matches!(out, ReassignOutcome::NotOwner { .. }),
            "got {out:?}"
        );
        assert_eq!(rpc.built(), 0);
        assert!(rpc.sent().is_empty());
    }

    #[tokio::test]
    async fn an_unregistered_file_is_refused() {
        let rpc = MockRpc {
            info: Reply::Ok(None),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::NotRegistered), "got {out:?}");
        assert!(rpc.sent().is_empty());

        let rpc = MockRpc {
            coverage: Reply::Ok(None),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::NotRegistered), "got {out:?}");
        assert!(rpc.sent().is_empty());
    }

    /// Every way the builder's output can disagree with the request is
    /// refused before anything is signed.
    #[tokio::test]
    async fn mismatched_builder_output_is_refused_unsigned() {
        let own = addr(&SEED);
        let other = addr(&OTHER_SEED);
        let good = honest_build(&own, ROOT, CHAIN_ID, FEE);
        let abandon = build_response(&TransactionV2 {
            chain_id: CHAIN_ID,
            from: own,
            fee: FEE,
            nonce: NONCE,
            payload: TxPayload::StorageMetadataV2(StorageMetadataV2TxData {
                operation: StorageMetadataOperationV2::AbandonFileV2 {
                    merkle_root: Hash::new(ROOT),
                },
            }),
        });
        let mut trailing = good.clone();
        trailing.unsigned_tx.push_str("00");
        let cases: Vec<(&str, StorageBuildResponse)> = vec![
            ("foreign sender", honest_build(&other, ROOT, CHAIN_ID, FEE)),
            ("other file", honest_build(&own, [0x6b; 32], CHAIN_ID, FEE)),
            ("other chain", honest_build(&own, ROOT, 2, FEE)),
            ("other fee", honest_build(&own, ROOT, CHAIN_ID, FEE + 1)),
            ("other operation", abandon),
            ("trailing bytes", trailing),
            (
                "not hex",
                StorageBuildResponse {
                    unsigned_tx: "0xzz".into(),
                    ..good.clone()
                },
            ),
            (
                "echoed nonce",
                StorageBuildResponse {
                    nonce: NONCE + 1,
                    ..good.clone()
                },
            ),
            (
                "echoed sender",
                StorageBuildResponse {
                    from: other.to_base58(),
                    ..good.clone()
                },
            ),
            (
                "echoed fee",
                StorageBuildResponse {
                    fee: FEE + 1,
                    ..good.clone()
                },
            ),
            (
                "echoed chain id",
                StorageBuildResponse {
                    chain_id: 2,
                    ..good.clone()
                },
            ),
            (
                "signing hash",
                StorageBuildResponse {
                    signing_hash: format!("0x{}", hex::encode([0u8; 32])),
                    ..good.clone()
                },
            ),
        ];
        for (name, resp) in cases {
            let rpc = MockRpc {
                build: Reply::Ok(resp),
                ..MockRpc::happy()
            };
            let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
            assert!(
                matches!(out, ReassignOutcome::BuildMismatch { .. }),
                "{name}: expected BuildMismatch, got {out:?}"
            );
            assert!(rpc.sent().is_empty(), "{name}: nothing may be sent");
        }
    }

    #[tokio::test]
    async fn rpc_errors_fail_closed_at_every_step() {
        let cases = [
            MockRpc {
                info: Reply::Err("file info down"),
                ..MockRpc::happy()
            },
            MockRpc {
                coverage: Reply::Err("coverage down"),
                ..MockRpc::happy()
            },
            MockRpc {
                build: Reply::Err("builder down"),
                ..MockRpc::happy()
            },
        ];
        for rpc in cases {
            let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
            assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
            assert!(rpc.sent().is_empty());
        }

        let rpc = MockRpc {
            send: Reply::Err("submission refused by the node"),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
    }

    #[tokio::test]
    async fn dry_run_verifies_but_never_signs_or_sends() {
        let rpc = MockRpc::happy();
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        match out {
            ReassignOutcome::DryRun {
                nonce,
                fee,
                signing_hash,
            } => {
                assert_eq!(nonce, NONCE);
                assert_eq!(fee, FEE);
                assert_eq!(
                    signing_hash,
                    honest_build(&addr(&SEED), ROOT, CHAIN_ID, FEE).signing_hash
                );
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        assert_eq!(rpc.built(), 1, "dry run still verifies the built bytes");
        assert!(rpc.sent().is_empty(), "dry run sends nothing");
    }

    #[tokio::test]
    async fn dry_run_still_refuses_mismatched_builder_output() {
        let rpc = MockRpc {
            build: Reply::Ok(honest_build(&addr(&OTHER_SEED), ROOT, CHAIN_ID, FEE)),
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(true)).await;
        assert!(
            matches!(out, ReassignOutcome::BuildMismatch { .. }),
            "got {out:?}"
        );
    }

    #[tokio::test]
    async fn a_chain_refusal_is_reported_with_its_reason() {
        let rpc = MockRpc {
            status: TxStatusV2::Failed {
                block_height: Some(778),
                reason: "Failed(332): reassignment not needed".into(),
            },
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        match out {
            ReassignOutcome::Refused {
                tx_hash,
                block_height,
                reason,
            } => {
                assert_eq!(tx_hash, "0xfeed");
                assert_eq!(block_height, Some(778));
                assert!(reason.contains("332"), "reason keeps the code: {reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dropped_transaction_is_a_failure_not_a_success() {
        let rpc = MockRpc {
            status: TxStatusV2::Dropped,
            ..MockRpc::happy()
        };
        let out = reassign_chunks_v2(&rpc, &SEED, &request(false)).await;
        assert!(matches!(out, ReassignOutcome::Failed { .. }), "got {out:?}");
    }
}
