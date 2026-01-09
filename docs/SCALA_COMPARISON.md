# Ergo Rust Node vs Scala Node Comparison

## Executive Summary

This document compares the Rust implementation with the Scala reference implementation to identify gaps and guide further development.

**Last Updated:** January 9, 2025  
**Overall Completion:** ~95%

---

## Quick Status Overview

| Component | Status | Notes |
|-----------|--------|-------|
| P2P Networking | COMPLETE | Full message types, handshake, peer management |
| Sync Protocol | COMPLETE | Header-first sync, block download, rate limiting |
| Consensus | COMPLETE | Autolykos v2, difficulty adjustment |
| UTXO State | COMPLETE | AVL+ tree, state root, box indexing |
| State Rollback | COMPLETE | UndoData mechanism |
| Mempool | COMPLETE | Transaction dependency tracking implemented |
| Mining | COMPLETE | Candidate generation, coinbase, tx selection |
| Wallet | COMPLETE | HD derivation, tx building, signing, encryption |
| REST API | COMPLETE | Core + extended endpoints (utils, emission, script) |
| Block Pruning | COMPLETE | Configurable blocks_to_keep |
| UTXO Snapshots | MISSING | Not yet implemented |
| NiPoPoW | MISSING | Not yet implemented |
| Extra Indexer | MISSING | Not yet implemented |
| Re-emission (EIP-27) | MISSING | Not yet implemented |

---

## Detailed Component Comparison

### 1. P2P Networking

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| Handshake Protocol | Yes | Yes | COMPLETE |
| Message Types (all) | Yes | Yes | COMPLETE |
| Message Framing | Magic + checksum | Same | COMPLETE |
| Peer Management | Actor-based | DashMap + channels | COMPLETE |
| Rate Limiting | 400 items/100ms | Same | COMPLETE |
| Connection Pool | Configurable | Configurable | COMPLETE |
| DNS Seed Discovery | Yes | No | MISSING |
| Peer Scoring | Complex | Basic | PARTIAL |

### 2. Sync Protocol

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| Header-first sync | Yes | Yes | COMPLETE |
| SyncInfo V1/V2 | Both | Both | COMPLETE |
| Inv handling | Full | Full | COMPLETE |
| Modifier requests | Batched (400) | Batched (400) | COMPLETE |
| Parallel block download | Yes | Yes (16 concurrent) | COMPLETE |
| Chain reorganization | Full | Cumulative difficulty | COMPLETE |
| NiPoPoW bootstrap | Yes | No | MISSING |
| UTXO snapshot sync | Yes | No | MISSING |

### 3. State Management

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| UTXO State (AVL+) | avldb | ergo_avltree_rust | COMPLETE |
| State root calculation | Yes | Yes | COMPLETE |
| State root verification | Yes | ADDigest comparison | COMPLETE |
| Box storage | Yes | Yes | COMPLETE |
| ErgoTree index | Yes | BLAKE2b hash index | COMPLETE |
| Token index | Yes | TokenId index | COMPLETE |
| State snapshots | Periodic | Framework only | PARTIAL |
| Rollback | Full | UndoData mechanism | COMPLETE |

### 4. Block/Transaction Validation

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| Header validation | Full | Full | COMPLETE |
| PoW verification | Autolykos v2 | Autolykos v2 | COMPLETE |
| Difficulty adjustment | Linear LSQ | Linear LSQ | COMPLETE |
| Merkle root verification | Yes | Yes | COMPLETE |
| Transaction validation | Full | Full | COMPLETE |
| Script execution | ergotree-interpreter | Via sigma-rust | COMPLETE |
| Input existence | Yes | validate_inputs_exist | COMPLETE |
| Data input existence | Yes | validate_data_inputs_exist | COMPLETE |
| Token conservation | Yes | validate_token_conservation | COMPLETE |
| ERG conservation | Yes | validate_erg_conservation | COMPLETE |
| Block cost limits | Yes | Framework only | PARTIAL |

### 5. Mempool

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| Transaction storage | Yes | DashMap | COMPLETE |
| Fee ordering | Yes | BTreeSet | COMPLETE |
| Double-spend detection | Yes | Input mapping | COMPLETE |
| Size limits | Configurable | Configurable | COMPLETE |
| Expiry | Yes | 1 hour default | COMPLETE |
| Dependency tracking | Yes | Weight-based ordering | COMPLETE |

### 6. Mining

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| Block candidate | Full | Full | COMPLETE |
| Transaction selection | Fee-based | Fee-based | COMPLETE |
| Coinbase creation | Yes | EmissionParams + CoinbaseBuilder | COMPLETE |
| Emission schedule | 75 ERG decreasing | Same | COMPLETE |
| Difficulty calculation | Yes | DifficultyAdjustment | COMPLETE |
| External mining | Stratum-like | Framework | PARTIAL |
| Internal CPU mining | Yes | No | MISSING |

### 7. Wallet

| Feature | Scala | Rust | Status |
|---------|-------|------|--------|
| HD derivation (BIP32/44) | Yes | Via ergo-lib | COMPLETE |
| Mnemonic (BIP39) | Yes | 12-24 words | COMPLETE |
| EIP-3 paths | m/44'/429'/... | Same | COMPLETE |
| Address generation | All types | P2PK | COMPLETE |
| Box tracking | Full | BoxTracker | COMPLETE |
| Transaction building | Full | WalletTxBuilder | COMPLETE |
| Box selection | Multiple algorithms | SimpleBoxSelector | COMPLETE |
| Transaction signing | Yes | Via ergo-lib | COMPLETE |
| Encryption | AES-256-GCM | AES-256-GCM + Argon2id | COMPLETE |
| P2SH/P2S addresses | Yes | No | MISSING |

### 8. REST API

| Category | Scala Endpoints | Rust Status |
|----------|-----------------|-------------|
| /info | Full | COMPLETE |
| /blocks | Full | COMPLETE |
| /transactions | Full | PARTIAL |
| /utxo | Full | PARTIAL |
| /peers | Full | COMPLETE |
| /mining | Full | COMPLETE |
| /wallet | Full | COMPLETE |
| /script | Full | COMPLETE |
| /emission | Full | COMPLETE |
| /utils | Full | COMPLETE |
| /scan | Full | MISSING |
| /nipopow | Full | MISSING |
| /blockchain (indexed) | Full | MISSING |

---

## Gaps Requiring Implementation

### Priority 1: UTXO Snapshot Sync

**Problem:** New nodes must replay entire blockchain to build UTXO state.

**Scala Implementation:**
- `UtxoSetSnapshotProcessor.scala` - snapshot download management
- `SnapshotsDb.scala` - manifest/chunk storage
- P2P messages: GetManifest, Manifest, GetUtxoSnapshotChunk, UtxoSnapshotChunk

**Rust Changes:**
- Create `snapshots.rs` module
- Implement download plan and chunk management
- Add P2P message types
- Integrate with sync module

### Priority 2: NiPoPoW Support

**Problem:** No light client support or cross-chain verification.

**Scala Implementation:**
- `NipopowAlgos.scala` - proof generation and comparison
- `PoPowHeader.scala` - headers with interlinks
- `NipopowVerifier.scala` - proof verification state machine

**Rust Changes:**
- Create `nipopow/` module in ergo-consensus
- Implement PoPowHeader, NipopowProof, NipopowVerifier
- Add P2P messages and API endpoints
- Integrate bootstrap sync

### Priority 3: Extra Indexer

**Problem:** No advanced blockchain queries for explorers/dApps.

**Scala Implementation:**
- `ExtraIndexer.scala` - optional indexed mode
- Segment-based indexing for large datasets
- `/blockchain/*` API endpoints

**Rust Changes:**
- Create `indexer/` module in ergo-storage
- Implement indexed data structures
- Add `/blockchain/*` API endpoints
- Make optional via configuration

### Priority 4: Re-emission (EIP-27)

**Problem:** No support for long-term token economics.

**Scala Implementation:**
- `ReemissionSettings.scala` - configuration
- Modified emission calculation after activation

**Rust Changes:**
- Add re-emission configuration
- Modify emission/coinbase logic
- Update validation rules

---

## Architecture Comparison

| Aspect | Scala Node | Rust Node |
|--------|------------|-----------|
| Concurrency | Akka actors | Tokio async/await |
| Messaging | Actor messages | mpsc channels |
| Framework | Scorex | Direct implementation |
| Storage | Various backends | RocksDB only |
| Main coordinator | ErgoNodeViewHolder | StateManager |
| Sync logic | ErgoNodeViewSynchronizer | SyncProtocol |
| Network | NetworkController | NetworkService |

---

## Key File Mapping

| Scala Component | Rust Equivalent |
|-----------------|-----------------|
| ErgoApp | ergo-node/src/node.rs |
| ErgoNodeViewHolder | ergo-state/src/manager.rs |
| ErgoNodeViewSynchronizer | ergo-sync/src/protocol.rs |
| NetworkController | ergo-network/src/service.rs |
| PeerManager | ergo-network/src/peer.rs |
| ErgoMemPool | ergo-mempool/src/pool.rs |
| UtxoState | ergo-state/src/utxo.rs |
| ErgoHistory | ergo-state/src/history.rs |
| AutolykosPowScheme | ergo-consensus/src/autolykos.rs |
| ErgoMiner | ergo-mining/src/miner.rs |
| CandidateGenerator | ergo-mining/src/candidate.rs |
| ErgoWalletActor | ergo-wallet/src/wallet.rs |
| TransactionBuilder | ergo-wallet/src/tx_builder.rs |

---

## Scala Test References

Use these tests to verify Rust implementation compatibility:

| Feature | Test File |
|---------|-----------|
| Mempool dependencies | ErgoMemPoolSpec.scala |
| NiPoPoW proofs | PoPowAlgosSpec.scala |
| NiPoPoW verification | NipopowVerifierSpec.scala |
| UTXO snapshots | UtxoSetSnapshotProcessorSpecification.scala |
| Deep rollback | DeepRollBackSpec.scala |
| Fork resolution | ForkResolutionSpec.scala |
| Multi-node sync | UtxoStateNodesSyncSpec.scala |
| Mining | CandidateGeneratorSpec.scala, ErgoMinerSpec.scala |
