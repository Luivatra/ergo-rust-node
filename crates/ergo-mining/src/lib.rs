//! # ergo-mining
//!
//! Mining support for the Ergo blockchain.
//!
//! This crate provides:
//! - Block candidate generation
//! - Transaction selection by fee
//! - Coinbase transaction creation
//! - External miner protocol (stratum-like)
//! - Internal CPU mining with worker threads
//! - Solution verification and submission

mod candidate;
mod coinbase;
mod error;
mod miner;
mod solver;
mod worker;

pub use candidate::{BlockCandidate, CandidateGenerator};
pub use coinbase::{
    block_reward_at_height, calculate_total_reward, emission_at_height, reemission_at_height,
    CoinbaseBuilder, EmissionParams,
};
pub use ergo_lib::ergotree_ir::chain::address::NetworkPrefix;
pub use error::{MiningError, MiningResult};
pub use miner::{Miner, MinerConfig, MiningStats};
pub use solver::AutolykosSolver;
pub use worker::{FoundSolution, MiningTask, MiningWorker, WorkerPool};

/// Default mining reward address (placeholder).
pub const DEFAULT_REWARD_ADDRESS: &str = "";

/// Maximum transactions per block.
pub const MAX_TRANSACTIONS_PER_BLOCK: usize = 1000;

/// Coinbase maturity (blocks before coinbase can be spent).
pub const COINBASE_MATURITY: u32 = 720;
