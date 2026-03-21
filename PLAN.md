Here is the complete architectural structure and phased implementation plan for the SUM Chain Native Decentralized Storage Protocol.
This design keeps the Layer-1 state trie lightweight by ensuring no actual file data touches the blockchain. The chain acts strictly as the cryptographic source of truth, the financial settlement layer, and the access control enforcer.

Part 1: Architectural Structure (How It Looks)
The architecture is divided into the On-Chain State (the metadata and rules) and the Off-Chain Mesh (the physical byte storage).
1. Native State Trie Objects (On-Chain)
These structures are compiled directly into the SUM Chain Rust node binary.
    •    NodeRegistry: Tracks the network's hardware.
    ◦    address: Ed25519 Identity.
    ◦    role: StorageNode (Requires a minimum staked balance of native Koppa Ϙ).
    ◦    status: Active, Challenged, Slashed.
    •    StorageMetadata: Replaces traditional smart contracts. Maps a file's identity to its rules.
    ◦    merkle_root: The BLAKE3 top hash of the file's chunk DAG.
    ◦    file_size: Total bytes.
    ◦    access_list: Vector of allowed Ed25519 addresses (empty = public).
    ◦    storage_fee_pool: Locked Koppa (Ϙ) to pay nodes over time.
2. The File Ingestion Pipeline (Client-Side to Network)
    1    Encrypt (Optional): If the file is private, encrypt locally via XChaCha20-Poly1305.
    2    Chunk & Hash: Split the file into uniform blocks (e.g., 1 MB). Hash each block with BLAKE3 to create leaf nodes.
    3    Merkle DAG: Build the Merkle Tree. The resulting merkle_root becomes the file's absolute network identity.
    4    Allocate Transaction: The client broadcasts a TxPayload::AllocateStorage transaction to the Validators, containing the merkle_root, the access_list, and the storage_fee_pool.
3. The Combinatorial Storage Mesh (Off-Chain P2P)
Once the metadata is on-chain, the client pushes the actual chunks to the Storage Nodes via a dedicated libp2p protocol (e.g., /sum/storage/v1).
    •    Unique State Routing: The network uses the combinatorial math discussed previously. If a file has 4 chunks and the replication factor is 3, the network needs to store 12 chunk copies. The protocol assigns these 12 copies across the nodes such that no node shares the exact same state matrix.
    •    Peer-to-Peer Gossiping: Storage nodes use gossipsub to announce which chunks they have successfully acquired.
4. The L1 Security Engine (Proof of Retrievability - PoR)
    •    The Audit: Every N blocks, the active Validator uses the previous block's hash as a random seed to select a specific StorageMetadata entry and a specific chunk index.
    •    The Challenge: The Validator emits an on-chain event challenging the assigned Storage Nodes.
    •    The Proof: The Storage Nodes must submit a TxPayload::SubmitStorageProof containing the chunk's BLAKE3 hash and the Merkle sibling path to the root.
    •    Settlement: The native StorageExecutor verifies the Merkle path. Pass = reward distributed from the storage_fee_pool. Fail = stake slashed.

Part 2: Phased Implementation Plan
This roadmap is designed for a core Rust blockchain engineering team to build the protocol from the ground up without breaking consensus.
Phase 1: Cryptography & State Foundation (Weeks 1-3)
Goal: Define the native state and build the client-side file chunker.
    1    Core State Types: Implement the NodeRegistry and StorageMetadata structs in the blockchain's state definitions. Ensure they are correctly serialized into the state trie.
    2    Merkle Builder (CLI/SDK): Build a standalone Rust utility that takes a file, chunks it into 1 MB slices, calculates the BLAKE3 leaf hashes, and generates the Merkle Root and sibling paths.
    3    Transaction Payloads: Define TxPayload::AllocateStorage and TxPayload::RegisterStorageNode.
Phase 2: Native StorageExecutor & Economics (Weeks 4-6)
Goal: Allow the chain to accept storage requests and lock funds.
    1    Executor Implementation: Build storage_executor.rs. Wire it into the main block execution loop.
    2    Fee Mechanics: Implement the logic for AllocateStorage. When this transaction is executed, the executor must deduct native Koppa from the sender and lock it in the StorageMetadata state object.
    3    Node Staking: Implement the logic allowing users to lock Koppa to become recognized as StorageNodes in the NodeRegistry.
Phase 3: The Combinatorial P2P Mesh (Weeks 7-10)
Goal: Nodes can actually pass the file chunks to each other.
    1    Storage Libp2p Protocol: Implement the custom /sum/storage/v1 request-response protocol for transferring 1 MB chunks.
    2    Combinatorial Assignment Logic: Write the algorithm that maps a file's Merkle Root and chunk count to a specific subset of active Storage Nodes, enforcing the rule that configuration states must be unique (e.g., calculating $\binom{C}{m}$).
    3    Native ACL Interception: Modify the inbound P2P handler so that before serving a requested chunk, it natively queries the local blockchain state to verify if the requester's PeerId matches the access_list in the StorageMetadata.
Phase 4: Consensus Security & PoR (Weeks 11-14)
Goal: Ensure data availability through cryptographic audits.
    1    Challenge Generation: Update the block proposer logic. Every 100 blocks, use the block hash to deterministically select a Merkle Root and chunk index to audit.
    2    Proof Submission: Implement TxPayload::SubmitStorageProof.
    3    Native Verification: Update the StorageExecutor to take the submitted Merkle path, hash it up to the root, and compare it against the root stored in the StorageMetadata state.
    4    Slashing & Rewards: If the proof is valid, trigger the TokenExecutor to unlock a micro-payment of Koppa from the file's fee pool to the node. If the proof is missing after the challenge window expires, slash the node's stake.
Phase 5: The "Scale-Out" Upgrade (Future)
Goal: Transition from pure replication to Erasure Coding.
    1    Reed-Solomon Integration: Replace the pure 3x chunk replication with a Reed-Solomon encoding library.
    2    Protocol Upgrade: Update the Merkle Builder to hash shards (data + parity) instead of raw chunks. The rest of the on-chain metadata, ACL, and PoR systems remain completely unchanged, but the network storage overhead drops by 60-80%.

sum-storage-node/
│
├── Cargo.toml                          # Workspace manifest (drastically simplified)
├── rust-toolchain.toml                 # Rust 2024 edition, MSRV 1.85+
├── README.md                           # Protocol spec for SUM Chain native storage
│
├── crates/
│   ├── sum-types/                      # Crate 1: Core Definitions
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── node.rs                 # NodeRegistry types (Address, Role, Stake)
│   │       ├── manifest.rs             # StorageMetadata, MerkleRoot, ChunkDescriptor
│   │       └── error.rs                # Unified Storage & Net errors
│   │
│   ├── sum-net/                        # Crate 2: The P2P Mesh (libp2p)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # SumNet API handle
│   │       ├── behaviour.rs            # mDNS, Gossipsub, Request-Response
│   │       ├── identity.rs             # NEW: Ed25519 SUM Chain Wallet Integration
│   │       ├── codec.rs                # Custom /sum/storage/v1 transfer protocol
│   │       ├── gossip.rs               # Chunk availability announcements
│   │       └── interceptor.rs          # NEW: Queries L1 state trie to enforce ACLs
│   │
│   ├── sum-store/                      # Crate 3: File I/O & Merkle Math
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── chunker.rs              # REFACTORED: Generic binary file slicer (no GGUF)
│   │       ├── merkle.rs               # NEW: BLAKE3 Merkle DAG builder & Path generator
│   │       ├── store.rs                # On-disk content-addressed chunk store
│   │       ├── fetch.rs                # 64 Mi




What We Deleted (The "Anti-Features")
To make this pure storage, we are dropping several heavy components that existed in the OmniNode repo:
    •    omni-bridge & python/ directory: Gone. There is no need for Python, PyO3, or NumPy. This is 100% native Rust.
    •    omni-pipeline: Gone. We don't need micro-batch scheduling, GPipe, or hidden-state tensor transfers.
    •    gguf.rs: Gone. The parser is removed. sum-store treats everything as raw bytes.
The 3 Core Engineering Focuses for Team B
    1    The merkle.rs Engine (in sum-store) Instead of just hashing chunks and stopping there, Team B needs to build the Merkle DAG constructor. This takes the raw file, chunks it, hashes the leaves with BLAKE3, and outputs the final MerkleRoot (which gets sent to the L1 chain) and the individual chunk proofs (which nodes need for the Proof of Retrievability audits).
    2    The identity.rs Bridge (in sum-net) Out of the box, libp2p generates random PeerIds. Team B must write a custom identity provider that takes a standard SUM Chain Ed25519 private key seed and derives the PeerId from it. This ensures that every storage node's network identity is perfectly tied to its on-chain financial identity.
    3    The rpc_client.rs (in sum-node) The storage daemon cannot operate blindly. It needs a lightweight JSON-RPC client to talk to the local (or remote) SUM Chain Validator node. When a peer requests a chunk, sum-node must instantly ask the L1 chain: "Is this PeerId in the ACL for this MerkleRoot?"
