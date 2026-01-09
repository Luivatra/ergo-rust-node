//! # ergo-storage
//!
//! Storage layer for the Ergo Rust Node.
//!
//! This crate provides a RocksDB-based storage abstraction with support for:
//! - Column families for different data types (blocks, headers, UTXO, etc.)
//! - Atomic batch writes
//! - Efficient key-value operations
//!
//! ## Column Families
//!
//! - `Headers`: Block headers indexed by BlockId
//! - `BlockTransactions`: Transaction data indexed by BlockId
//! - `Extensions`: Block extensions indexed by BlockId
//! - `AdProofs`: Authenticated data proofs indexed by BlockId
//! - `Utxo`: Unspent boxes indexed by BoxId
//! - `UtxoSnapshots`: UTXO snapshot metadata
//! - `HeaderChain`: Best chain headers indexed by height
//! - `TxIndex`: Transaction index (TxId -> BlockId + position)
//! - `Metadata`: Node metadata and configuration

mod batch;
mod database;
mod error;
pub mod indexer;

pub use batch::WriteBatch;
pub use database::{ColumnFamily, Database};
pub use error::{StorageError, StorageResult};
pub use indexer::{
    BalanceInfo, ExtraIndexer, IndexedErgoAddress, IndexedErgoBox, IndexedErgoTransaction,
    IndexedToken, IndexerConfig, IndexerState,
};

/// Storage trait for abstracting database operations.
///
/// This allows for easy testing with mock implementations.
pub trait Storage: Send + Sync {
    /// Get a value by key from a column family.
    fn get(&self, cf: ColumnFamily, key: &[u8]) -> StorageResult<Option<Vec<u8>>>;

    /// Put a key-value pair into a column family.
    fn put(&self, cf: ColumnFamily, key: &[u8], value: &[u8]) -> StorageResult<()>;

    /// Delete a key from a column family.
    fn delete(&self, cf: ColumnFamily, key: &[u8]) -> StorageResult<()>;

    /// Check if a key exists in a column family.
    fn contains(&self, cf: ColumnFamily, key: &[u8]) -> StorageResult<bool> {
        Ok(self.get(cf, key)?.is_some())
    }

    /// Execute a batch of writes atomically.
    fn write_batch(&self, batch: WriteBatch) -> StorageResult<()>;

    /// Create an iterator over a column family.
    fn iter(
        &self,
        cf: ColumnFamily,
    ) -> StorageResult<Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + '_>>;

    /// Get multiple values by keys from a column family.
    fn multi_get(&self, cf: ColumnFamily, keys: &[&[u8]]) -> StorageResult<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|k| self.get(cf, k)).collect()
    }
}
