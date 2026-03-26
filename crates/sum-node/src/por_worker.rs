//! Background PoR (Proof of Retrievability) challenge responder.
//!
//! Polls the L1 for active challenges targeting this node, reads the
//! challenged chunk from disk, generates the Merkle proof, builds and
//! signs a `SubmitStorageProof` transaction, and submits it to the L1.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};

use sum_store::{MerkleTree, SumStore};
use sum_types::rpc_types::ChallengeInfo;

use crate::rpc_client::L1RpcClient;
use crate::tx_builder;

/// Background worker that responds to L1 PoR challenges.
pub struct PorWorker {
    rpc: Arc<L1RpcClient>,
    /// Raw Ed25519 seed (32 bytes) for signing transactions.
    ed25519_seed: [u8; 32],
    /// This node's L1 address in base58 format.
    l1_address_base58: String,
    /// How often to poll for challenges.
    poll_interval: Duration,
    /// Challenge IDs we have already responded to (avoids double-submission).
    responded: HashSet<String>,
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
            responded: HashSet::new(),
        }
    }

    /// Run the PoR worker loop indefinitely.
    pub async fn run(mut self, store: Arc<RwLock<SumStore>>) {
        info!(
            address = %self.l1_address_base58,
            interval_secs = self.poll_interval.as_secs(),
            "PoR worker started"
        );

        let mut interval = tokio::time::interval(self.poll_interval);
        loop {
            interval.tick().await;
            if let Err(e) = self.poll_and_respond(&store).await {
                warn!(%e, "PoR poll cycle failed");
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
                    self.responded.insert(challenge.challenge_id.clone());
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

        // Clean up old responded entries (keep set bounded).
        if self.responded.len() > 1000 {
            self.responded.clear();
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

        // Look up manifest and chunk CID.
        let store_read = store.read().await;
        let manifest = store_read
            .manifest_idx
            .get_by_merkle_root(&merkle_root)
            .ok_or_else(|| anyhow::anyhow!("no manifest for merkle root {}", challenge.merkle_root))?;

        let chunk_cid = store_read
            .manifest_idx
            .chunk_cid(&merkle_root, chunk_index)
            .ok_or_else(|| {
                anyhow::anyhow!("no chunk at index {chunk_index} for root {}", challenge.merkle_root)
            })?
            .to_string();

        // Read chunk data from disk.
        let chunk_data = store_read.local.get(&chunk_cid)?;

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

        drop(store_read); // Release read lock before RPC calls.

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
