//! L1 Identity Bridge — derives a libp2p keypair from a SUM Chain Ed25519 seed.
//!
//! Every storage node's libp2p PeerId must be derived from the same Ed25519
//! keypair used for its SUM Chain L1 wallet. This ensures that the node's
//! network identity is cryptographically linked to its on-chain financial identity.
//!
//! The L1 address derivation (from `sum-chain/crates/primitives/src/address.rs:42-48`):
//! ```text
//! Address = blake3(ed25519_pubkey)[12..32]  (last 20 bytes)
//! ```
//!
//! The libp2p PeerId is derived differently (multihash of protobuf-encoded pubkey),
//! but both use the **same underlying Ed25519 keypair**.

use libp2p::identity::{self, ed25519, Keypair};
use libp2p::PeerId;

/// Derive a libp2p keypair from a raw Ed25519 private key seed (32 bytes).
///
/// This seed should come from the user's SUM Chain L1 wallet. The resulting
/// keypair is used for both libp2p network identity and L1 transaction signing.
pub fn keypair_from_seed(seed: &[u8; 32]) -> anyhow::Result<Keypair> {
    let mut seed_copy = *seed;
    let ed_secret = ed25519::SecretKey::try_from_bytes(&mut seed_copy)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 seed: {e}"))?;
    let ed_keypair = ed25519::Keypair::from(ed_secret);
    Ok(identity::Keypair::from(ed_keypair))
}

/// Derive the PeerId from a keypair.
pub fn peer_id_from_keypair(keypair: &Keypair) -> PeerId {
    keypair.public().to_peer_id()
}

/// Derive the SUM Chain L1 Address (20 bytes) from a keypair.
///
/// Matches `Address::from_public_key()` in `sum-chain/crates/primitives/src/address.rs`:
/// `blake3(ed25519_pubkey_32_bytes)[12..32]`
pub fn l1_address_from_keypair(keypair: &Keypair) -> [u8; 20] {
    let ed_pubkey = keypair
        .public()
        .try_into_ed25519()
        .expect("keypair is Ed25519");
    let pubkey_bytes = ed_pubkey.to_bytes();
    let hash = blake3::hash(&pubkey_bytes);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash.as_bytes()[12..32]);
    addr
}

/// Format an L1 address as a hex string (40 lowercase hex chars).
pub fn l1_address_hex(addr: &[u8; 20]) -> String {
    addr.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_keypair() {
        let seed = [42u8; 32];
        let kp1 = keypair_from_seed(&seed).unwrap();
        let kp2 = keypair_from_seed(&seed).unwrap();
        assert_eq!(
            peer_id_from_keypair(&kp1),
            peer_id_from_keypair(&kp2),
            "same seed must produce same PeerId"
        );
    }

    #[test]
    fn different_seeds_different_peerids() {
        let kp1 = keypair_from_seed(&[1u8; 32]).unwrap();
        let kp2 = keypair_from_seed(&[2u8; 32]).unwrap();
        assert_ne!(
            peer_id_from_keypair(&kp1),
            peer_id_from_keypair(&kp2),
            "different seeds must produce different PeerIds"
        );
    }

    #[test]
    fn l1_address_derivation() {
        let seed = [99u8; 32];
        let kp = keypair_from_seed(&seed).unwrap();
        let addr = l1_address_from_keypair(&kp);

        // Verify: blake3(pubkey)[12..32]
        let ed_pubkey = kp.public().try_into_ed25519().unwrap();
        let hash = blake3::hash(&ed_pubkey.to_bytes());
        assert_eq!(&addr[..], &hash.as_bytes()[12..32]);
    }

    #[test]
    fn l1_address_is_20_bytes() {
        let seed = [7u8; 32];
        let kp = keypair_from_seed(&seed).unwrap();
        let addr = l1_address_from_keypair(&kp);
        assert_eq!(addr.len(), 20);
    }

    #[test]
    fn l1_address_hex_format() {
        let addr = [0xAB; 20];
        let hex = l1_address_hex(&addr);
        assert_eq!(hex.len(), 40);
        assert_eq!(hex, "ab".repeat(20));
    }
}
