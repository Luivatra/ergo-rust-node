//! Block history management.
//!
//! Tracks block headers, full blocks, and manages chain selection.

use crate::{columns, StateError, StateResult};
use ergo_chain_types::{BlockId, Digest32, Header};
use ergo_consensus::{nbits_to_difficulty, ADProofs, BlockTransactions, Extension, FullBlock};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_storage::{ColumnFamily, Storage, WriteBatch};
use num_bigint::BigUint;
use parking_lot::RwLock;
use sigma_ser::ScorexSerializable;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// Chain selection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainSelection {
    /// New header extends the best chain.
    Extended,
    /// New header causes a chain reorganization.
    Reorg {
        /// Common ancestor height.
        fork_height: u32,
        /// Number of blocks to rollback.
        rollback_count: u32,
    },
    /// New header is on a shorter/equal fork (ignored).
    Ignored,
}

/// Block section types stored in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSection {
    Header,
    BlockTransactions,
    Extension,
    ADProofs,
}

/// Header storage and indexing.
pub struct HeaderStore {
    storage: Arc<dyn Storage>,
}

impl HeaderStore {
    /// Create a new header store.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    /// Store a header using raw bytes (batched version).
    /// This preserves the original serialization to ensure correct ID computation on retrieval.
    pub fn put_with_bytes_batched(
        &self,
        batch: &mut WriteBatch,
        header: &Header,
        raw_bytes: &[u8],
    ) {
        let id = header.id.0.as_ref();

        // Store the ORIGINAL bytes, not re-serialized bytes
        batch.put(columns::HEADERS, id.to_vec(), raw_bytes.to_vec());

        // Store height -> block_id index
        let height_key = header.height.to_be_bytes();
        batch.put(columns::HEADER_CHAIN, height_key.to_vec(), id.to_vec());
    }

    /// Store a header using raw bytes.
    /// This preserves the original serialization to ensure correct ID computation on retrieval.
    /// The header's ID field should already be set to the correct ID (computed from raw_bytes).
    pub fn put_with_bytes(&self, header: &Header, raw_bytes: &[u8]) -> StateResult<()> {
        let mut batch = WriteBatch::new();
        self.put_with_bytes_batched(&mut batch, header, raw_bytes);
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Store a header (batched version).
    pub fn put_batched(&self, batch: &mut WriteBatch, header: &Header) -> StateResult<()> {
        let id = header.id.0.as_ref();
        let bytes = header
            .scorex_serialize_bytes()
            .map_err(|e| StateError::Serialization(e.to_string()))?;

        batch.put(columns::HEADERS, id.to_vec(), bytes);
        let height_key = header.height.to_be_bytes();
        batch.put(columns::HEADER_CHAIN, height_key.to_vec(), id.to_vec());
        Ok(())
    }

    /// Store a header (legacy method - uses re-serialization).
    /// Prefer put_with_bytes when original bytes are available.
    pub fn put(&self, header: &Header) -> StateResult<()> {
        let mut batch = WriteBatch::new();
        self.put_batched(&mut batch, header)?;
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Compute the correct header ID from raw bytes using blake2b-256.
    /// This is needed because sigma-rust's deserialization can produce wrong IDs
    /// due to BigInt serialization differences in Autolykos v1 headers.
    fn compute_header_id(data: &[u8]) -> [u8; 32] {
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(data);
        let result = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&result);
        arr
    }

    /// Get a header by ID.
    pub fn get(&self, id: &BlockId) -> StateResult<Option<Header>> {
        match self.storage.get(columns::HEADERS, id.0.as_ref())? {
            Some(bytes) => {
                let mut header = Header::scorex_parse_bytes(&bytes)
                    .map_err(|e| StateError::Serialization(e.to_string()))?;
                // Fix the header ID - compute from stored bytes to handle BigInt serialization issues
                let correct_id = Self::compute_header_id(&bytes);
                header.id = BlockId(Digest32::from(correct_id));
                Ok(Some(header))
            }
            None => Ok(None),
        }
    }

    /// Get a header by height.
    pub fn get_by_height(&self, height: u32) -> StateResult<Option<Header>> {
        let height_key = height.to_be_bytes();
        match self.storage.get(columns::HEADER_CHAIN, &height_key)? {
            Some(id_bytes) => {
                if id_bytes.len() != 32 {
                    return Err(StateError::Serialization("Invalid block ID length".into()));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&id_bytes);
                let id = BlockId(Digest32::from(arr));
                self.get(&id)
            }
            None => Ok(None),
        }
    }

    /// Check if a header exists.
    pub fn contains(&self, id: &BlockId) -> StateResult<bool> {
        Ok(self.storage.contains(columns::HEADERS, id.0.as_ref())?)
    }

    /// Get headers in a range.
    pub fn get_range(&self, from_height: u32, count: u32) -> StateResult<Vec<Header>> {
        let mut headers = Vec::with_capacity(count as usize);
        for height in from_height..from_height.saturating_add(count) {
            match self.get_by_height(height)? {
                Some(header) => headers.push(header),
                None => break,
            }
        }
        Ok(headers)
    }
}

/// Block body storage (transactions, extension, AD proofs).
pub struct BlockStore {
    storage: Arc<dyn Storage>,
}

impl BlockStore {
    /// Create a new block store.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    /// Serialize block transactions to bytes.
    fn serialize_transactions(txs: &BlockTransactions) -> StateResult<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(txs.txs.len() as u32).to_be_bytes());

        for tx in &txs.txs {
            let tx_bytes = tx
                .sigma_serialize_bytes()
                .map_err(|e| StateError::Serialization(e.to_string()))?;
            bytes.extend_from_slice(&(tx_bytes.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&tx_bytes);
        }
        Ok(bytes)
    }

    /// Store block transactions (batched version).
    pub fn put_transactions_batched(
        &self,
        batch: &mut WriteBatch,
        block_id: &BlockId,
        txs: &BlockTransactions,
    ) -> StateResult<()> {
        let bytes = Self::serialize_transactions(txs)?;
        batch.put(columns::BLOCK_TXS, block_id.0.as_ref().to_vec(), bytes);
        Ok(())
    }

    /// Store block transactions.
    pub fn put_transactions(&self, block_id: &BlockId, txs: &BlockTransactions) -> StateResult<()> {
        let bytes = Self::serialize_transactions(txs)?;
        self.storage
            .put(columns::BLOCK_TXS, block_id.0.as_ref(), &bytes)?;
        Ok(())
    }

    /// Get block transactions.
    pub fn get_transactions(&self, block_id: &BlockId) -> StateResult<Option<BlockTransactions>> {
        match self.storage.get(columns::BLOCK_TXS, block_id.0.as_ref())? {
            Some(bytes) => {
                if bytes.len() < 4 {
                    return Err(StateError::Serialization(
                        "Transaction data too short".into(),
                    ));
                }

                let count = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
                let mut txs = Vec::with_capacity(count);
                let mut offset = 4;

                for _ in 0..count {
                    if offset + 4 > bytes.len() {
                        return Err(StateError::Serialization("Truncated tx data".into()));
                    }
                    let tx_len =
                        u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;

                    if offset + tx_len > bytes.len() {
                        return Err(StateError::Serialization("Truncated tx bytes".into()));
                    }

                    let tx = ergo_lib::chain::transaction::Transaction::sigma_parse_bytes(
                        &bytes[offset..offset + tx_len],
                    )
                    .map_err(|e| StateError::Serialization(e.to_string()))?;
                    txs.push(tx);
                    offset += tx_len;
                }

                Ok(Some(BlockTransactions::new(block_id.clone(), txs)))
            }
            None => Ok(None),
        }
    }

    /// Check if block transactions exist.
    pub fn has_transactions(&self, block_id: &BlockId) -> StateResult<bool> {
        Ok(self
            .storage
            .contains(columns::BLOCK_TXS, block_id.0.as_ref())?)
    }

    /// Serialize extension to bytes.
    fn serialize_extension(extension: &Extension) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(extension.fields.len() as u32).to_be_bytes());

        for field in &extension.fields {
            bytes.extend_from_slice(&field.key);
            bytes.extend_from_slice(&(field.value.len() as u16).to_be_bytes());
            bytes.extend_from_slice(&field.value);
        }
        bytes
    }

    /// Store extension (batched version).
    pub fn put_extension_batched(&self, batch: &mut WriteBatch, extension: &Extension) {
        let bytes = Self::serialize_extension(extension);
        batch.put(
            ColumnFamily::Extensions,
            extension.header_id.0.as_ref().to_vec(),
            bytes,
        );
    }

    /// Store extension.
    pub fn put_extension(&self, extension: &Extension) -> StateResult<()> {
        let bytes = Self::serialize_extension(extension);
        self.storage.put(
            ColumnFamily::Extensions,
            extension.header_id.0.as_ref(),
            &bytes,
        )?;
        Ok(())
    }

    /// Store AD proofs (batched version).
    pub fn put_ad_proofs_batched(&self, batch: &mut WriteBatch, proofs: &ADProofs) {
        batch.put(
            ColumnFamily::AdProofs,
            proofs.header_id.0.as_ref().to_vec(),
            proofs.proof_bytes.clone(),
        );
    }

    /// Store AD proofs.
    pub fn put_ad_proofs(&self, proofs: &ADProofs) -> StateResult<()> {
        self.storage.put(
            ColumnFamily::AdProofs,
            proofs.header_id.0.as_ref(),
            &proofs.proof_bytes,
        )?;
        Ok(())
    }

    /// Get AD proofs.
    pub fn get_ad_proofs(&self, block_id: &BlockId) -> StateResult<Option<ADProofs>> {
        match self
            .storage
            .get(ColumnFamily::AdProofs, block_id.0.as_ref())?
        {
            Some(bytes) => Ok(Some(ADProofs::new(block_id.clone(), bytes))),
            None => Ok(None),
        }
    }
}

/// Block history manager.
pub struct History {
    /// Header storage.
    pub headers: HeaderStore,
    /// Block body storage.
    pub blocks: BlockStore,
    /// Storage backend.
    storage: Arc<dyn Storage>,
    /// Best header ID.
    best_header_id: RwLock<Option<BlockId>>,
    /// Best header height.
    best_height: RwLock<u32>,
    /// Best cumulative difficulty.
    best_cumulative_difficulty: RwLock<BigUint>,
    /// Best full block ID (may lag behind best header).
    best_full_block_id: RwLock<Option<BlockId>>,
    /// Best full block height.
    best_full_block_height: RwLock<u32>,
}

impl History {
    /// Create a new history manager.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            headers: HeaderStore::new(Arc::clone(&storage)),
            blocks: BlockStore::new(Arc::clone(&storage)),
            storage,
            best_header_id: RwLock::new(None),
            best_height: RwLock::new(0),
            best_cumulative_difficulty: RwLock::new(BigUint::from(0u32)),
            best_full_block_id: RwLock::new(None),
            best_full_block_height: RwLock::new(0),
        }
    }

    /// Initialize from storage.
    pub fn init_from_storage(storage: Arc<dyn Storage>) -> StateResult<Self> {
        let history = Self::new(storage);

        // Load best header ID
        if let Some(id_bytes) = history
            .storage
            .get(ColumnFamily::Metadata, b"best_header_id")?
        {
            if id_bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&id_bytes);
                *history.best_header_id.write() = Some(BlockId(Digest32::from(arr)));
            }
        }

        // Load best height
        if let Some(height_bytes) = history
            .storage
            .get(ColumnFamily::Metadata, b"best_height")?
        {
            if height_bytes.len() >= 4 {
                let height = u32::from_be_bytes(height_bytes[0..4].try_into().unwrap());
                *history.best_height.write() = height;
            }
        }

        // Load best cumulative difficulty
        if let Some(diff_bytes) = history
            .storage
            .get(ColumnFamily::Metadata, b"best_cumulative_difficulty")?
        {
            let difficulty = BigUint::from_bytes_be(&diff_bytes);
            *history.best_cumulative_difficulty.write() = difficulty;
        }

        // Load best full block
        if let Some(id_bytes) = history
            .storage
            .get(ColumnFamily::Metadata, b"best_full_block_id")?
        {
            if id_bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&id_bytes);
                *history.best_full_block_id.write() = Some(BlockId(Digest32::from(arr)));
            }
        }

        if let Some(height_bytes) = history
            .storage
            .get(ColumnFamily::Metadata, b"best_full_block_height")?
        {
            if height_bytes.len() >= 4 {
                let height = u32::from_be_bytes(height_bytes[0..4].try_into().unwrap());
                *history.best_full_block_height.write() = height;
            }
        }

        info!(
            best_height = history.best_height(),
            best_full_block_height = history.best_full_block_height(),
            best_cumulative_difficulty = %history.best_cumulative_difficulty(),
            "History initialized from storage"
        );

        Ok(history)
    }

    /// Get the best header ID.
    pub fn best_header_id(&self) -> Option<BlockId> {
        self.best_header_id.read().clone()
    }

    /// Get the best header height.
    pub fn best_height(&self) -> u32 {
        *self.best_height.read()
    }

    /// Get the best cumulative difficulty.
    pub fn best_cumulative_difficulty(&self) -> BigUint {
        self.best_cumulative_difficulty.read().clone()
    }

    /// Get the best full block height.
    pub fn best_full_block_height(&self) -> u32 {
        *self.best_full_block_height.read()
    }

    /// Get the best header.
    pub fn best_header(&self) -> StateResult<Option<Header>> {
        match self.best_header_id() {
            Some(id) => self.headers.get(&id),
            None => Ok(None),
        }
    }

    /// Get cumulative difficulty for a block.
    pub fn get_cumulative_difficulty(&self, block_id: &BlockId) -> StateResult<Option<BigUint>> {
        match self
            .storage
            .get(ColumnFamily::CumulativeDifficulty, block_id.0.as_ref())?
        {
            Some(bytes) => Ok(Some(BigUint::from_bytes_be(&bytes))),
            None => Ok(None),
        }
    }

    /// Store cumulative difficulty for a block.
    fn store_cumulative_difficulty(
        &self,
        batch: &mut WriteBatch,
        block_id: &BlockId,
        difficulty: &BigUint,
    ) {
        batch.put(
            ColumnFamily::CumulativeDifficulty,
            block_id.0.as_ref().to_vec(),
            difficulty.to_bytes_be(),
        );
    }

    /// Calculate difficulty from nBits (compact target representation).
    fn difficulty_from_nbits(n_bits: u64) -> BigUint {
        // The difficulty is the ratio of the maximum target to the current target
        nbits_to_difficulty(n_bits)
    }

    /// Append a new header.
    #[instrument(skip(self, header), fields(height = header.height, id = %header.id))]
    pub fn append_header(&self, header: Header) -> StateResult<ChainSelection> {
        self.append_header_internal(header, None)
    }

    /// Append a header to the chain using the original raw bytes.
    /// This preserves correct ID computation for headers with BigInt serialization issues.
    pub fn append_header_with_bytes(
        &self,
        header: Header,
        raw_bytes: &[u8],
    ) -> StateResult<ChainSelection> {
        self.append_header_internal(header, Some(raw_bytes))
    }

    fn append_header_internal(
        &self,
        header: Header,
        raw_bytes: Option<&[u8]>,
    ) -> StateResult<ChainSelection> {
        let header_id = header.id.clone();
        let parent_id = header.parent_id.clone();
        let height = header.height;
        let n_bits = header.n_bits;

        // Check if we already have this header (idempotent operation)
        if self.headers.contains(&header_id)? {
            info!(height, id = %header_id, "Header already exists in database, skipping");
            return Ok(ChainSelection::Ignored);
        }

        // Check parent exists (except for genesis and first block after genesis)
        // Genesis is at height=1, so height=2 headers have genesis as parent which isn't stored
        if height > 2 && !self.headers.contains(&parent_id)? {
            return Err(StateError::HeaderNotFound(format!("{}", parent_id)));
        }

        // Calculate cumulative difficulty for this header
        let block_difficulty = Self::difficulty_from_nbits(n_bits);
        let parent_cumulative = if height <= 2 {
            // Genesis or first block - no parent cumulative difficulty
            BigUint::from(0u32)
        } else {
            self.get_cumulative_difficulty(&parent_id)?
                .unwrap_or_else(|| BigUint::from(0u32))
        };
        let cumulative_difficulty = &parent_cumulative + &block_difficulty;

        // Store the header - use raw bytes if available for correct ID preservation
        if let Some(bytes) = raw_bytes {
            self.headers.put_with_bytes(&header, bytes)?;
        } else {
            self.headers.put(&header)?;
        }

        // Store cumulative difficulty for this block
        let mut batch = WriteBatch::new();
        self.store_cumulative_difficulty(&mut batch, &header_id, &cumulative_difficulty);

        // Determine chain selection based on cumulative difficulty
        let current_best_difficulty = self.best_cumulative_difficulty();
        let selection = if cumulative_difficulty > current_best_difficulty {
            // This chain has more cumulative work - switch to it
            let current_height = self.best_height();

            batch.put(
                ColumnFamily::Metadata,
                b"best_header_id",
                header_id.0.as_ref().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_height",
                height.to_be_bytes().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_cumulative_difficulty",
                cumulative_difficulty.to_bytes_be(),
            );

            self.storage.write_batch(batch)?;

            *self.best_header_id.write() = Some(header_id.clone());
            *self.best_height.write() = height;
            *self.best_cumulative_difficulty.write() = cumulative_difficulty.clone();

            if height <= current_height && current_height > 0 {
                // This is a reorg - we're switching to a different chain
                // Find the fork point by walking back from both chains
                let fork_height = self.find_fork_height(&header_id, current_height)?;
                let rollback_count = current_height - fork_height;

                warn!(
                    height,
                    current_height,
                    fork_height,
                    rollback_count,
                    cumulative_difficulty = %cumulative_difficulty,
                    previous_difficulty = %current_best_difficulty,
                    "Chain reorganization due to higher cumulative difficulty"
                );

                ChainSelection::Reorg {
                    fork_height,
                    rollback_count,
                }
            } else {
                info!(
                    height,
                    %header_id,
                    cumulative_difficulty = %cumulative_difficulty,
                    "New best header"
                );
                ChainSelection::Extended
            }
        } else {
            // Store the batch (just the cumulative difficulty for this header)
            self.storage.write_batch(batch)?;

            debug!(
                height,
                cumulative_difficulty = %cumulative_difficulty,
                best_difficulty = %current_best_difficulty,
                "Header on chain with less work, ignored"
            );
            ChainSelection::Ignored
        };

        Ok(selection)
    }

    /// Find the fork height between a new header and the current best chain.
    fn find_fork_height(&self, new_header_id: &BlockId, _current_height: u32) -> StateResult<u32> {
        // Get the new header to find its height
        let new_header = self
            .headers
            .get(new_header_id)?
            .ok_or_else(|| StateError::HeaderNotFound(format!("{}", new_header_id)))?;

        // Walk back from both chains to find common ancestor
        let mut new_id = new_header.parent_id.clone();
        let mut new_height = new_header.height.saturating_sub(1);

        while new_height > 0 {
            // Check if this block is on the main chain
            if let Some(main_header) = self.headers.get_by_height(new_height)? {
                if main_header.id == new_id {
                    return Ok(new_height);
                }
            }

            // Walk back on the new chain
            if let Some(header) = self.headers.get(&new_id)? {
                new_id = header.parent_id.clone();
                new_height = new_height.saturating_sub(1);
            } else {
                break;
            }
        }

        // If we couldn't find a common ancestor, assume genesis
        Ok(1)
    }

    /// Append a full block using a single batched write for all data.
    /// This is more efficient than append_full_block as it minimizes disk I/O.
    pub fn append_full_block(&self, block: FullBlock) -> StateResult<ChainSelection> {
        let block_id = block.id();
        let height = block.height();
        let header = block.header.clone();

        // Create a single batch for ALL block data
        let mut batch = WriteBatch::new();

        // Add header to batch
        self.headers.put_batched(&mut batch, &header)?;

        // Add cumulative difficulty
        let parent_id = header.parent_id.clone();
        let block_difficulty = Self::difficulty_from_nbits(header.n_bits);
        let parent_cumulative = if height <= 2 {
            BigUint::from(0u32)
        } else {
            self.get_cumulative_difficulty(&parent_id)?
                .unwrap_or_else(|| BigUint::from(0u32))
        };
        let cumulative_difficulty = &parent_cumulative + &block_difficulty;
        self.store_cumulative_difficulty(&mut batch, &block_id, &cumulative_difficulty);

        // Add block sections to batch
        self.blocks
            .put_transactions_batched(&mut batch, &block_id, &block.transactions)?;
        self.blocks
            .put_extension_batched(&mut batch, &block.extension);

        if let Some(ad_proofs) = &block.ad_proofs {
            self.blocks.put_ad_proofs_batched(&mut batch, ad_proofs);
        }

        // Determine chain selection
        let current_best_difficulty = self.best_cumulative_difficulty();
        let current_height = self.best_height();

        let selection = if cumulative_difficulty > current_best_difficulty {
            // Update best header metadata
            batch.put(
                ColumnFamily::Metadata,
                b"best_header_id",
                block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_height",
                height.to_be_bytes().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_cumulative_difficulty",
                cumulative_difficulty.to_bytes_be(),
            );

            if height <= current_height && current_height > 0 {
                ChainSelection::Reorg {
                    fork_height: 1, // Will be computed after batch write
                    rollback_count: current_height - 1,
                }
            } else {
                ChainSelection::Extended
            }
        } else {
            ChainSelection::Ignored
        };

        // Update best full block if this extends it
        let current_full_height = self.best_full_block_height();
        let update_full_block = height > current_full_height;
        if update_full_block {
            batch.put(
                ColumnFamily::Metadata,
                b"best_full_block_id",
                block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_full_block_height",
                height.to_be_bytes().to_vec(),
            );
        }

        // Execute single batch write for all data
        self.storage.write_batch(batch)?;

        // Update in-memory state after successful write
        if cumulative_difficulty > current_best_difficulty {
            *self.best_header_id.write() = Some(block_id.clone());
            *self.best_height.write() = height;
            *self.best_cumulative_difficulty.write() = cumulative_difficulty;
        }

        if update_full_block {
            *self.best_full_block_id.write() = Some(block_id.clone());
            *self.best_full_block_height.write() = height;
            info!(height, %block_id, "New best full block");
        }

        Ok(selection)
    }

    /// Add a full block's data to an external batch without executing it.
    /// Returns the cumulative difficulty for this block.
    /// Call `finalize_block_batch` after adding all blocks to commit and update in-memory state.
    pub fn add_block_to_batch(
        &self,
        batch: &mut WriteBatch,
        block: &FullBlock,
        parent_cumulative: &BigUint,
    ) -> StateResult<BigUint> {
        let block_id = block.id();
        let header = &block.header;

        // Add header to batch
        self.headers.put_batched(batch, header)?;

        // Calculate and store cumulative difficulty
        let block_difficulty = Self::difficulty_from_nbits(header.n_bits);
        let cumulative_difficulty = parent_cumulative + &block_difficulty;
        self.store_cumulative_difficulty(batch, &block_id, &cumulative_difficulty);

        // Add block sections to batch
        self.blocks
            .put_transactions_batched(batch, &block_id, &block.transactions)?;
        self.blocks.put_extension_batched(batch, &block.extension);

        if let Some(ad_proofs) = &block.ad_proofs {
            self.blocks.put_ad_proofs_batched(batch, ad_proofs);
        }

        Ok(cumulative_difficulty)
    }

    /// Finalize a batch of blocks by adding metadata updates and executing the batch.
    /// Updates in-memory state after successful write.
    pub fn finalize_block_batch(
        &self,
        mut batch: WriteBatch,
        final_block_id: &BlockId,
        final_height: u32,
        final_cumulative_difficulty: &BigUint,
    ) -> StateResult<ChainSelection> {
        let current_best_difficulty = self.best_cumulative_difficulty();
        let current_height = self.best_height();

        let selection = if final_cumulative_difficulty > &current_best_difficulty {
            // Update best header metadata
            batch.put(
                ColumnFamily::Metadata,
                b"best_header_id",
                final_block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_height",
                final_height.to_be_bytes().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_cumulative_difficulty",
                final_cumulative_difficulty.to_bytes_be(),
            );

            if final_height <= current_height && current_height > 0 {
                ChainSelection::Reorg {
                    fork_height: 1,
                    rollback_count: current_height - 1,
                }
            } else {
                ChainSelection::Extended
            }
        } else {
            ChainSelection::Ignored
        };

        // Update best full block metadata
        let current_full_height = self.best_full_block_height();
        let update_full_block = final_height > current_full_height;
        if update_full_block {
            batch.put(
                ColumnFamily::Metadata,
                b"best_full_block_id",
                final_block_id.0.as_ref().to_vec(),
            );
            batch.put(
                ColumnFamily::Metadata,
                b"best_full_block_height",
                final_height.to_be_bytes().to_vec(),
            );
        }

        // Execute the batch
        self.storage.write_batch(batch)?;

        // Update in-memory state
        if final_cumulative_difficulty > &current_best_difficulty {
            *self.best_header_id.write() = Some(final_block_id.clone());
            *self.best_height.write() = final_height;
            *self.best_cumulative_difficulty.write() = final_cumulative_difficulty.clone();
        }

        if update_full_block {
            *self.best_full_block_id.write() = Some(final_block_id.clone());
            *self.best_full_block_height.write() = final_height;
            info!(final_height, %final_block_id, "New best full block (batched)");
        }

        Ok(selection)
    }

    /// Execute a write batch directly (for combined history+utxo batches).
    pub fn execute_batch(&self, batch: WriteBatch) -> StateResult<()> {
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Update in-memory state after a successful batched write.
    pub fn update_in_memory_state(
        &self,
        block_id: BlockId,
        height: u32,
        cumulative_difficulty: BigUint,
    ) {
        let current_best = self.best_cumulative_difficulty();
        if cumulative_difficulty > current_best {
            *self.best_header_id.write() = Some(block_id.clone());
            *self.best_height.write() = height;
            *self.best_cumulative_difficulty.write() = cumulative_difficulty;
        }

        let current_full = self.best_full_block_height();
        if height > current_full {
            *self.best_full_block_id.write() = Some(block_id);
            *self.best_full_block_height.write() = height;
        }
    }

    /// Get a full block by ID.
    pub fn get_full_block(&self, id: &BlockId) -> StateResult<Option<FullBlock>> {
        let header = match self.headers.get(id)? {
            Some(h) => h,
            None => return Ok(None),
        };

        let transactions = match self.blocks.get_transactions(id)? {
            Some(txs) => txs,
            None => return Ok(None),
        };

        // Extension and AD proofs may or may not exist
        let extension = Extension::empty(id.clone()); // TODO: Load from storage
        let ad_proofs = self.blocks.get_ad_proofs(id)?;

        Ok(Some(FullBlock::new(
            header,
            transactions,
            extension,
            ad_proofs,
        )))
    }

    /// Get headers needed for sync (locator).
    pub fn get_header_locator(&self) -> StateResult<Vec<BlockId>> {
        let mut locator = Vec::new();
        let best = self.best_height();

        if best == 0 {
            return Ok(locator);
        }

        // Use exponentially spaced headers
        let mut step = 1u32;
        let mut height = best;

        loop {
            if let Some(header) = self.headers.get_by_height(height)? {
                locator.push(header.id);
            }

            if height == 1 {
                break;
            }

            height = height.saturating_sub(step);
            if height < 1 {
                height = 1;
            }

            // Increase step exponentially
            if locator.len() > 10 {
                step *= 2;
            }
        }

        Ok(locator)
    }

    /// Find common ancestor with a set of block IDs.
    pub fn find_common_ancestor(&self, ids: &[BlockId]) -> StateResult<Option<(BlockId, u32)>> {
        for id in ids {
            if let Some(header) = self.headers.get(id)? {
                // Check if this header is on our main chain
                if let Some(main_header) = self.headers.get_by_height(header.height)? {
                    if main_header.id == header.id {
                        return Ok(Some((header.id, header.height)));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Get chain height gap (headers ahead of full blocks).
    pub fn chain_gap(&self) -> u32 {
        self.best_height()
            .saturating_sub(self.best_full_block_height())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergo_storage::Database;
    use tempfile::TempDir;

    fn create_test_history() -> (History, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        let history = History::new(Arc::new(db));
        (history, tmp)
    }

    #[test]
    fn test_init_empty() {
        let (history, _tmp) = create_test_history();
        assert_eq!(history.best_height(), 0);
        assert!(history.best_header_id().is_none());
    }
}
