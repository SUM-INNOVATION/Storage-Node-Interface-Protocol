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

Phase 2: L1 RPC Bridge, ACL Enforcement & PoR Responder (Weeks 4-8) [COMPLETED]
Goal: Connect the storage node to the L1 blockchain, enforce access control, and automate proof-of-retrievability responses so nodes earn Koppa. This phase merges the original Phases 2 and 4 because the RPC client and PoR responder are tightly coupled -- you can't test the RPC client without something consuming it, and the PoR responder is its primary consumer.
    1. RPC Client: Build a JSON-RPC client (reqwest) connecting to a local/remote SUM Chain L1 node. Bind endpoints: storage_getFundedFiles, storage_getActiveChallenges, storage_getAccessList, storage_getNodeRecord, send_raw_transaction, get_nonce, chain_id.
    2. Manifest Index: Build a persistent merkle_root -> DataManifest index so the node can locate chunks when challenged by the L1.
    3. Transaction Builder: Construct and sign SubmitStorageProof transactions using bincode v1 mirror types matching the L1's exact serialization format (TransactionV2, TxPayload variant index 18, SignedTransaction with Ed25519 signature).
    4. PoR Challenge Monitor: Background async loop polling storage_getActiveChallenges(self_address), reading challenged chunks from disk, rebuilding MerkleTree from manifest hashes, generating merkle_path proofs, signing and submitting proof transactions to the L1.
    5. ACL Interceptor: Before serving a chunk to a peer, query storage_getAccessList on the L1. If access_list is non-empty and requester's L1 address is not in it, deny the request. Map PeerId -> L1 Address via libp2p identify protocol's exchanged public keys.
    6. PeerId-to-Address Mapping: Use libp2p identify (already in the behaviour) to learn peers' Ed25519 public keys, then derive their L1 addresses via blake3(pubkey)[12..32]. Emit PeerIdentified events for the ACL checker.

Phase 3: The Combinatorial P2P Mesh (Weeks 9-12) [COMPLETED]
Goal: Nodes can deterministically route and distribute file chunks across the network with 3x replication.
    1. REPLICATION_FACTOR = 3: Added to both L1 (primitives) and off-chain (sum-types). Each chunk is stored on exactly min(3, N) nodes.
    2. Deterministic Assignment Algorithm: blake3(merkle_root ++ chunk_index ++ replica) maps each chunk replica to a position in the sorted active node list, with linear probing on collision. Identical implementation in both L1 (state/storage_metadata.rs) and off-chain (sum-store/assignment.rs).
    3. L1 generate_challenge() Update: Now only challenges nodes that are assigned to the selected file, instead of any random active node. Uses the assignment algorithm to compute eligible targets.
    4. storage_getActiveNodes() RPC: New L1 endpoint returning all active ArchiveNodes sorted by address bytes. Used by off-chain nodes to compute the same assignment.
    5. Manifest Exchange Protocol: serve.rs handles "manifest:" prefixed requests, serving CBOR-serialized DataManifest data. SumNet.request_manifest() convenience method added.
    6. MarketSyncWorker: Background task polling L1 for funded files, computing assignments, fetching missing manifests and chunks from peers. Runs alongside PorWorker.
    7. CLI: --market-sync-secs flag (default 30s, env SUM_MARKET_SYNC_INTERVAL).

Phase 4: Scale-Out & Production Readiness [IN PROGRESS — see docs/PHASE4-EXECUTION-PLAN.md]
Goal: Close remaining gaps, improve storage efficiency, enable WAN connectivity, and harden for production.
    1. Resilient Upload: Push protocol (push_data on ShardRequest) + UploadOrchestrator that pushes to R=3 assigned nodes with ACK confirmation. run_ingest() calls UploadOrchestrator::run(). In client mode (--client), Alice exits after R confirmations. In node mode (default), enters serve loop after upload. [DONE]
    2. File Download Command: sum-node download <merkle_root> --output <path> — manifest fetch, parallel chunk download (max 10 concurrent), CID verification, merkle root verification, file reassembly. [DONE]
    3. Garbage Collection: GarbageCollector with mark-and-sweep, configurable grace period (--gc-grace-secs, default 1 hour), L1-reachability safety guard (pauses if last poll > 5 min). Integrated into MarketSyncWorker. Entry nodes (the R=3 nodes Alice initially pushes to) are NOT treated specially — if the assignment recomputes and an entry node is no longer assigned to a chunk, GC deletes it after the grace period. This is safe because the MarketSync cycle (30s) fetches data to new assignees well before the GC grace period (1 hour) expires. No chunk provenance metadata is stored. [DONE]
    4. Reed-Solomon Erasure Coding: Replace pure 3x chunk replication with coded redundancy (k=4 data + m=2 parity shards). Storage overhead drops from 3x to 1.5x. Requires L1 changes to challenge on shard indices. [NOT STARTED]
    5. WAN Discovery: Kademlia DHT + AutoNAT + DCUtR for internet-wide peer discovery and NAT traversal. Currently only mDNS (LAN-only). [NOT STARTED]
    6. Production Hardening: Prometheus metrics, graceful shutdown, health checks, full E2E integration test against live validators. [NOT STARTED]

Phase 5: Client-Side Encryption for Private Files [NOT STARTED — see docs/SECURITY-ANALYSIS.md]
Goal: Make data confidentiality independent of node honesty. Nodes store ciphertext, never plaintext.
    The current ACL (access_list on StorageMetadata) is enforced off-chain by node code. A malicious node
    operator can bypass the check and read/serve plaintext data. Client-side encryption (XChaCha20-Poly1305)
    before chunking eliminates this — nodes hold ciphertext that is useless without the decryption key.
    The ACL becomes a bandwidth optimization, not the security boundary.
    See docs/SECURITY-ANALYSIS.md for full analysis of three approaches (deterministic+ACL, opt-in, encryption)
    and justification for why encryption is the correct layer.

What Can Be Tested Right Now
    1. cargo test --workspace — 90 unit tests, no setup needed
    2. bash tests/integration/test_p2p.sh — automated 2-node chunk transfer on localhost
    3. Two-terminal ingest + download — ingest on Terminal A, download <merkle_root> --output on Terminal B, diff to verify byte-identical output
    4. LAN test — same as above across two computers on the same WiFi (mDNS discovers peers automatically)
    5. VPN/Tailscale LAN test — mDNS may work over Tailscale mesh networks, enabling testing across different physical networks without WAN support

What Cannot Be Tested Without a Running L1
    1. PoR challenge/response loop (Steps 5-7) — needs validators generating challenges every 100 blocks
    2. Market sync auto-fetch of assigned chunks (Step 4) — needs storage_getFundedFiles + storage_getActiveNodes RPC
    3. ACL enforcement on private files (Step 8b) — needs storage_getAccessList RPC
    4. GC in production conditions — needs assignment from live on-chain state
    5. Upload orchestrator with real R=3 assignment — needs storage_getActiveNodes from L1

What Cannot Be Tested Without WAN Support (Phase 4, Objective 5)
    - Two computers on different networks (different WiFi, different cities, over the internet)
    - The only peer discovery mechanism is mDNS, which broadcasts on the local network only
    - WAN requires Kademlia DHT bootstrap nodes — not implemented
    - Workaround: Tailscale or similar VPN creates a virtual LAN where mDNS works across physical networks

Known Gaps (Current State)
    1. Client mode starts a full swarm: The --client flag makes run_ingest() exit after upload (instead of serving forever), but still starts a full libp2p swarm with gossipsub subscriptions. A true lightweight client that is outbound-only (no listening port, no gossipsub) is a future optimization. See docs/CLIENT-MODE-GAP.md Phase 3.
    2. Steps 4-7 untested against live L1: The PoR loop (market sync, challenge polling, proof generation, settlement) is implemented but has never been tested end-to-end against running sum-chain validators.
    3. Client-side encryption for private files: Nodes hold plaintext on disk. ACL is enforced off-chain by node code and can be bypassed by a modified binary. See docs/SECURITY-ANALYSIS.md.
        - This is the architecture described in the README: Alice and Bob are external users, not infrastructure operators.

sum-storage-node/
|
+-- Cargo.toml                          # Workspace manifest
+-- rust-toolchain.toml                 # Rust 2024 edition, MSRV 1.85+
+-- README.md                           # Full protocol walkthrough with diagrams
+-- PLAN.md                             # This file
+-- docs/
    +-- PHASE4-EXECUTION-PLAN.md        # Detailed Phase 4 test specs & objectives
    +-- PHASE4-IMPLEMENTATION-PLAN.md   # Phase 4 implementation plan (approved)
    +-- diagrams/                       # SVG diagrams for README (step0-step8)
|
+-- crates/
    +-- sum-types/                      # Crate 1: Core Definitions
    |   +-- src/
    |       +-- lib.rs
    |       +-- storage.rs              # ChunkDescriptor, DataManifest, CHUNK_SIZE, REPLICATION_FACTOR
    |       +-- rpc_types.rs            # RPC response types (StorageFileInfo, ChallengeInfo, NodeRecordInfo)
    |       +-- config.rs               # NetConfig, StoreConfig, RpcConfig
    |       +-- node.rs                 # NodeCapability types
    |       +-- error.rs                # SumError
    |
    +-- sum-net/                        # Crate 2: The P2P Mesh (libp2p)
    |   +-- src/
    |       +-- lib.rs                  # SumNet API handle
    |       +-- behaviour.rs            # mDNS, Gossipsub, Identify, Request-Response
    |       +-- identity.rs             # Ed25519 SUM Chain Wallet Integration + Base58 + L1 Address
    |       +-- codec.rs                # Custom /sum/storage/v1 transfer protocol
    |       +-- gossip.rs               # Chunk availability announcements (sum/storage/v1 topic)
    |       +-- swarm.rs                # Swarm event loop + PeerIdentified events
    |       +-- events.rs               # SumNetEvent enum
    |       +-- discovery.rs            # mDNS peer discovery
    |       +-- capability.rs           # [DEFERRED] WAN capability advertisement
    |       +-- nat.rs                  # [DEFERRED] AutoNAT / DCUtR / Relay
    |       +-- transport.rs            # [DEFERRED] TCP/Noise fallback transport
    |
    +-- sum-store/                      # Crate 3: File I/O & Merkle Math
    |   +-- src/
    |       +-- lib.rs                  # SumStore top-level API (ingest_file, announce_chunks)
    |       +-- chunker.rs              # Generic BinaryChunker (any file -> 1 MB chunks)
    |       +-- merkle.rs               # BLAKE3 Merkle DAG builder & proof generator
    |       +-- assignment.rs           # Deterministic chunk-to-node assignment (3x replication)
    |       +-- manifest_index.rs       # Persistent merkle_root -> DataManifest lookup
    |       +-- store.rs                # On-disk content-addressed chunk store (<cid>.chunk)
    |       +-- fetch.rs                # Windowed chunk download orchestration
    |       +-- serve.rs                # Inbound chunk, manifest, and push request handler
    |       +-- verify.rs               # BLAKE3, CID, and Merkle proof verification
    |       +-- manifest.rs             # CBOR manifest serialization + deserialization
    |       +-- content_id.rs           # BLAKE3 -> CIDv1 conversion
    |       +-- mmap.rs                 # Zero-copy memory-mapped file I/O
    |       +-- announce.rs             # ChunkAnnouncement gossipsub messages
    |       +-- gc.rs                   # Garbage collection of unassigned chunks (mark-and-sweep)
    |
    +-- sum-node/                       # Crate 4: CLI Entry Point
        +-- src/
            +-- main.rs                 # CLI: listen, ingest, fetch, download, send
            +-- lib.rs                  # Library exports for e2e_helper
            +-- rpc_client.rs           # JSON-RPC client to L1
            +-- tx_builder.rs           # Transaction construction + signing (bincode v1 mirror types)
            +-- por_worker.rs           # Background PoR challenge responder
            +-- market_sync.rs          # Background market sync + auto-fetch + GC integration
            +-- acl.rs                  # ACL check before serving chunks
            +-- download.rs             # DownloadOrchestrator — full file retrieval by merkle_root
            +-- upload.rs               # UploadOrchestrator — multi-node push with confirmation
            +-- bin/e2e_helper.rs       # E2E test helper CLI

Tests: 90 unit tests + P2P integration test passing
    - sum-types: 8 tests (storage types, RPC types, config)
    - sum-net: 14 tests (codec, identity, base58, L1 address, push round-trip)
    - sum-store: 63 tests (chunker, merkle, verify, content_id, store, manifest, manifest_index, assignment, announce, mmap, gc)
    - sum-node: 4 tests (tx_builder)
    - Integration: test_p2p.sh (2-node chunk transfer on localhost)
    - Integration: test_e2e_l1.sh (full PoR loop, requires live validators)

What We Deleted (The "Anti-Features")
To make this pure storage, we dropped several heavy components from the OmniNode repo:
    - omni-bridge & python/ directory: Gone. No Python, PyO3, or NumPy needed. 100% native Rust.
    - omni-pipeline: Gone. No micro-batch scheduling, GPipe, or hidden-state tensor transfers.
    - gguf.rs: Gone. The parser is removed. sum-store treats everything as raw bytes.

The Core Engineering Focuses for Team B
    1. The merkle.rs Engine (in sum-store): Builds the Merkle DAG constructor. Takes raw files, chunks them, hashes leaves with BLAKE3, outputs the MerkleRoot (sent to L1) and individual chunk proofs (for PoR audits). [DONE]
    2. The identity.rs Bridge (in sum-net): Custom identity provider taking a SUM Chain Ed25519 private key seed and deriving the PeerId. Every storage node's network identity is tied to its on-chain financial identity. [DONE]
    3. The rpc_client.rs (in sum-node): Lightweight JSON-RPC client to talk to the SUM Chain Validator node. Enables ACL checks, challenge polling, and proof submission. [DONE]
    4. The por_worker.rs (in sum-node): Background loop that polls for PoR challenges, generates Merkle proofs, and submits signed transactions back to the L1 to earn Koppa. [DONE]
    5. The tx_builder.rs (in sum-node): Constructs transactions with bincode v1 mirror types matching the L1's exact byte layout, signs with Ed25519, and hex-encodes for RPC submission. [DONE]
    6. The assignment.rs (in sum-store): Deterministic chunk-to-node mapping using blake3(merkle_root ++ chunk_index ++ replica). Same algorithm on L1 and off-chain. [DONE]
    7. The market_sync.rs (in sum-node): Polls L1 for funded files, computes assignments, auto-fetches missing chunks from peers. [DONE]
    8. The download command (in sum-node): DownloadOrchestrator — manifest fetch, parallel chunk download (max 10 concurrent), CID verification, merkle root verification, file reassembly. [DONE]
    9. The upload orchestrator (in sum-node): UploadOrchestrator — push protocol (push_data on ShardRequest), push to R=3 assigned nodes, ACK confirmation tracking. Wired into run_ingest(). --client flag exits after confirmation; default enters serve loop. [DONE]
    10. The garbage collector (in sum-store): GarbageCollector with mark-and-sweep, configurable grace period, L1-reachability guard. Integrated into MarketSyncWorker. [DONE]
