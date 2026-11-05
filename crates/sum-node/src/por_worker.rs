//! Background PoR (Proof of Retrievability) challenge responder.
//!
//! Polls the L1 for active challenges targeting this node, reads the
//! challenged chunk from disk, generates the Merkle proof, builds and
//! signs a `SubmitStorageProof` transaction, and submits it to the L1.
//!
//! Where the proof comes from:
//!
//! * **The file's Public manifest is held here** (V1 and Public V2): the
//!   chunk is located through the manifest and the Merkle path is rebuilt
//!   from the manifest's chunk hashes.
//! * **No Public manifest** (a Private V2 file, whose manifest this archive
//!   cannot read; or a Public V2 file whose manifest never arrived): the
//!   answer is the leaf and Merkle path recorded when the chunk's V2 Push
//!   was accepted (`ManifestIndex::v2_pushed_proof`). The chunk is located
//!   through the held-index record and its bytes must hash to that leaf.
//!   With no record (a chunk never pushed here, or pushed before these
//!   records existed), no readable chunk, or a mismatch, nothing is sent.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};

use sum_store::manifest_index::ManifestIndex;
use sum_store::store::ChunkStore;
use sum_store::{MerkleTree, SumStore};
use sum_types::rpc_types::ChallengeInfo;

use crate::rpc_client::L1RpcClient;
use crate::tx_builder;

/// Maximum number of responded challenge IDs to track (FIFO eviction).
const MAX_RESPONDED: usize = 500;

/// Background worker that responds to L1 PoR challenges.
pub struct PorWorker {
    rpc: Arc<L1RpcClient>,
    /// Raw Ed25519 seed (32 bytes) for signing transactions.
    ed25519_seed: [u8; 32],
    /// This node's L1 address in base58 format.
    l1_address_base58: String,
    /// How often to poll for challenges.
    poll_interval: Duration,
    /// Challenge IDs we have already responded to (bounded FIFO, avoids double-submission).
    responded: Vec<String>,
    /// Consecutive RPC failures (for exponential backoff).
    consecutive_failures: u32,
}

impl PorWorker {
    pub fn new(
        rpc: Arc<L1RpcClient>,
        ed25519_seed: [u8; 32],
        l1_address_base58: String,
        poll_interval: Duration,
    ) -> Self {
        Self {
            rpc,
            ed25519_seed,
            l1_address_base58,
            poll_interval,
            responded: Vec::new(),
            consecutive_failures: 0,
        }
    }

    /// Run the PoR worker loop indefinitely.
    pub async fn run(
        mut self,
        store: Arc<RwLock<SumStore>>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        info!(
            address = %self.l1_address_base58,
            interval_secs = self.poll_interval.as_secs(),
            "PoR worker started"
        );

        let mut interval = tokio::time::interval(self.poll_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.poll_and_respond(&store).await {
                        Ok(()) => { self.consecutive_failures = 0; }
                        Err(e) => {
                            self.consecutive_failures += 1;
                            let backoff_secs = self.poll_interval.as_secs()
                                * 2u64.pow(self.consecutive_failures.min(5));
                            warn!(
                                %e,
                                backoff_secs,
                                failures = self.consecutive_failures,
                                "PoR poll failed — backing off"
                            );
                            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("PoR worker shutting down — one final poll");
                        let _ = self.poll_and_respond(&store).await;
                        return;
                    }
                }
            }
        }
    }

    async fn poll_and_respond(&mut self, store: &Arc<RwLock<SumStore>>) -> Result<()> {
        let challenges = self
            .rpc
            .get_active_challenges(&self.l1_address_base58)
            .await?;

        if challenges.is_empty() {
            return Ok(());
        }

        info!(count = challenges.len(), "active challenges found");

        for challenge in &challenges {
            if self.responded.contains(&challenge.challenge_id) {
                continue;
            }
            match self.respond_to_challenge(store, challenge).await {
                Ok(()) => {
                    // Bounded FIFO: evict oldest when full.
                    if self.responded.len() >= MAX_RESPONDED {
                        self.responded.remove(0);
                    }
                    self.responded.push(challenge.challenge_id.clone());
                }
                Err(e) => {
                    warn!(
                        challenge_id = %challenge.challenge_id,
                        %e,
                        "failed to respond to challenge"
                    );
                }
            }
        }

        Ok(())
    }

    async fn respond_to_challenge(
        &self,
        store: &Arc<RwLock<SumStore>>,
        challenge: &ChallengeInfo,
    ) -> Result<()> {
        // Parse merkle_root from 0x-hex to [u8; 32].
        let root_hex = challenge
            .merkle_root
            .strip_prefix("0x")
            .unwrap_or(&challenge.merkle_root);
        let root_bytes = hex::decode(root_hex)?;
        if root_bytes.len() != 32 {
            anyhow::bail!("merkle_root is not 32 bytes");
        }
        let mut merkle_root = [0u8; 32];
        merkle_root.copy_from_slice(&root_bytes);

        // Parse challenge_id.
        let cid_hex = challenge
            .challenge_id
            .strip_prefix("0x")
            .unwrap_or(&challenge.challenge_id);
        let cid_bytes = hex::decode(cid_hex)?;
        if cid_bytes.len() != 32 {
            anyhow::bail!("challenge_id is not 32 bytes");
        }
        let mut challenge_id = [0u8; 32];
        challenge_id.copy_from_slice(&cid_bytes);

        let chunk_index = challenge.chunk_index;

        let answer = {
            let store_read = store.read().await;
            build_answer(
                &store_read.manifest_idx,
                &store_read.local,
                &merkle_root,
                chunk_index,
            )?
        }; // Read lock released before the RPC calls.

        self.submit_proof(
            challenge,
            challenge_id,
            merkle_root,
            answer.chunk_hash,
            answer.merkle_path,
        )
        .await
    }

    /// Build, sign and submit the `SubmitStorageProof` for `challenge`.
    async fn submit_proof(
        &self,
        challenge: &ChallengeInfo,
        challenge_id: [u8; 32],
        merkle_root: [u8; 32],
        chunk_hash: [u8; 32],
        merkle_path: Vec<[u8; 32]>,
    ) -> Result<()> {
        let chunk_index = challenge.chunk_index;

        // Fetch nonce and chain_id.
        let nonce = self.rpc.get_nonce(&self.l1_address_base58).await?;
        let chain_id = self.rpc.get_chain_id().await?;

        // Build and sign the transaction.
        let fee: u128 = 1_000_000; // Minimal fee (1M base units).
        let tx_hex = tx_builder::build_submit_proof_tx(
            &self.ed25519_seed,
            chain_id,
            nonce,
            fee,
            challenge_id,
            merkle_root,
            chunk_index,
            chunk_hash,
            merkle_path,
        )?;

        // Submit to L1.
        let result = self.rpc.send_raw_transaction(&tx_hex).await?;
        info!(
            challenge_id = %challenge.challenge_id,
            chunk_index,
            ?result,
            "PoR proof submitted"
        );

        Ok(())
    }
}

/// Where a PoR answer's Merkle path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerSource {
    /// Rebuilt from the file's Public manifest held here.
    PublicManifest,
    /// The leaf and path recorded when the chunk's V2 Push was accepted.
    PushedProof,
}

/// The answer this archive would submit for a challenge on `chunk_index`
/// of `merkle_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorAnswer {
    pub chunk_hash: [u8; 32],
    pub merkle_path: Vec<[u8; 32]>,
    pub source: AnswerSource,
}

/// Build the answer the responder submits for a challenge on `chunk_index`
/// of `merkle_root`, from the local store only, exactly as the responder
/// does: from the file's Public manifest when it is held here, otherwise
/// from the chunk's recorded push proof ([`answer_from_pushed_proof`]).
/// An error means no proof would be sent. It does not check the answer
/// against the chain's verifier; the repair audit does that.
pub fn build_answer(
    manifest_idx: &ManifestIndex,
    local: &ChunkStore,
    merkle_root: &[u8; 32],
    chunk_index: u32,
) -> Result<PorAnswer> {
    let Some(manifest) = manifest_idx.get_by_merkle_root(merkle_root) else {
        // No Public manifest (a Private file): answer from the proof the
        // chunk's V2 Push carried, or not at all.
        let (chunk_hash, merkle_path) =
            answer_from_pushed_proof(manifest_idx, local, merkle_root, chunk_index)?;
        return Ok(PorAnswer {
            chunk_hash,
            merkle_path,
            source: AnswerSource::PushedProof,
        });
    };

    let chunk_cid = manifest_idx
        .chunk_cid(merkle_root, chunk_index)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no chunk at index {chunk_index} for root 0x{}",
                hex::encode(merkle_root)
            )
        })?;

    // Read chunk data from disk.
    let chunk_data = local.get(chunk_cid)?;

    // Compute chunk hash.
    let chunk_hash = *blake3::hash(&chunk_data).as_bytes();

    // Rebuild Merkle tree from manifest hashes and generate proof.
    let leaf_hashes: Vec<blake3::Hash> = manifest
        .chunks
        .iter()
        .map(|c| blake3::Hash::from(c.blake3_hash))
        .collect();
    let tree = MerkleTree::build(&leaf_hashes);
    let proof = tree.generate_proof(chunk_index);
    let merkle_path: Vec<[u8; 32]> = proof.iter().map(|h| *h.as_bytes()).collect();
    Ok(PorAnswer {
        chunk_hash,
        merkle_path,
        source: AnswerSource::PublicManifest,
    })
}

/// The chunk hash and Merkle path for a challenge on a file with no Public
/// manifest here (a Private file), from what the chunk's V2 Push left: the
/// recorded leaf and path, provided the chunk the held index names for
/// `chunk_index` is on disk and hashes to that leaf.
///
/// Fails closed: with no recorded proof (a chunk never pushed here, or
/// pushed before push proofs were recorded), no held-index record, an
/// unreadable chunk, or bytes that do not hash to the recorded leaf, it
/// returns an error and no proof is sent. The bytes check is what makes an
/// answer honest: the chain checks only the leaf and path, which the record
/// alone would let this archive send without holding the chunk.
fn answer_from_pushed_proof(
    manifest_idx: &ManifestIndex,
    local: &ChunkStore,
    merkle_root: &[u8; 32],
    chunk_index: u32,
) -> Result<([u8; 32], Vec<[u8; 32]>)> {
    let root = hex::encode(merkle_root);
    let proof = manifest_idx
        .v2_pushed_proof(merkle_root, chunk_index)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not answering: no Public manifest and no recorded push proof for chunk \
                 {chunk_index} of root 0x{root} (chunk not pushed to this archive, or pushed \
                 before push proofs were recorded)"
            )
        })?;
    let cid = manifest_idx
        .v2_held_records(merkle_root)
        .and_then(|records| records.get(&chunk_index))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not answering: chunk {chunk_index} of root 0x{root} has a recorded push proof \
                 but no held-index record"
            )
        })?;
    let bytes = local.get(cid).map_err(|e| {
        anyhow::anyhow!(
            "not answering: chunk {chunk_index} of root 0x{root} ({cid}) unreadable: {e}"
        )
    })?;
    if *blake3::hash(&bytes).as_bytes() != proof.leaf {
        anyhow::bail!(
            "not answering: chunk {chunk_index} of root 0x{root} ({cid}) does not hash to its \
             recorded push leaf"
        );
    }
    Ok((proof.leaf, proof.path.clone()))
}

/// Test-only entry points into the responder, for the PoR tests in
/// `inbound_v2.rs`, which store files through the real push handlers and
/// then challenge them.
#[cfg(test)]
impl PorWorker {
    pub(crate) async fn respond_to_challenge_for_test(
        &self,
        store: &Arc<RwLock<SumStore>>,
        challenge: &ChallengeInfo,
    ) -> Result<()> {
        self.respond_to_challenge(store, challenge).await
    }

    pub(crate) async fn poll_and_respond_for_test(
        &mut self,
        store: &Arc<RwLock<SumStore>>,
    ) -> Result<()> {
        self.poll_and_respond(store).await
    }

    pub(crate) fn responded_for_test(&self) -> &[String] {
        &self.responded
    }
}
