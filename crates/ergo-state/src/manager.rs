//! State manager coordinating UTXO state and history.

use crate::{History, StateChange, StateError, StateResult, UtxoState};
use ergo_chain_types::Header;
use ergo_consensus::FullBlock;
use ergo_storage::Storage;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// State root verification mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRootVerification {
    /// Skip state root verification (for initial sync or testing).
    Skip,
    /// Log warnings but don't fail on mismatch.
    WarnOnly,
    /// Enforce state root verification (production mode).
    Enforce,
}

impl Default for StateRootVerification {
    fn default() -> Self {
        // Default to WarnOnly during development; change to Enforce for production
        Self::WarnOnly
    }
}

/// Coordinates state transitions between UTXO state and history.
pub struct StateManager {
    /// UTXO state.
    pub utxo: UtxoState,
    /// Block history.
    pub history: History,
    /// State root verification mode.
    state_root_verification: StateRootVerification,
}

impl StateManager {
    /// Create a new state manager with the given storage.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            utxo: UtxoState::new(Arc::clone(&storage)),
            history: History::new(storage),
            state_root_verification: StateRootVerification::default(),
        }
    }

    /// Create a new state manager with custom verification mode.
    pub fn with_verification(
        storage: Arc<dyn Storage>,
        verification: StateRootVerification,
    ) -> Self {
        Self {
            utxo: UtxoState::new(Arc::clone(&storage)),
            history: History::new(storage),
            state_root_verification: verification,
        }
    }

    /// Initialize from existing storage.
    pub fn init_from_storage(storage: Arc<dyn Storage>) -> StateResult<Self> {
        let utxo = UtxoState::init_from_storage(Arc::clone(&storage))?;
        let history = History::init_from_storage(storage)?;

        info!(
            utxo_height = utxo.height(),
            header_height = history.best_height(),
            "State manager initialized"
        );

        Ok(Self {
            utxo,
            history,
            state_root_verification: StateRootVerification::default(),
        })
    }

    /// Set the state root verification mode.
    pub fn set_verification_mode(&mut self, mode: StateRootVerification) {
        self.state_root_verification = mode;
    }

    /// Get current state heights.
    pub fn heights(&self) -> (u32, u32) {
        (self.utxo.height(), self.history.best_height())
    }

    /// Check if state is synchronized.
    /// We consider ourselves synced if:
    /// 1. We have headers (header height > 0)
    /// 2. Our full block height matches our header height (all blocks downloaded)
    /// 3. We have at least some blocks (full block height > 0)
    /// During initial header sync (no blocks yet), we report as NOT synced.
    pub fn is_synced(&self) -> bool {
        let header_height = self.history.best_height();
        let full_block_height = self.history.best_full_block_height();

        // Must have blocks and headers must match full blocks
        full_block_height > 0 && full_block_height == header_height
    }

    /// Get the gap between headers and full blocks.
    pub fn chain_gap(&self) -> u32 {
        self.history.chain_gap()
    }

    /// Apply a validated header to the chain.
    #[instrument(skip(self, header), fields(height = header.height))]
    pub fn apply_header(&self, header: Header) -> StateResult<crate::ChainSelection> {
        self.history.append_header(header)
    }

    /// Apply a validated header to the chain using raw bytes.
    /// This preserves correct ID computation for headers with BigInt serialization issues.
    #[instrument(skip(self, header, raw_bytes), fields(height = header.height))]
    pub fn apply_header_with_bytes(
        &self,
        header: Header,
        raw_bytes: &[u8],
    ) -> StateResult<crate::ChainSelection> {
        self.history.append_header_with_bytes(header, raw_bytes)
    }

    /// Apply a validated full block to the state.
    #[instrument(skip(self, block, state_change), fields(height = block.height()))]
    pub fn apply_block(
        &self,
        block: FullBlock,
        state_change: StateChange,
    ) -> StateResult<crate::ChainSelection> {
        let height = block.height();
        let expected_state_root: Vec<u8> = block.header.state_root.0.as_ref().to_vec();

        // Verify this block extends current state
        let utxo_height = self.utxo.height();
        if height != utxo_height + 1 {
            return Err(StateError::InvalidTransition(format!(
                "Expected height {}, got {}",
                utxo_height + 1,
                height
            )));
        }

        // Store the full block (header, transactions, extension, proofs)
        let selection = self.history.append_full_block(block)?;

        // Apply state change to UTXO set
        self.utxo.apply_change(&state_change, height)?;

        // Verify state root after application (based on verification mode)
        if self.state_root_verification != StateRootVerification::Skip {
            let computed_state_root = self.utxo.state_root();

            if computed_state_root.is_empty() || computed_state_root.iter().all(|&b| b == 0) {
                // State root not yet computed - this is expected until AVL tree is fully initialized
                debug!(
                    height,
                    expected_state_root = %hex::encode(&expected_state_root),
                    "State root verification skipped (AVL tree not initialized)"
                );
            } else {
                // Compare computed state root with expected
                // ADDigest is 33 bytes (32-byte hash + 1-byte height flag)
                // Our computed root is 32 bytes, so we compare the hash portion
                let expected_hash: &[u8] = if expected_state_root.len() == 33 {
                    &expected_state_root[..32]
                } else {
                    &expected_state_root
                };

                if computed_state_root.as_slice() != expected_hash {
                    let error_msg = format!(
                        "State root mismatch at height {}: expected {}, computed {}",
                        height,
                        hex::encode(expected_hash),
                        hex::encode(&computed_state_root)
                    );

                    match self.state_root_verification {
                        StateRootVerification::Enforce => {
                            // Rollback the state change since verification failed
                            if let Err(e) = self.utxo.rollback(utxo_height) {
                                warn!(height, error = %e, "Failed to rollback after state root mismatch");
                            }
                            return Err(StateError::StateRootMismatch {
                                height,
                                expected: hex::encode(expected_hash),
                                computed: hex::encode(&computed_state_root),
                            });
                        }
                        StateRootVerification::WarnOnly => {
                            warn!(height, expected = %hex::encode(expected_hash), computed = %hex::encode(&computed_state_root), "State root mismatch (warning only)");
                        }
                        StateRootVerification::Skip => {
                            // Already checked above, but included for completeness
                        }
                    }
                } else {
                    debug!(
                        height,
                        state_root = %hex::encode(&computed_state_root),
                        "State root verified successfully"
                    );
                }
            }
        }

        info!(height, "Block applied to state");
        Ok(selection)
    }

    /// Apply state change without storing block (for validation).
    pub fn apply_state_change(&self, state_change: &StateChange, height: u32) -> StateResult<()> {
        self.utxo.apply_change(state_change, height)
    }

    /// Apply multiple blocks in a single batched write operation.
    /// This is significantly more efficient than applying blocks individually during sync.
    ///
    /// Each element is a tuple of (FullBlock, StateChange).
    /// Blocks must be in ascending height order and contiguous.
    #[instrument(skip(self, blocks), fields(count = blocks.len()))]
    pub fn apply_blocks_batched(
        &self,
        blocks: Vec<(FullBlock, StateChange)>,
    ) -> StateResult<crate::ChainSelection> {
        if blocks.is_empty() {
            return Ok(crate::ChainSelection::Ignored);
        }

        let first_height = blocks[0].0.height();
        let last_height = blocks[blocks.len() - 1].0.height();
        let count = blocks.len();

        // Verify this batch extends current state
        let utxo_height = self.utxo.height();
        if first_height != utxo_height + 1 {
            return Err(StateError::InvalidTransition(format!(
                "Expected first block at height {}, got {}",
                utxo_height + 1,
                first_height
            )));
        }

        // Verify blocks are contiguous
        for (i, (block, _)) in blocks.iter().enumerate() {
            let expected_height = first_height + i as u32;
            if block.height() != expected_height {
                return Err(StateError::InvalidTransition(format!(
                    "Non-contiguous block: expected height {}, got {}",
                    expected_height,
                    block.height()
                )));
            }
        }

        info!(
            first_height,
            last_height, count, "Applying {} blocks in batched write", count
        );

        // Create a single batch for ALL blocks
        let mut batch = ergo_storage::WriteBatch::new();

        // Get starting cumulative difficulty
        let mut cumulative_difficulty = if first_height <= 2 {
            num_bigint::BigUint::from(0u32)
        } else {
            let parent_id = &blocks[0].0.header.parent_id;
            self.history
                .get_cumulative_difficulty(parent_id)?
                .unwrap_or_else(|| num_bigint::BigUint::from(0u32))
        };

        // Add all blocks to the batch
        let mut final_block_id = blocks[0].0.id();
        for (block, state_change) in &blocks {
            let height = block.height();
            final_block_id = block.id();

            // Add block data to history batch
            cumulative_difficulty =
                self.history
                    .add_block_to_batch(&mut batch, block, &cumulative_difficulty)?;

            // Add UTXO changes to batch
            self.utxo
                .add_change_to_batch(&mut batch, state_change, height)?;
        }

        // Add final metadata updates
        let current_best_difficulty = self.history.best_cumulative_difficulty();
        let selection = if cumulative_difficulty > current_best_difficulty {
            batch.put(
                ergo_storage::ColumnFamily::Metadata,
                b"best_header_id",
                final_block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ergo_storage::ColumnFamily::Metadata,
                b"best_height",
                last_height.to_be_bytes().to_vec(),
            );
            batch.put(
                ergo_storage::ColumnFamily::Metadata,
                b"best_cumulative_difficulty",
                cumulative_difficulty.to_bytes_be(),
            );
            crate::ChainSelection::Extended
        } else {
            crate::ChainSelection::Ignored
        };

        // Update best full block metadata
        let current_full_height = self.history.best_full_block_height();
        if last_height > current_full_height {
            batch.put(
                ergo_storage::ColumnFamily::Metadata,
                b"best_full_block_id",
                final_block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ergo_storage::ColumnFamily::Metadata,
                b"best_full_block_height",
                last_height.to_be_bytes().to_vec(),
            );
        }

        // Execute single batch write for ALL blocks
        self.history.execute_batch(batch)?;

        // Update in-memory state after successful write
        self.history
            .update_in_memory_state(final_block_id, last_height, cumulative_difficulty);
        self.utxo.update_height_in_memory(last_height);

        info!(
            first_height,
            last_height, count, "Successfully applied {} blocks in single batch", count
        );

        Ok(selection)
    }

    /// Rollback state to a previous height.
    #[instrument(skip(self))]
    pub fn rollback_to(&self, height: u32) -> StateResult<()> {
        // Rollback UTXO state
        self.utxo.rollback(height)?;

        info!(height, "State rolled back");
        Ok(())
    }

    /// Get headers for sync (header locator).
    pub fn get_header_locator(&self) -> StateResult<Vec<ergo_chain_types::BlockId>> {
        self.history.get_header_locator()
    }

    /// Get headers in a range.
    pub fn get_headers(&self, from_height: u32, count: u32) -> StateResult<Vec<Header>> {
        self.history.headers.get_range(from_height, count)
    }

    /// Check if we have a header.
    pub fn has_header(&self, id: &ergo_chain_types::BlockId) -> StateResult<bool> {
        self.history.headers.contains(id)
    }

    /// Get a header by ID.
    pub fn get_header(&self, id: &ergo_chain_types::BlockId) -> StateResult<Option<Header>> {
        self.history.headers.get(id)
    }

    /// Get a full block by ID.
    pub fn get_full_block(&self, id: &ergo_chain_types::BlockId) -> StateResult<Option<FullBlock>> {
        self.history.get_full_block(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergo_storage::Database;
    use tempfile::TempDir;

    #[test]
    fn test_state_manager_init() {
        let tmp = TempDir::new().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        let manager = StateManager::new(Arc::new(db));

        assert_eq!(manager.heights(), (0, 0));
    }
}
