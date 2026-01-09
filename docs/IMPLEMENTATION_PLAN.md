# Ergo Rust Node - Implementation Plan

## Overview

This document outlines the implementation plan for the Ergo Rust Node, a full blockchain node implementation with feature parity to the [Scala reference implementation](https://github.com/ergoplatform/ergo).

## Current Status

**Last Updated:** January 9, 2025

The project has completed core infrastructure (Phases 1-10). Header and block synchronization is working with real Ergo mainnet nodes. Mining and wallet functionality are fully implemented.

| Crate | Status | Description |
|-------|--------|-------------|
| ergo-storage | COMPLETE | RocksDB storage layer with column families |
| ergo-consensus | COMPLETE | Autolykos v2 PoW, difficulty adjustment, validation |
| ergo-state | COMPLETE | UTXO state with AVL tree, history management, box indexing |
| ergo-mempool | COMPLETE | Transaction pool with fee ordering, dependency tracking |
| ergo-network | COMPLETE | P2P protocol, TCP handling, handshake, peer management |
| ergo-sync | COMPLETE | Sync state machine, header sync, block download scheduler |
| ergo-api | COMPLETE | REST API endpoints (core + extended: utils, emission, script) |
| ergo-mining | COMPLETE | Block candidate generation, coinbase, tx selection |
| ergo-wallet | COMPLETE | HD wallet, transaction building, signing |
| ergo-node | COMPLETE | Main binary with all components wired up, block pruning |

**Overall Completion: ~95%**

---

## Completed Work

### Phase 1: Core Infrastructure

- [x] Workspace structure with 10 crates
- [x] Integration with sigma-rust types (ergo-lib, ergo-chain-types)
- [x] RocksDB storage with column families
- [x] Error types and result handling
- [x] Configuration system (TOML-based)
- [x] Logging with tracing

### Phase 2: State Management

- [x] UTXO State with AVL tree integration via ergo_avltree_rust
- [x] State root calculation and verification
- [x] Box indexing (ErgoTree hash, Token ID)
- [x] History management with header/block storage
- [x] Cumulative difficulty for fork selection
- [x] Fork detection via find_fork_height

### Phase 3: Consensus

- [x] Autolykos v2 PoW verification
- [x] Difficulty adjustment (linear least squares over 8 epochs)
- [x] Header and block validation
- [x] Transaction validation (inputs, scripts, token/ERG conservation)

### Phase 4: Networking

- [x] P2P message types (Handshake, GetPeers, Peers, SyncInfo, Inv, RequestModifier, Modifier)
- [x] Message codec with framing and checksums
- [x] Peer management with scoring
- [x] Connection pool management

### Phase 5: Synchronization

- [x] Header-first sync strategy
- [x] Block download scheduler with parallel downloads
- [x] Rate limiting (matches Scala: 400 items, 100ms interval)
- [x] Chain reorganization handling

### Phase 6: Node Binary

- [x] Component initialization and coordination
- [x] Event routing between network, sync, and state
- [x] Graceful shutdown
- [x] Peer reconnection with exponential backoff

### Phase 7: Mining

- [x] Block candidate generation with proper difficulty
- [x] Transaction selection by fee from mempool
- [x] Coinbase transaction creation with emission schedule
- [x] External miner support

### Phase 8: API (Core Endpoints)

- [x] /info - Node information
- [x] /blocks - Block queries
- [x] /transactions - Transaction submission
- [x] /utxo - UTXO queries
- [x] /peers - Peer management
- [x] /mining - Mining interface (candidate, solution, rewardAddress)
- [x] /wallet - Wallet operations (init, unlock, lock, status, balances, addresses, send)

### Phase 9: Wallet

- [x] HD key derivation (BIP32/BIP39, EIP-3 compliant)
- [x] Mnemonic support (12-24 words)
- [x] Transaction building with box selection
- [x] Transaction signing via ergo-lib
- [x] AES-256-GCM encryption with Argon2id key derivation

### Phase 10: Mempool

- [x] Fee-based transaction ordering
- [x] Double-spend detection
- [x] Size limits and expiry

### Phase 11: Transaction Dependency Tracking

- [x] Weight field in PooledTransaction
- [x] Parent-child relationship tracking via output_to_tx mapping
- [x] Recursive weight propagation (update_family)
- [x] Weight-based ordering (parents before children)
- [x] DoS protection (MAX_PARENT_SCAN_DEPTH, MAX_PARENT_SCAN_TIME_MS)

### Phase 12: Block Pruning

- [x] blocks_to_keep configuration (-1 = keep all)
- [x] PruningConfig in History
- [x] minimal_full_block_height tracking
- [x] Pruning logic respecting voting epoch boundaries (every 1024 blocks)
- [x] should_download_block_at_height() check for sync

### Phase 13: Extended API Endpoints

- [x] /utils/* (seed generation, blake2b hashing, address validation/conversion)
- [x] /emission/* (emission info at height, emission scripts)
- [x] /script/* (p2sAddress, p2shAddress, addressToTree, addressToBytes)

---

## Remaining Work

### Priority 1: UTXO Snapshot Sync

Enable fast bootstrap via UTXO set snapshots.

**New files:** `crates/ergo-state/src/snapshots.rs`

**Tasks:**
- [ ] SnapshotsDb for manifest/chunk storage
- [ ] Manifest and subtree serialization
- [ ] Download plan management
- [ ] P2P messages (GetManifest, Manifest, GetUtxoSnapshotChunk, UtxoSnapshotChunk)
- [ ] Sync module integration

### Priority 2: NiPoPoW Support

Light client bootstrap and cross-chain verification.

**New module:** `crates/ergo-consensus/src/nipopow/`

**Tasks:**
- [ ] PoPowHeader with interlinks
- [ ] NipopowAlgos (update_interlinks, prove, verify)
- [ ] NipopowProof struct and validation
- [ ] NipopowVerifier state machine
- [ ] P2P messages and API endpoints

### Priority 3: Extra Indexer

Advanced blockchain queries for explorers/dApps.

**New module:** `crates/ergo-storage/src/indexer/`

**Tasks:**
- [ ] Indexed data structures (address, transaction, box, token)
- [ ] Segment-based indexing
- [ ] /blockchain/* API endpoints
- [ ] Optional configuration

### Priority 4: Re-emission (EIP-27)

Long-term token economics support.

**Files:** `crates/ergo-consensus/src/emission.rs`, `crates/ergo-mining/src/coinbase.rs`

**Tasks:**
- [ ] Re-emission configuration
- [ ] Modified emission calculation
- [ ] Re-emission output validation
- [ ] Mining candidate updates

---

## Test Coverage

| Crate | Tests | Status |
|-------|-------|--------|
| ergo-consensus | 58 | PASSING |
| ergo-mempool | 22 | PASSING |
| ergo-network | 19 | PASSING |
| ergo-state | 29 | PASSING |
| ergo-sync | 8 | PASSING |
| ergo-mining | 3 | PASSING |
| ergo-wallet | 23 | PASSING |
| ergo-storage | 3 | PASSING |
| ergo-api | 18 | PASSING |
| ergo-node | 2 | PASSING |
| **Total** | **185** | **PASSING** |

---

## Performance Targets

| Metric | Target | Current |
|--------|--------|---------|
| Block validation | < 100ms | TBD |
| Transaction validation | < 10ms | TBD |
| State lookup | < 1ms | TBD |
| API response | < 50ms | TBD |
| Header sync speed | > 100 headers/sec | ~17 headers/sec |
| Block sync speed | > 100 blocks/sec | TBD |
| Memory (full node) | < 4GB | TBD |

---

## Dependencies

### From sigma-rust
- `ergo-lib` - Core types, wallet utilities, HD derivation
- `ergotree-interpreter` - ErgoScript execution
- `ergo-chain-types` - Headers, BlockId, basic types
- `sigma-ser` - Binary serialization
- `ergo-nipopow` - NiPoPoW proofs
- `ergo_avltree_rust` - AVL+ tree implementation

### External
- `tokio` - Async runtime
- `rocksdb` - Storage backend
- `axum` - HTTP server
- `k256` - secp256k1 cryptography
- `blake2` - Hashing
- `tokio-util` - Codec support

---

## Milestones

| Milestone | Status |
|-----------|--------|
| M1: Compile & Basic Tests | COMPLETE |
| M2: Core Components | COMPLETE |
| M3: Sync Headers | COMPLETE |
| M4: Sync Blocks | COMPLETE |
| M5: Mining Support | COMPLETE |
| M6: Wallet Complete | COMPLETE |
| M7: API Complete | COMPLETE |
| M8: Production Ready | IN PROGRESS |

---

## Architecture

```
                    +-----------------------------------------------------+
                    |                    ergo-node                        |
                    |         (CLI, Configuration, Coordination)          |
                    +-----------------------------------------------------+
                                           |
           +-------------------------------+-------------------------------+
           |                               |                               |
           v                               v                               v
+---------------------+      +---------------------+      +---------------------+
|    ergo-network     |      |     ergo-sync       |      |     ergo-api        |
|  (P2P, Handshake,   |<---->|  (Sync Protocol,    |      |   (REST Server,     |
|   Peer Management)  |      |   Block Download)   |      |    Handlers)        |
+---------------------+      +---------------------+      +---------------------+
                                       |
           +---------------------------+---------------------------+
           |                           |                           |
           v                           v                           v
+---------------------+      +---------------------+      +---------------------+
|   ergo-consensus    |      |    ergo-state       |      |    ergo-mempool     |
|  (PoW, Difficulty,  |<---->|  (UTXO, History,    |<---->|   (Tx Pool, Fee     |
|   Validation)       |      |   Rollback)         |      |    Ordering)        |
+---------------------+      +---------------------+      +---------------------+
                                       |
           +---------------------------+---------------------------+
           |                           |                           |
           v                           v                           v
+---------------------+      +---------------------+      +---------------------+
|    ergo-mining      |      |   ergo-storage      |      |    ergo-wallet      |
|  (Candidates,       |      |  (RocksDB, Column   |      |  (HD Keys, TX       |
|   Coinbase)         |      |   Families)         |      |   Building)         |
+---------------------+      +---------------------+      +---------------------+
```
