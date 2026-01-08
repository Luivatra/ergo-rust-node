# Ergo Rust Node

A Rust implementation of the Ergo blockchain node with feature parity to the [Scala reference implementation](https://github.com/ergoplatform/ergo).

## Implementation Status

### Completed
- **P2P Networking**: Full handshake protocol, message framing, peer management
- **Sync Protocol**: Header-first sync, block download, inventory handling (rate limits match Scala node)
- **PoW Verification**: Autolykos v2 verification with difficulty adjustment
- **Block Validation**: Merkle root verification, header validation
- **Transaction Validation**: Input/data-input existence, script execution, token/ERG conservation
- **UTXO State**: Box storage/retrieval, state root calculation and verification
- **State Rollback**: UndoData mechanism for chain reorganizations
- **Storage Layer**: RocksDB with column families for all data types
- **Box Indexing**: ErgoTree hash index, Token ID index for fast lookups
- **Fork Management**: Cumulative difficulty tracking, chain reorganization detection
- **Mining**: Block candidate generation, transaction selection by fee, coinbase creation
- **Wallet HD Derivation**: BIP32/BIP39 mnemonic support, EIP-3 compliant derivation paths
- **Transaction Building**: WalletTxBuilder with box selection and signing support
- **Wallet Security**: AES-256-GCM encryption with Argon2id key derivation (OWASP params)
- **Mining API**: GET/POST endpoints for candidate generation, solution submission, reward address
- **Wallet API**: Init, unlock, lock, status, balances, addresses, transaction send endpoints

### In Progress
- Full block synchronization testing
- Header sync verified working (~17 headers/sec with mainnet nodes)

### Planned
- NiPoPoW bootstrap
- UTXO snapshot sync
- Block pruning
- Periodic state snapshots

## Project Overview

This project implements a full Ergo blockchain node in Rust, capable of:

- **Full blockchain synchronization** from genesis or UTXO snapshots
- **Block validation** including Autolykos v2 Proof-of-Work verification
- **Transaction validation** with ErgoScript execution
- **P2P networking** compatible with existing Ergo nodes
- **REST API** matching the Scala node's API surface
- **Mining support** for block production
- **Integrated wallet** for key management and transaction signing

## Architecture

### Crate Structure

```
crates/
├── ergo-node/        # Main binary, CLI, configuration
├── ergo-consensus/   # Consensus rules, Autolykos PoW, difficulty adjustment
├── ergo-state/       # UTXO state management, AVL+ tree
├── ergo-mempool/     # Transaction pool with fee ordering
├── ergo-network/     # P2P protocol, peer management
├── ergo-sync/        # Block synchronization strategies
├── ergo-storage/     # RocksDB abstraction, indexes
├── ergo-api/         # REST API server (axum-based)
├── ergo-mining/      # Block candidate generation, coinbase, tx selection
└── ergo-wallet/      # HD wallet, transaction building, signing
```

### Dependencies on sigma-rust

This project builds on top of [sigma-rust](https://github.com/ergoplatform/sigma-rust), using:

| Crate | Purpose |
|-------|---------|
| `ergo-lib` | Transaction/box types, wallet utilities, HD derivation |
| `ergotree-interpreter` | ErgoScript execution |
| `ergo-chain-types` | Headers, block IDs, basic types |
| `sigma-ser` | Binary serialization |
| `ergo-nipopow` | NiPoPoW proof verification |
| `ergo-merkle-tree` | Merkle proofs |

### Component Responsibilities

**ergo-node**: Application entry point
- CLI argument parsing (clap)
- Configuration loading (TOML)
- Component initialization and lifecycle
- Signal handling and graceful shutdown

**ergo-consensus**: Validation rules
- Block header validation
- Autolykos v2 PoW verification
- Difficulty adjustment (linear least squares over 8 epochs)
- Protocol parameter voting
- Extension block validation

**ergo-state**: Blockchain state
- UTXO set management via AVL+ tree
- State root calculation
- Box (UTXO) creation and spending
- State snapshots for rollback
- AD (Authenticated Data) proofs

**ergo-mempool**: Unconfirmed transactions
- Transaction storage with fee ordering
- Dependency graph tracking
- Double-spend prevention
- Size limits and eviction policies
- Transaction validation before acceptance

**ergo-network**: Peer-to-peer communication
- TCP connection handling
- Ergo P2P protocol messages
- Peer discovery (DNS seeds, peer exchange)
- Connection management (max peers, timeouts)
- Message serialization/deserialization

**ergo-sync**: Chain synchronization
- Header-first sync strategy
- Block download scheduling
- Parallel block fetching
- Chain reorganization
- UTXO snapshot bootstrap

**ergo-storage**: Persistence layer
- RocksDB wrapper with column families
- Block storage and retrieval
- Header chain indexing
- UTXO database
- Transaction index

**ergo-api**: HTTP interface
- REST endpoints matching Scala node
- OpenAPI compatibility
- JSON serialization
- Authentication (API key)
- Rate limiting

**ergo-mining**: Block production
- Block candidate assembly with difficulty calculation
- Transaction selection by fee from mempool
- Coinbase transaction creation with emission schedule
- External miner protocol (stratum-like)
- Solution verification

**ergo-wallet**: Key management
- HD wallet (BIP32/BIP39) with EIP-3 derivation paths
- Mnemonic support (12/15/18/21/24 words)
- Address derivation (P2PK, mainnet/testnet)
- Box tracking and balance calculation
- Transaction building with box selection
- Transaction signing via ergo-lib
- Encrypted key storage

## Key Technical Details

### Ergo Protocol Specifics

**Block Structure**:
- Header: version, parent ID, merkle roots, timestamp, nBits, height, votes, PoW solution
- Block Transactions: 1 to 10M transactions per block
- Extension: key-value pairs for protocol parameters
- AD Proofs: authenticated data structure proofs

**UTXO Model (Extended)**:
- Boxes contain: value (ERG), tokens, registers (R0-R9), ErgoTree (spending condition)
- Boxes have creation height and unique ID
- Data inputs: read-only box references
- Storage rent: boxes can be spent after 4 years if rent not paid

**Autolykos v2 PoW**:
- Memory-hard algorithm (~2GB table)
- BLAKE2b-based
- k=32 (number of elements to sum)
- Solution: nonce + 32 element indices

**Difficulty Adjustment**:
- Epoch length: 1024 blocks
- Target: 2 minutes per block
- Linear least squares regression over 8 epochs
- Clamped to 2x adjustment per epoch

**Emission Schedule**:
- Initial reward: 75 ERG per block
- Fixed rate for first 2 years
- Decreases by 3 ERG per year after
- Minimum reward: 3 ERG per block

**Wallet Derivation (EIP-3)**:
- BIP44 path: `m/44'/429'/{account}'/0/{address_index}`
- Coin type: 429 (Ergo registered)
- Hardened account derivation
- Non-hardened address derivation

### Async Architecture

```rust
// Example component interaction pattern
pub struct NodeComponents {
    pub network: NetworkHandle,
    pub sync: SyncHandle,
    pub state: StateHandle,
    pub mempool: MempoolHandle,
    pub api: ApiHandle,
}

// Components communicate via async channels
pub struct NetworkHandle {
    cmd_tx: mpsc::Sender<NetworkCommand>,
    event_rx: broadcast::Receiver<NetworkEvent>,
}
```

### Configuration

```toml
# ergo-node.toml
[node]
state_type = "utxo"  # utxo, digest
mining = false
api_key = "..."

[network]
bind_address = "0.0.0.0:9030"
declared_address = "..."
max_connections = 30
known_peers = ["...", "..."]

[api]
bind_address = "127.0.0.1:9053"
public_url = "..."

[mining]
use_external_miner = true
reward_address = "..."

[wallet]
data_dir = ".ergo-wallet"
pre_generate = 20
```

## Development Guidelines

### ⚠️ CRITICAL: Serialization Compatibility with Scala Node

**ALWAYS check the Scala node source code in detail before implementing or modifying any serialization code.**

The Rust node MUST produce byte-identical serialization to the Scala node. Incorrect serialization will cause peers to reject connections or blacklist the node.

**Key serialization files in the Scala node** (located at `/home/luivatra/develop/ergo/ergo`):
- `ergo-core/src/main/scala/org/ergoplatform/network/message/` - Message specs (InvSpec, ModifiersSpec, etc.)
- `src/main/scala/org/ergoplatform/network/message/MessageSerializer.scala` - Message framing
- `src/main/scala/org/ergoplatform/network/message/BasicMessagesRepo.scala` - GetPeers, Peers, Snapshots
- `ergo-core/src/main/scala/org/ergoplatform/network/HandshakeSerializer.scala` - Handshake protocol
- `ergo-core/src/main/scala/org/ergoplatform/network/message/SyncInfoMessageSpec.scala` - SyncInfo V1/V2

**Common serialization pitfalls to verify:**
1. **VLQ vs fixed-width integers**: Scala uses VLQ (Variable Length Quantity) encoding for many integers via `putUInt`, `putUShort`, etc. Check if the Scala code uses `Writer.putInt` (VLQ) vs `ByteString.putInt` (fixed 4-byte big-endian).
2. **Message framing**: Checksum is only included when payload length > 0. Empty messages (like GetPeers) have no checksum.
3. **Message codes**: Must match exactly (GetPeers=1, Peers=2, RequestModifier=22, Modifier=33, Inv=55, SyncInfo=65, Handshake=75, etc.)
4. **String encoding**: Usually VLQ length prefix + UTF-8 bytes
5. **Optional fields**: Check how None/Some are encoded
6. **Collection encoding**: Usually VLQ count + elements

**Before implementing any P2P message serialization:**
1. Find the corresponding `MessageSpec` in the Scala code
2. Read the `serialize` and `parse` methods carefully
3. Check what `Writer`/`Reader` methods are used (VLQ vs fixed-width)
4. Write tests that verify byte-for-byte compatibility

### Code Style

- Follow Rust API guidelines
- Use `thiserror` for error types
- Use `tracing` for logging
- Prefer `async/await` over blocking operations
- Document public APIs with rustdoc

### Testing Strategy

1. **Unit tests**: Each crate has isolated tests
2. **Integration tests**: Cross-crate functionality in `tests/`
3. **Compatibility tests**: Verify against Scala node responses
4. **Sync tests**: Full chain synchronization on testnet

### Building

```bash
# Development build
cargo build

# Release build
cargo build --release

# Run tests
cargo test --workspace

# Run node
cargo run --release -- --config ergo-node.toml
```

### Logging

```rust
use tracing::{info, debug, warn, error, instrument};

#[instrument(skip(self))]
async fn process_block(&self, block: &Block) -> Result<()> {
    info!(block_id = %block.id(), height = block.height(), "Processing block");
    // ...
}
```

## Compatibility Notes

### P2P Protocol Compatibility

The Rust node must be fully compatible with Scala nodes:
- Same message types and serialization
- Same handshake protocol
- Same sync behavior

### API Compatibility

REST API must match Scala node for ecosystem tool compatibility:
- Same endpoint paths
- Same JSON schemas
- Same query parameters

### State Compatibility

Critical for network consensus:
- State roots must match exactly
- AVL+ tree operations must produce identical proofs
- Box serialization must be byte-identical

### Wallet Compatibility

Address derivation must match existing Ergo wallets:
- Same derivation paths (EIP-3)
- Same address encoding (base58)
- Transaction format compatibility

## References

- [Ergo Protocol Specification](https://docs.ergoplatform.com/protocol/)
- [Ergo Scala Node](https://github.com/ergoplatform/ergo)
- [sigma-rust Library](https://github.com/ergoplatform/sigma-rust)
- [ErgoScript Documentation](https://docs.ergoplatform.com/ergoscript/)
- [Autolykos PoW Paper](https://docs.ergoplatform.com/ErgoPow.pdf)
- [NiPoPoW Specification](https://nipopows.com/)
- [EIP-3: Wallet Derivation](https://github.com/ergoplatform/eips/blob/master/eip-0003.md)

## License

This project follows the same licensing as the Ergo platform (CC0 1.0 Universal for protocol, Apache 2.0 for implementation).
