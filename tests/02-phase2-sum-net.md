# sum-net — Test Details

**Crate:** `crates/sum-net/`
**Tests:** 9
**Result:** ALL PASS

---

## identity.rs (5 tests)

**Source:** `crates/sum-net/src/identity.rs`

### 1. `deterministic_keypair`

**Purpose:** Same 32-byte seed must always produce the same PeerId.

```rust
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
```

**Output:** `ok`
**Why it matters:** The node must produce the same PeerId across restarts so the L1 can consistently identify it.

---

### 2. `different_seeds_different_peerids`

**Purpose:** Different seeds must produce different PeerIds (no collisions).

```rust
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
```

**Output:** `ok`

---

### 3. `l1_address_derivation`

**Purpose:** Verifies that our L1 address derivation matches the formula used on-chain: `blake3(pubkey)[12..32]`.

```rust
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
```

**Output:** `ok`
**Why it matters:** This is the critical L1 alignment test. If the address derivation doesn't match `sum-chain/crates/primitives/src/address.rs:42-48`, the node's network identity won't correspond to its on-chain identity, and it cannot respond to PoR challenges.

---

### 4. `l1_address_is_20_bytes`

**Purpose:** L1 addresses are exactly 20 bytes (matching Ethereum-style addressing).

```rust
#[test]
fn l1_address_is_20_bytes() {
    let seed = [7u8; 32];
    let kp = keypair_from_seed(&seed).unwrap();
    let addr = l1_address_from_keypair(&kp);
    assert_eq!(addr.len(), 20);
}
```

**Output:** `ok`

---

### 5. `l1_address_hex_format`

**Purpose:** Hex encoding of a 20-byte address produces exactly 40 lowercase hex characters.

```rust
#[test]
fn l1_address_hex_format() {
    let addr = [0xAB; 20];
    let hex = l1_address_hex(&addr);
    assert_eq!(hex.len(), 40);
    assert_eq!(hex, "ab".repeat(20));
}
```

**Output:** `ok`

---

## codec.rs (4 tests)

**Source:** `crates/sum-net/src/codec.rs`

### 6. `request_round_trip`

**Purpose:** A `ShardRequest` written to a byte stream can be read back identically.

```rust
#[tokio::test]
async fn request_round_trip() {
    let mut codec = ShardCodec::default();
    let req = ShardRequest {
        cid: "bafkr4itest".into(),
        offset: Some(1024),
        max_bytes: Some(65536),
    };

    let mut buf = Vec::new();
    codec.write_request(&String::new(), &mut Cursor::new(&mut buf), req.clone()).await.unwrap();
    let decoded = codec.read_request(&String::new(), &mut Cursor::new(&buf)).await.unwrap();

    assert_eq!(decoded.cid, req.cid);
    assert_eq!(decoded.offset, req.offset);
    assert_eq!(decoded.max_bytes, req.max_bytes);
}
```

**Output:** `ok`
**Why it matters:** Validates the `[u32 BE length][bincode payload]` wire format used by the `/sum/storage/v1` protocol.

---

### 7. `response_round_trip`

**Purpose:** A `ShardResponse` with 4096 bytes of payload data survives wire encoding/decoding.

```rust
#[tokio::test]
async fn response_round_trip() {
    let mut codec = ShardCodec::default();
    let resp = ShardResponse {
        cid: "bafkr4itest".into(),
        offset: 0,
        total_bytes: 1_000_000,
        data: vec![0xAB; 4096],
        error: None,
    };

    let mut buf = Vec::new();
    codec.write_response(&String::new(), &mut Cursor::new(&mut buf), resp.clone()).await.unwrap();
    let decoded = codec.read_response(&String::new(), &mut Cursor::new(&buf)).await.unwrap();

    assert_eq!(decoded.cid, resp.cid);
    assert_eq!(decoded.data.len(), 4096);
    assert_eq!(decoded.data[0], 0xAB);
    assert!(decoded.error.is_none());
}
```

**Output:** `ok`

---

### 8. `error_response_round_trip`

**Purpose:** Error responses (empty data, error message set) round-trip correctly.

```rust
#[tokio::test]
async fn error_response_round_trip() {
    let mut codec = ShardCodec::default();
    let resp = ShardResponse {
        cid: "bafkr4imissing".into(),
        offset: 0, total_bytes: 0,
        data: Vec::new(),
        error: Some("shard not found".into()),
    };

    let mut buf = Vec::new();
    codec.write_response(&String::new(), &mut Cursor::new(&mut buf), resp.clone()).await.unwrap();
    let decoded = codec.read_response(&String::new(), &mut Cursor::new(&buf)).await.unwrap();

    assert_eq!(decoded.error.as_deref(), Some("shard not found"));
    assert!(decoded.data.is_empty());
}
```

**Output:** `ok`

---

### 9. `rejects_oversized_message`

**Purpose:** Messages exceeding the size limit are rejected with an error (prevents memory exhaustion attacks).

```rust
#[tokio::test]
async fn rejects_oversized_message() {
    let mut codec = ShardCodec { max_msg_bytes: 16 }; // tiny limit for test

    let mut buf = Vec::new();
    buf.extend_from_slice(&1000u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 1000]);

    let result = codec.read_request(&String::new(), &mut Cursor::new(&buf)).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("message too large"));
}
```

**Output:** `ok`
**Why it matters:** Production safety limit is 256 MiB. This test verifies the enforcement mechanism works.
