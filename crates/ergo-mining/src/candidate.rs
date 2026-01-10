//! Block candidate generation.

use crate::{block_reward_at_height, MiningError, MiningResult, MAX_TRANSACTIONS_PER_BLOCK};
use ergo_chain_types::BlockId;
use ergo_consensus::{DifficultyAdjustment, HeaderForDifficulty};
use ergo_mempool::Mempool;
use ergo_state::StateManager;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// A block candidate ready for mining.
#[derive(Debug, Clone)]
pub struct BlockCandidate {
    /// Parent block ID.
    pub parent_id: Option<BlockId>,
    /// Block height.
    pub height: u32,
    /// Timestamp.
    pub timestamp: u64,
    /// Difficulty target (nBits).
    pub n_bits: u32,
    /// Transactions root.
    pub transactions_root: Vec<u8>,
    /// State root.
    pub state_root: Vec<u8>,
    /// Extension hash.
    pub extension_hash: Vec<u8>,
    /// Serialized header bytes (for mining).
    pub header_bytes: Vec<u8>,
    /// Transaction IDs included.
    pub transaction_ids: Vec<Vec<u8>>,
    /// Reward amount.
    pub reward: u64,
}

impl BlockCandidate {
    /// Get the message to mine (header bytes for hashing).
    pub fn mining_message(&self) -> &[u8] {
        &self.header_bytes
    }
}

/// Block candidate generator.
pub struct CandidateGenerator {
    /// State manager.
    state: Arc<StateManager>,
    /// Transaction mempool.
    mempool: Arc<Mempool>,
    /// Reward address.
    reward_address: RwLock<Option<String>>,
    /// Cached candidate.
    cached_candidate: RwLock<Option<BlockCandidate>>,
}

impl CandidateGenerator {
    /// Create a new candidate generator.
    pub fn new(state: Arc<StateManager>, mempool: Arc<Mempool>) -> Self {
        Self {
            state,
            mempool,
            reward_address: RwLock::new(None),
            cached_candidate: RwLock::new(None),
        }
    }

    /// Set the reward address.
    pub fn set_reward_address(&self, address: String) {
        *self.reward_address.write() = Some(address);
        // Invalidate cache
        *self.cached_candidate.write() = None;
    }

    /// Get the current reward address.
    pub fn reward_address(&self) -> Option<String> {
        self.reward_address.read().clone()
    }

    /// Generate a new block candidate.
    pub fn generate(&self) -> MiningResult<BlockCandidate> {
        // Validate that a reward address is set (used in coinbase transaction creation)
        let _reward_addr = self
            .reward_address
            .read()
            .clone()
            .ok_or(MiningError::NoRewardAddress)?;

        // Get current state
        let (_utxo_height, header_height) = self.state.heights();
        let best_header = self.state.history.best_header_id();
        let state_root = self.state.utxo.state_root();
        let new_height = header_height + 1;

        // Get transactions from mempool, sorted by fee
        let mempool_txs = self.mempool.get_by_fee(MAX_TRANSACTIONS_PER_BLOCK);

        // Convert mempool entries to actual transactions
        // For now, we just track the IDs and fees
        let tx_ids: Vec<Vec<u8>> = mempool_txs.iter().map(|tx| tx.id.clone()).collect();

        // Calculate total fees
        let total_fees: u64 = mempool_txs.iter().map(|tx| tx.fee).sum();

        // Calculate block reward using emission schedule
        let block_reward = block_reward_at_height(new_height);
        let total_reward = block_reward + total_fees;

        // Calculate difficulty from recent headers
        let n_bits = self.calculate_difficulty(new_height)?;

        // Compute transactions merkle root
        let transactions_root = self.compute_transactions_root(&tx_ids);

        // Compute extension hash (empty extension for now)
        let extension_hash = self.compute_extension_hash();

        // Create candidate
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Serialize header for mining
        let header_bytes = self.serialize_header_for_mining(
            best_header.as_ref(),
            new_height,
            timestamp,
            n_bits,
            &transactions_root,
            &state_root,
            &extension_hash,
        );

        let candidate = BlockCandidate {
            parent_id: best_header,
            height: new_height,
            timestamp,
            n_bits,
            transactions_root,
            state_root,
            extension_hash,
            header_bytes,
            transaction_ids: tx_ids,
            reward: total_reward,
        };

        // Cache candidate
        *self.cached_candidate.write() = Some(candidate.clone());

        info!(
            height = candidate.height,
            txs = candidate.transaction_ids.len(),
            reward = total_reward,
            n_bits = format!("{:#x}", n_bits),
            "Generated block candidate"
        );

        Ok(candidate)
    }

    /// Calculate difficulty for the next block based on recent headers.
    fn calculate_difficulty(&self, next_height: u32) -> MiningResult<u32> {
        // Get recent headers for difficulty calculation
        let headers_needed = 8 * 1024; // 8 epochs worth
        let start_height = next_height.saturating_sub(headers_needed);

        let mut headers_for_diff = Vec::new();

        for height in start_height..next_height {
            if let Ok(Some(header)) = self.state.history.headers.get_by_height(height) {
                headers_for_diff.push(HeaderForDifficulty {
                    height: header.height,
                    timestamp: header.timestamp,
                    n_bits: header.n_bits,
                });
            }
        }

        if headers_for_diff.is_empty() {
            // Genesis difficulty
            return Ok(0x1f00ffff);
        }

        let difficulty_adj = DifficultyAdjustment::new();
        match difficulty_adj.calculate_next_difficulty(&headers_for_diff, next_height) {
            Ok(n_bits) => Ok(n_bits),
            Err(e) => {
                warn!("Failed to calculate difficulty: {}, using previous", e);
                // Use the last header's difficulty
                Ok(headers_for_diff
                    .last()
                    .map(|h| h.n_bits)
                    .unwrap_or(0x1f00ffff))
            }
        }
    }

    /// Compute the merkle root of transactions.
    fn compute_transactions_root(&self, tx_ids: &[Vec<u8>]) -> Vec<u8> {
        if tx_ids.is_empty() {
            return vec![0u8; 32];
        }

        // Use BLAKE2b to hash transaction IDs into a merkle tree
        // This is a simplified implementation - the actual Ergo merkle tree
        // uses the full serialized transactions
        use blake2::{Blake2b, Digest};
        type Blake2b256 = Blake2b<blake2::digest::consts::U32>;

        // For now, just hash all tx IDs together
        // TODO: Implement proper merkle tree construction
        let mut hasher = Blake2b256::new();
        for tx_id in tx_ids {
            hasher.update(tx_id);
        }
        hasher.finalize().to_vec()
    }

    /// Compute the extension hash.
    fn compute_extension_hash(&self) -> Vec<u8> {
        // Empty extension hash
        vec![0u8; 32]
    }

    /// Serialize header fields for mining (without PoW solution).
    fn serialize_header_for_mining(
        &self,
        parent_id: Option<&BlockId>,
        height: u32,
        timestamp: u64,
        n_bits: u32,
        transactions_root: &[u8],
        state_root: &[u8],
        extension_hash: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(200);

        // Version (1 byte)
        bytes.push(2u8); // Autolykos v2

        // Parent ID (32 bytes)
        if let Some(parent) = parent_id {
            bytes.extend_from_slice(parent.0.as_ref());
        } else {
            bytes.extend_from_slice(&[0u8; 32]);
        }

        // AD proofs root (32 bytes) - placeholder
        bytes.extend_from_slice(&[0u8; 32]);

        // State root (32 bytes)
        if state_root.len() >= 32 {
            bytes.extend_from_slice(&state_root[..32]);
        } else {
            bytes.extend_from_slice(state_root);
            bytes.extend(std::iter::repeat(0u8).take(32 - state_root.len()));
        }

        // Transactions root (32 bytes)
        if transactions_root.len() >= 32 {
            bytes.extend_from_slice(&transactions_root[..32]);
        } else {
            bytes.extend_from_slice(transactions_root);
            bytes.extend(std::iter::repeat(0u8).take(32 - transactions_root.len()));
        }

        // Timestamp (8 bytes)
        bytes.extend_from_slice(&timestamp.to_be_bytes());

        // nBits (8 bytes)
        bytes.extend_from_slice(&n_bits.to_be_bytes());

        // Height (4 bytes)
        bytes.extend_from_slice(&height.to_be_bytes());

        // Extension hash (32 bytes)
        if extension_hash.len() >= 32 {
            bytes.extend_from_slice(&extension_hash[..32]);
        } else {
            bytes.extend_from_slice(extension_hash);
            bytes.extend(std::iter::repeat(0u8).take(32 - extension_hash.len()));
        }

        // Votes (3 bytes) - no votes
        bytes.extend_from_slice(&[0u8; 3]);

        bytes
    }

    /// Get cached candidate or generate new one.
    pub fn get_or_generate(&self) -> MiningResult<BlockCandidate> {
        // Check if cached candidate is still valid
        if let Some(cached) = self.cached_candidate.read().clone() {
            let (_, current_height) = self.state.heights();
            if cached.height == current_height + 1 {
                // Check if timestamp is recent (within 30 seconds)
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                if now - cached.timestamp < 30_000 {
                    debug!("Using cached candidate");
                    return Ok(cached);
                }
            }
        }

        self.generate()
    }

    /// Invalidate cached candidate.
    pub fn invalidate(&self) {
        *self.cached_candidate.write() = None;
    }
}
