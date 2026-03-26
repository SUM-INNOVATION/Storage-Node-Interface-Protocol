//! ACL (Access Control List) enforcement for chunk serving.
//!
//! Before serving a chunk to a peer, queries the L1's `storage_getAccessList`
//! to verify the requester is authorized. Maps PeerId -> L1 Address using
//! the libp2p identify protocol's exchanged public keys.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use sum_net::PeerId;
use tokio::sync::RwLock;
use tracing::warn;

use sum_net::identity;
use sum_store::ManifestIndex;

use crate::rpc_client::L1RpcClient;

/// Checks whether a peer is allowed to retrieve a chunk.
pub struct AclChecker {
    rpc: Arc<L1RpcClient>,
    /// PeerId -> L1 Address mapping, populated by `PeerIdentified` events.
    peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
}

impl AclChecker {
    pub fn new(
        rpc: Arc<L1RpcClient>,
        peer_addresses: Arc<RwLock<HashMap<PeerId, [u8; 20]>>>,
    ) -> Self {
        Self { rpc, peer_addresses }
    }

    /// Check if `peer_id` is allowed to retrieve the chunk identified by `cid`.
    ///
    /// Returns:
    /// - `Ok(true)` — access allowed (public file, or peer is in ACL)
    /// - `Ok(false)` — access denied
    /// - `Err(_)` — RPC failure (caller should decide whether to allow or deny)
    pub async fn check_access(
        &self,
        peer_id: &PeerId,
        cid: &str,
        manifest_index: &ManifestIndex,
    ) -> Result<bool> {
        // 1. Find which file (merkle_root) this CID belongs to.
        let Some(root) = manifest_index.merkle_root_for_cid(cid) else {
            // Unknown CID — not tracked in our manifest index.
            // Allow it (could be a locally-managed chunk not registered on L1).
            return Ok(true);
        };

        // 2. Query L1 for the file's access list.
        let root_hex = format!(
            "0x{}",
            root.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let file_info = self.rpc.get_access_list(&root_hex).await?;

        let Some(info) = file_info else {
            // File not registered on L1 — allow (pre-registration state).
            return Ok(true);
        };

        // 3. If access_list is empty, the file is public.
        if info.access_list.is_empty() {
            return Ok(true);
        }

        // 4. Look up the requester's L1 address.
        let peer_addr = {
            let map = self.peer_addresses.read().await;
            map.get(peer_id).copied()
        };
        let Some(addr) = peer_addr else {
            warn!(
                %peer_id,
                "ACL check: peer's L1 address unknown (identify not yet received) — denying"
            );
            return Ok(false);
        };

        // 5. Check if the peer's address is in the access list.
        let addr_base58 = identity::l1_address_base58(&addr);
        Ok(info.access_list.contains(&addr_base58))
    }
}
