//! # ergo-mempool
//!
//! Transaction mempool for the Ergo blockchain.
//!
//! This crate provides:
//! - Transaction storage with weight-based ordering
//! - Transaction dependency tracking (parent transactions get weight from children)
//! - Transaction validation before acceptance
//! - Double-spend detection
//! - Size limits and eviction policies
//!
//! ## Transaction Ordering
//!
//! Transactions are ordered by weight, not just fee. When a transaction spends
//! outputs of another mempool transaction, the parent's weight is increased by
//! the child's weight. This ensures:
//!
//! 1. Parent transactions are always processed before children
//! 2. Transaction chains are kept together during block creation
//! 3. Eviction of a parent also conceptually affects the child's viability
//!
//! This matches the Scala node's `OrderedTxPool` implementation.

mod error;
mod ordering;
mod pool;

pub use error::{MempoolError, MempoolResult};
pub use ordering::{FeeOrdering, WeightedTxId};
pub use pool::{Mempool, MempoolConfig, MempoolStats, PooledTransaction};

/// Default maximum mempool size in bytes.
pub const DEFAULT_MAX_SIZE: usize = 100 * 1024 * 1024; // 100 MB

/// Default maximum number of transactions.
pub const DEFAULT_MAX_TXS: usize = 10_000;

/// Default transaction expiry time in seconds.
pub const DEFAULT_TX_EXPIRY_SECS: u64 = 3600; // 1 hour
