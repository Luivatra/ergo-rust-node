//! # ergo-state
//!
//! State management for the Ergo blockchain.
//!
//! This crate provides:
//! - UTXO state management with AVL+ tree
//! - Block history storage and chain selection
//! - State snapshots and rollback support
//! - Box (UTXO) retrieval and validation
//!
//! ## Architecture
//!
//! The state is organized into several components:
//! - `UtxoState`: Manages the current UTXO set using an AVL+ tree
//! - `History`: Tracks block headers and full blocks
//! - `StateManager`: Coordinates state transitions and rollbacks

mod error;
mod history;
mod manager;
mod utxo;

pub use error::{StateError, StateResult};
pub use history::{BlockSection, BlockStore, ChainSelection, HeaderStore, History, PruningConfig};
pub use manager::{StateManager, StateRootVerification};
pub use utxo::{BoxEntry, StateChange, UndoData, UtxoState};

use ergo_storage::ColumnFamily;

/// State-related column families.
pub mod columns {
    use super::ColumnFamily;

    /// UTXO boxes.
    pub const UTXO: ColumnFamily = ColumnFamily::Utxo;
    /// Block headers.
    pub const HEADERS: ColumnFamily = ColumnFamily::Headers;
    /// Block transactions.
    pub const BLOCK_TXS: ColumnFamily = ColumnFamily::BlockTransactions;
    /// Header chain (height -> block ID).
    pub const HEADER_CHAIN: ColumnFamily = ColumnFamily::HeaderChain;
    /// ErgoTree hash -> BoxIds index.
    pub const ERGOTREE_INDEX: ColumnFamily = ColumnFamily::ErgoTreeIndex;
    /// TokenId -> BoxIds index.
    pub const TOKEN_INDEX: ColumnFamily = ColumnFamily::TokenIndex;
}

/// State snapshot identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotId {
    /// Block height at snapshot.
    pub height: u32,
    /// State root at snapshot.
    pub state_root: Vec<u8>,
}
