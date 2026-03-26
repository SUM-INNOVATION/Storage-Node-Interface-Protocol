# Phase 2: sum-net — New Test Details

**Crate:** `crates/sum-net/`
**New tests in Phase 2:** 4 (added to existing 9 from Phase 1, total 13)
**Result:** ALL PASS

All new tests are in `crates/sum-net/src/identity.rs`, extending the identity module with base58 encoding and peer-to-address mapping.

---

## identity.rs — New Tests (4)

**Source:** `crates/sum-net/src/identity.rs`

### 1. `base58_round_trip`

**Purpose:** Encode an L1 address as base58 with checksum, then decode it back, verifying the original bytes are recovered.

```rust
#[test]
fn base58_round_trip() {
    let seed = [55u8; 32];
    let kp = keypair_from_seed(&seed).unwrap();
    let addr = l1_address_from_keypair(&kp);
    let encoded = l1_address_base58(&addr);
    let decoded = l1_address_from_base58(&encoded).unwrap();
    assert_eq!(addr, decoded);
}
```

**Output:** `ok`
**Why it matters:** The RPC client sends addresses in base58 format to the L1. If encoding or decoding is wrong, every RPC call with an address parameter would target the wrong account. The checksum algorithm (`blake3(blake3(addr))[0..4]`) must match the L1's `Address::to_base58()` at `sum-chain/crates/primitives/src/address.rs:78-87`.

---

### 2. `base58_bad_checksum_fails`

**Purpose:** A corrupted base58 string (last character changed) must fail checksum verification.

```rust
#[test]
fn base58_bad_checksum_fails() {
    let seed = [55u8; 32];
    let kp = keypair_from_seed(&seed).unwrap();
    let addr = l1_address_from_keypair(&kp);
    let mut encoded = l1_address_base58(&addr);
    // Corrupt the last character
    let last = encoded.pop().unwrap();
    encoded.push(if last == '1' { '2' } else { '1' });
    assert!(l1_address_from_base58(&encoded).is_err());
}
```

**Output:** `ok`
**Why it matters:** The 4-byte blake3 double-hash checksum exists to catch typos and corruption. This test verifies that even a single-character change is detected.

---

### 3. `base58_wrong_length_fails`

**Purpose:** A base58 string that decodes to the wrong byte count (not 24 = 20 address + 4 checksum) must fail.

```rust
#[test]
fn base58_wrong_length_fails() {
    assert!(l1_address_from_base58("abc").is_err());
}
```

**Output:** `ok`
**Why it matters:** Prevents accidental use of non-address strings (e.g., a PeerId or random text) as L1 addresses.

---

### 4. `l1_address_from_peer_pubkey`

**Purpose:** Verifies that deriving an L1 address from a libp2p public key produces the same result as deriving it directly from the keypair.

```rust
#[test]
fn l1_address_from_peer_pubkey() {
    let seed = [77u8; 32];
    let kp = keypair_from_seed(&seed).unwrap();
    let addr_direct = l1_address_from_keypair(&kp);
    let addr_from_pubkey = l1_address_from_peer_public_key(&kp.public()).unwrap();
    assert_eq!(addr_direct, addr_from_pubkey);
}
```

**Output:** `ok`
**Why it matters:** This is the function used by the ACL interceptor. When the libp2p identify protocol reveals a peer's public key, we derive their L1 address from it to check the on-chain access list. If this derivation doesn't match `l1_address_from_keypair()`, the ACL would deny legitimate peers. Both paths must produce `blake3(ed25519_pubkey)[12..32]`.
