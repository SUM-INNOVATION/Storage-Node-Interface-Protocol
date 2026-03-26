Here is the complete architectural structure and phased implementation plan for the SUM Chain Native Decentralized Storage Protocol.
This design keeps the Layer-1 state trie lightweight by ensuring no actual file data touches the blockchain. The chain acts strictly as the cryptographic source of truth, the financial settlement layer, and the access control enforcer.

Part 1: Architectural Structure (How It Looks)
The architecture is divided into the On-Chain State (the metadata and rules) and the Off-Chain Mesh (the physical byte storage).
1. Native State Trie Objects (On-Chain)
These structures are compiled directly into the SUM Chain Rust node binary.
    - NodeRegistry: Tracks the network's hardware.
        - address: Ed25519 Identity.
        - role: StorageNode (Requires a minimum staked balance of native Koppa).
        - status: Active, Challenged, Slashed.
    - StorageMetadata: Replaces traditional smart contracts. Maps a file's identity to its rules.
        - merkle_root: The BLAKE3 top hash of the file's chunk DAG.
        - file_size: Total bytes.
        - access_list: Vector of allowed Ed25519 addresses (empty = public).
        - storage_fee_pool: Locked Koppa to pay nodes over time.
2. The File Ingestion Pipeline (Client-Side to Network)
    1. Encrypt (Optional): If the file is private, encrypt locally via XChaCha20-Poly1305.
    2. Chunk & Hash: Split the file into uniform blocks (e.g., 1 MB). Hash each block with BLAKE3 to create leaf nodes.
    3. Merkle DAG: Build the Merkle Tree. The resulting merkle_root becomes the file's absolute network identity.
    4. Allocate Transaction: The client broadcasts a TxPayload::AllocateStorage transaction to the Validators, containing the merkle_root, the access_list, and the storage_fee_pool.
3. The Combinatorial Storage Mesh (Off-Chain P2P)
Once the metadata is on-chain, the client pushes the actual chunks to the Storage Nodes via a dedicated libp2p protocol (e.g., /sum/storage/v1).
    - Unique State Routing: The network uses combinatorial math. If a file has 4 chunks and the replication factor is 3, the network needs to store 12 chunk copies. The protocol assigns these 12 copies across the nodes such that no node shares the exact same state matrix.
    - Peer-to-Peer Gossiping: Storage nodes use gossipsub to announce which chunks they have successfully acquired.
4. The L1 Security Engine (Proof of Retrievability - PoR)
    - The Audit: Every N blocks, the active Validator uses the previous block's hash as a random seed to select a specific StorageMetadata entry and a specific chunk index.
    - The Challenge: The Validator emits an on-chain event challenging the assigned Storage Nodes.
    - The Proof: The Storage Nodes must submit a TxPayload::SubmitStorageProof containing the chunk's BLAKE3 hash and the Merkle sibling path to the root.
    - Settlement: The native StorageExecutor verifies the Merkle path. Pass = reward distributed from the storage_fee_pool. Fail = stake slashed.

Part 2: Phased Implementation Plan
This roadmap is designed for a core Rust blockchain engineering team to build the protocol from the ground up without breaking consensus.

Phase 1: Cryptography & State Foundation (Weeks 1-3) [COMPLETED]
Goal: Define the native state and build the client-side file chunker.
    1. Core State Types: Implement the NodeRegistry and StorageMetadata structs in the blockchain's state definitions. Ensure they are correctly serialized into the state trie.
    2. Merkle Builder (CLI/SDK): Build a standalone Rust utility that takes a file, chunks it into 1 MB slices, calculates the BLAKE3 leaf hashes, and generates the Merkle Root and sibling paths.
    3. Transaction Payloads: Define TxPayload::AllocateStorage and TxPayload::RegisterStorageNode.
    4. L1 Identity Bridge: Derive libp2p PeerId from the same Ed25519 seed used for the L1 wallet, ensuring network identity matches on-chain identity.

Phase 2: L1 RPC Bridge, ACL Enforcement & PoR Responder (Weeks 4-8) [CURRENT]
Goal: Connect the storage node to the L1 blockchain, enforce access control, and automate proof-of-retrievability responses so nodes earn Koppa. This phase merges the original Phases 2 and 4 because the RPC client and PoR responder are tightly coupled -- you can't test the RPC client without something consuming it, and the PoR responder is its primary consumer.
    1. RPC Client: Build a JSON-RPC client (reqwest) connecting to a local/remote SUM Chain L1 node. Bind endpoints: storage_getFundedFiles, storage_getActiveChallenges, storage_getAccessList, storage_getNodeRecord, send_raw_transaction, get_nonce, chain_id.
    2. Manifest Index: Build a persistent merkle_root -> DataManifest index so the node can locate chunks when challenged by the L1.
    3. Transaction Builder: Construct and sign SubmitStorageProof transactions using bincode v1 mirror types matching the L1's exact serialization format (TransactionV2, TxPayload variant index 18, SignedTransaction with Ed25519 signature).
    4. PoR Challenge Monitor: Background async loop polling storage_getActiveChallenges(self_address), reading challenged chunks from disk, rebuilding MerkleTree from manifest hashes, generating merkle_path proofs, signing and submitting proof transactions to the L1.
    5. ACL Interceptor: Before serving a chunk to a peer, query storage_getAccessList on the L1. If access_list is non-empty and requester's L1 address is not in it, deny the request. Map PeerId -> L1 Address via libp2p identify protocol's exchanged public keys.
    6. PeerId-to-Address Mapping: Use libp2p identify (already in the behaviour) to learn peers' Ed25519 public keys, then derive their L1 addresses via blake3(pubkey)[12..32]. Emit PeerIdentified events for the ACL checker.

Phase 3: The Combinatorial P2P Mesh (Weeks 9-12)
Goal: Nodes can deterministically route and distribute file chunks across the network.
    1. Combinatorial Assignment Logic: Write the algorithm that maps a file's Merkle Root and chunk count to a specific subset of active Storage Nodes, enforcing the rule that configuration states must be unique (e.g., calculating C(C,m)).
    2. Market Sync: Use storage_getFundedFiles to discover files the node should be storing based on its assignment, and automatically fetch assigned chunks from peers.

Phase 4: The "Scale-Out" Upgrade (Future)
Goal: Transition from pure replication to Erasure Coding.
    1. Reed-Solomon Integration: Replace the pure 3x chunk replication with a Reed-Solomon encoding library.
    2. Protocol Upgrade: Update the Merkle Builder to hash shards (data + parity) instead of raw chunks. The rest of the on-chain metadata, ACL, and PoR systems remain completely unchanged, but the network storage overhead drops by 60-80%.

sum-storage-node/
|
+-- Cargo.toml                          # Workspace manifest
+-- rust-toolchain.toml                 # Rust 2024 edition, MSRV 1.85+
+-- README.md                           # Protocol spec for SUM Chain native storage
|
+-- crates/
    +-- sum-types/                      # Crate 1: Core Definitions
    |   +-- src/
    |       +-- lib.rs
    |       +-- storage.rs              # ChunkDescriptor, DataManifest, CHUNK_SIZE
    |       +-- rpc_types.rs            # NEW (Phase 2): RPC response types
    |       +-- config.rs              # NetConfig, StoreConfig, RpcConfig
    |       +-- node.rs                 # NodeCapability types
    |       +-- error.rs                # SumError
    |
    +-- sum-net/                        # Crate 2: The P2P Mesh (libp2p)
    |   +-- src/
    |       +-- lib.rs                  # SumNet API handle
    |       +-- behaviour.rs            # mDNS, Gossipsub, Request-Response
    |       +-- identity.rs             # Ed25519 SUM Chain Wallet Integration + Base58
    |       +-- codec.rs                # Custom /sum/storage/v1 transfer protocol
    |       +-- gossip.rs               # Chunk availability announcements
    |       +-- swarm.rs                # Swarm event loop + PeerIdentified events
    |       +-- events.rs               # SumNetEvent enum
    |       +-- discovery.rs            # mDNS peer discovery
    |
    +-- sum-store/                      # Crate 3: File I/O & Merkle Math
    |   +-- src/
    |       +-- lib.rs
    |       +-- chunker.rs              # Generic binary file slicer
    |       +-- merkle.rs               # BLAKE3 Merkle DAG builder & proof generator
    |       +-- manifest_index.rs       # NEW (Phase 2): merkle_root -> manifest lookup
    |       +-- store.rs                # On-disk content-addressed chunk store
    |       +-- fetch.rs                # Windowed chunk download orchestration
    |       +-- serve.rs                # Inbound chunk request handler
    |       +-- verify.rs               # BLAKE3, CID, and Merkle proof verification
    |       +-- manifest.rs             # CBOR manifest serialization
    |       +-- content_id.rs           # BLAKE3 -> CIDv1
    |       +-- mmap.rs                 # Zero-copy memory-mapped file I/O
    |       +-- announce.rs             # ChunkAnnouncement gossipsub messages
    |
    +-- sum-node/                       # Crate 4: CLI Entry Point
        +-- src/
            +-- main.rs                 # CLI: listen, ingest, fetch, send
            +-- rpc_client.rs           # NEW (Phase 2): JSON-RPC client to L1
            +-- tx_builder.rs           # NEW (Phase 2): Transaction construction + signing
            +-- por_worker.rs           # NEW (Phase 2): Background PoR challenge responder
            +-- acl.rs                  # NEW (Phase 2): ACL check before serving chunks

What We Deleted (The "Anti-Features")
To make this pure storage, we dropped several heavy components from the OmniNode repo:
    - omni-bridge & python/ directory: Gone. No Python, PyO3, or NumPy needed. 100% native Rust.
    - omni-pipeline: Gone. No micro-batch scheduling, GPipe, or hidden-state tensor transfers.
    - gguf.rs: Gone. The parser is removed. sum-store treats everything as raw bytes.

The Core Engineering Focuses for Team B
    1. The merkle.rs Engine (in sum-store): Builds the Merkle DAG constructor. Takes raw files, chunks them, hashes leaves with BLAKE3, outputs the MerkleRoot (sent to L1) and individual chunk proofs (for PoR audits). [DONE]
    2. The identity.rs Bridge (in sum-net): Custom identity provider taking a SUM Chain Ed25519 private key seed and deriving the PeerId. Every storage node's network identity is tied to its on-chain financial identity. [DONE]
    3. The rpc_client.rs (in sum-node): Lightweight JSON-RPC client to talk to the SUM Chain Validator node. Enables ACL checks, challenge polling, and proof submission. [Phase 2]
    4. The por_worker.rs (in sum-node): Background loop that polls for PoR challenges, generates Merkle proofs, and submits signed transactions back to the L1 to earn Koppa. [Phase 2]
    5. The tx_builder.rs (in sum-node): Constructs transactions with bincode v1 mirror types matching the L1's exact byte layout, signs with Ed25519, and hex-encodes for RPC submission. [Phase 2]
