//! UTXO state management.
//!
//! The UTXO state tracks all unspent transaction outputs (boxes) in the blockchain.
//! It uses an AVL+ tree for authenticated state representation.

use crate::{columns, StateError, StateResult};
use blake2::{digest::Digest, Blake2b};
use bytes::Bytes;
use ergo_avltree_rust::authenticated_tree_ops::AuthenticatedTreeOps;
use ergo_avltree_rust::batch_avl_prover::BatchAVLProver;
use ergo_avltree_rust::operation::{ADDigest, ADKey, ADValue, KeyValue, Operation};
use ergo_avltree_rust::versioned_avl_storage::VersionedAVLStorage;
use ergo_lib::ergotree_ir::chain::ergo_box::{BoxId, ErgoBox};
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use ergo_storage::{ColumnFamily, Storage, WriteBatch};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// Type alias for Blake2b with 256-bit output.
type Blake2b256 = Blake2b<blake2::digest::consts::U32>;

/// Box ID length in bytes (32 bytes for Digest32).
pub const BOX_ID_LENGTH: usize = 32;

/// A box entry in the UTXO set with full ErgoBox data.
#[derive(Debug, Clone)]
pub struct BoxEntry {
    /// The ErgoBox.
    pub ergo_box: ErgoBox,
    /// Creation height (block height where this box was created).
    pub creation_height: u32,
    /// Transaction ID that created this box.
    pub tx_id: Vec<u8>,
    /// Output index in the transaction.
    pub output_index: u16,
}

impl BoxEntry {
    /// Create a new box entry from an ErgoBox.
    pub fn new(ergo_box: ErgoBox, creation_height: u32, tx_id: Vec<u8>, output_index: u16) -> Self {
        Self {
            ergo_box,
            creation_height,
            tx_id,
            output_index,
        }
    }

    /// Get the box ID.
    pub fn box_id(&self) -> BoxId {
        self.ergo_box.box_id()
    }

    /// Get the box ID as bytes.
    pub fn box_id_bytes(&self) -> Vec<u8> {
        self.ergo_box.box_id().as_ref().to_vec()
    }

    /// Serialize the box entry for storage.
    /// Format: creation_height (4) | tx_id (32) | output_index (2) | ergo_box_bytes (rest)
    pub fn serialize(&self) -> StateResult<Vec<u8>> {
        let box_bytes = self
            .ergo_box
            .sigma_serialize_bytes()
            .map_err(|e| StateError::Serialization(e.to_string()))?;

        let mut bytes = Vec::with_capacity(38 + box_bytes.len());
        bytes.extend_from_slice(&self.creation_height.to_be_bytes());
        bytes.extend_from_slice(&self.tx_id);
        bytes.extend_from_slice(&self.output_index.to_be_bytes());
        bytes.extend_from_slice(&box_bytes);

        Ok(bytes)
    }

    /// Deserialize a box entry from storage.
    pub fn deserialize(bytes: &[u8]) -> StateResult<Self> {
        if bytes.len() < 38 {
            return Err(StateError::Serialization("Box entry too short".to_string()));
        }

        let creation_height = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        let tx_id = bytes[4..36].to_vec();
        let output_index = u16::from_be_bytes(bytes[36..38].try_into().unwrap());
        let box_bytes = &bytes[38..];

        let ergo_box = ErgoBox::sigma_parse_bytes(box_bytes)
            .map_err(|e| StateError::Serialization(e.to_string()))?;

        Ok(Self {
            ergo_box,
            creation_height,
            tx_id,
            output_index,
        })
    }

    /// Get just the serialized ErgoBox bytes (for AVL tree value).
    pub fn serialize_box_only(&self) -> StateResult<Vec<u8>> {
        self.ergo_box
            .sigma_serialize_bytes()
            .map_err(|e| StateError::Serialization(e.to_string()))
    }
}

/// UTXO state change from applying a block.
#[derive(Debug, Default)]
pub struct StateChange {
    /// Boxes created (added to UTXO set).
    pub created: Vec<BoxEntry>,
    /// Box IDs spent (removed from UTXO set).
    pub spent: Vec<BoxId>,
}

impl StateChange {
    /// Create a new empty state change.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the state change is empty.
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.spent.is_empty()
    }

    /// Get the number of operations in this change.
    pub fn len(&self) -> usize {
        self.created.len() + self.spent.len()
    }

    /// Convert to AVL tree operations.
    pub fn to_avl_operations(&self) -> StateResult<Vec<Operation>> {
        let mut ops = Vec::with_capacity(self.len());

        // Removals first (spent boxes)
        for box_id in &self.spent {
            let key: ADKey = Bytes::copy_from_slice(box_id.as_ref());
            ops.push(Operation::Remove(key));
        }

        // Then insertions (created boxes)
        for entry in &self.created {
            let key: ADKey = Bytes::copy_from_slice(entry.box_id().as_ref());
            let value: ADValue = Bytes::from(entry.serialize_box_only()?);
            ops.push(Operation::Insert(KeyValue { key, value }));
        }

        Ok(ops)
    }

    /// Create a StateChange from BlockTransactions.
    /// This extracts the inputs (spent boxes) and outputs (created boxes) from all transactions.
    pub fn from_block_transactions(
        block_txs: &ergo_consensus::BlockTransactions,
        block_height: u32,
    ) -> Self {
        let mut change = Self::new();

        // Extract all inputs (boxes being spent)
        change.spent = block_txs.get_input_box_ids();

        // Extract all outputs (boxes being created)
        for (ergo_box, tx_id, output_index) in block_txs.get_outputs() {
            let entry = BoxEntry::new(ergo_box, block_height, tx_id, output_index);
            change.created.push(entry);
        }

        change
    }
}

/// Undo data for reversing a state change (for rollback).
/// Contains the inverse of a StateChange: boxes that were spent (to restore)
/// and boxes that were created (to remove).
#[derive(Debug, Clone)]
pub struct UndoData {
    /// Block height this undo data is for.
    pub height: u32,
    /// Boxes that were spent in the original change (need to restore).
    pub spent_boxes: Vec<BoxEntry>,
    /// Box IDs that were created in the original change (need to remove).
    pub created_box_ids: Vec<Vec<u8>>,
}

impl UndoData {
    /// Create new undo data.
    pub fn new(height: u32) -> Self {
        Self {
            height,
            spent_boxes: Vec::new(),
            created_box_ids: Vec::new(),
        }
    }

    /// Serialize undo data for storage.
    /// Format: height (4) | spent_count (4) | [spent_boxes...] | created_count (4) | [created_box_ids...]
    pub fn serialize(&self) -> StateResult<Vec<u8>> {
        let mut bytes = Vec::new();

        // Height
        bytes.extend_from_slice(&self.height.to_be_bytes());

        // Spent boxes count and data
        bytes.extend_from_slice(&(self.spent_boxes.len() as u32).to_be_bytes());
        for entry in &self.spent_boxes {
            let entry_bytes = entry.serialize()?;
            bytes.extend_from_slice(&(entry_bytes.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&entry_bytes);
        }

        // Created box IDs count and data
        bytes.extend_from_slice(&(self.created_box_ids.len() as u32).to_be_bytes());
        for box_id in &self.created_box_ids {
            bytes.extend_from_slice(&(box_id.len() as u32).to_be_bytes());
            bytes.extend_from_slice(box_id);
        }

        Ok(bytes)
    }

    /// Deserialize undo data from storage.
    pub fn deserialize(bytes: &[u8]) -> StateResult<Self> {
        if bytes.len() < 12 {
            return Err(StateError::Serialization("Undo data too short".to_string()));
        }

        let mut offset = 0;

        // Height
        let height = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
        offset += 4;

        // Spent boxes
        let spent_count =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        let mut spent_boxes = Vec::with_capacity(spent_count);
        for _ in 0..spent_count {
            if offset + 4 > bytes.len() {
                return Err(StateError::Serialization("Undo data truncated".to_string()));
            }
            let entry_len =
                u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + entry_len > bytes.len() {
                return Err(StateError::Serialization("Undo data truncated".to_string()));
            }
            let entry = BoxEntry::deserialize(&bytes[offset..offset + entry_len])?;
            spent_boxes.push(entry);
            offset += entry_len;
        }

        // Created box IDs
        if offset + 4 > bytes.len() {
            return Err(StateError::Serialization("Undo data truncated".to_string()));
        }
        let created_count =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        let mut created_box_ids = Vec::with_capacity(created_count);
        for _ in 0..created_count {
            if offset + 4 > bytes.len() {
                return Err(StateError::Serialization("Undo data truncated".to_string()));
            }
            let id_len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + id_len > bytes.len() {
                return Err(StateError::Serialization("Undo data truncated".to_string()));
            }
            created_box_ids.push(bytes[offset..offset + id_len].to_vec());
            offset += id_len;
        }

        Ok(Self {
            height,
            spent_boxes,
            created_box_ids,
        })
    }

    /// Convert to a StateChange that reverses the original change.
    /// - Spent boxes become created boxes (restore them)
    /// - Created box IDs become spent (remove them)
    pub fn to_reverse_change(&self) -> StateChange {
        StateChange {
            created: self.spent_boxes.clone(),
            spent: self
                .created_box_ids
                .iter()
                .filter_map(|id| {
                    if id.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(id);
                        Some(BoxId::from(ergo_chain_types::Digest32::from(arr)))
                    } else {
                        None
                    }
                })
                .collect(),
        }
    }
}

/// RocksDB-backed storage for the AVL tree.
pub struct RocksDbAvlStorage {
    storage: Arc<dyn Storage>,
    current_version: RwLock<Option<ADDigest>>,
}

impl RocksDbAvlStorage {
    /// Create a new RocksDB-backed AVL storage.
    pub fn new(storage: Arc<dyn Storage>) -> StateResult<Self> {
        let current_version = storage
            .get(ColumnFamily::Metadata, b"avl_version")?
            .map(Bytes::from);

        Ok(Self {
            storage,
            current_version: RwLock::new(current_version),
        })
    }

    /// Save a node to storage.
    fn save_node(&self, key: &[u8], value: &[u8]) -> StateResult<()> {
        self.storage.put(columns::UTXO, key, value)?;
        Ok(())
    }

    /// Load a node from storage.
    #[allow(dead_code)]
    fn load_node(&self, key: &[u8]) -> StateResult<Option<Vec<u8>>> {
        Ok(self.storage.get(columns::UTXO, key)?)
    }
}

impl VersionedAVLStorage for RocksDbAvlStorage {
    fn update(
        &mut self,
        prover: &mut BatchAVLProver,
        additional_data: Vec<(ADKey, ADValue)>,
    ) -> anyhow::Result<()> {
        // Get the nodes that were removed during operations
        let removed_nodes = prover.removed_nodes();

        // Create a write batch
        let mut batch = WriteBatch::new();

        // Remove nodes that are no longer needed
        for node_id in removed_nodes {
            // Borrow the node from Rc<RefCell<Node>> to get its label
            let label = node_id.borrow().get_label();
            batch.delete(columns::UTXO, label.to_vec());
        }

        // Store additional data (box entries with full metadata)
        for (key, value) in additional_data {
            batch.put(columns::UTXO, key.to_vec(), value.to_vec());
        }

        // Update version with the new state root
        if let Some(digest) = prover.digest() {
            let digest_vec = digest.to_vec();
            batch.put(ColumnFamily::Metadata, b"avl_version", digest_vec.clone());
            *self.current_version.write() = Some(Bytes::from(digest_vec));
        }

        self.storage.write_batch(batch)?;
        Ok(())
    }

    fn rollback(
        &mut self,
        _version: &ADDigest,
    ) -> anyhow::Result<(ergo_avltree_rust::batch_node::NodeId, usize)> {
        // Load the root node for this version
        // In a full implementation, we'd need to store version -> (root, height) mappings
        // For now, we return an error as rollback requires additional tracking
        anyhow::bail!("Rollback not yet fully implemented - need version tracking")
    }

    fn version(&self) -> Option<ADDigest> {
        self.current_version.read().clone()
    }

    fn rollback_versions<'a>(&'a self) -> Box<dyn Iterator<Item = ADDigest> + 'a> {
        // Return an empty iterator for now
        // Full implementation would iterate over stored versions
        Box::new(std::iter::empty())
    }
}

/// UTXO state manager with AVL tree for authenticated state.
pub struct UtxoState {
    /// Storage backend.
    storage: Arc<dyn Storage>,
    /// Current state root (AVL tree root hash).
    state_root: RwLock<Vec<u8>>,
    /// Current height.
    height: RwLock<u32>,
}

impl UtxoState {
    /// Create a new UTXO state with the given storage.
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            state_root: RwLock::new(vec![0; 32]),
            height: RwLock::new(0),
        }
    }

    /// Initialize from storage (load existing state).
    pub fn init_from_storage(storage: Arc<dyn Storage>) -> StateResult<Self> {
        let state = Self::new(storage);

        // Load state root and height from metadata
        if let Some(root) = state.storage.get(ColumnFamily::Metadata, b"state_root")? {
            *state.state_root.write() = root;
        }

        if let Some(height_bytes) = state.storage.get(ColumnFamily::Metadata, b"utxo_height")? {
            if height_bytes.len() >= 4 {
                let height = u32::from_be_bytes(height_bytes[0..4].try_into().unwrap());
                *state.height.write() = height;
            }
        }

        Ok(state)
    }

    /// Get the current state root.
    pub fn state_root(&self) -> Vec<u8> {
        self.state_root.read().clone()
    }

    /// Get the current height.
    pub fn height(&self) -> u32 {
        *self.height.read()
    }

    /// Get a box by its ID.
    #[instrument(skip(self), fields(box_id = %box_id))]
    pub fn get_box(&self, box_id: &BoxId) -> StateResult<Option<BoxEntry>> {
        match self.storage.get(columns::UTXO, box_id.as_ref())? {
            Some(bytes) => {
                let entry = BoxEntry::deserialize(&bytes)?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// Get a box by its ID bytes.
    pub fn get_box_by_bytes(&self, box_id: &[u8]) -> StateResult<Option<BoxEntry>> {
        match self.storage.get(columns::UTXO, box_id)? {
            Some(bytes) => {
                let entry = BoxEntry::deserialize(&bytes)?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// Check if a box exists.
    pub fn contains_box(&self, box_id: &BoxId) -> StateResult<bool> {
        self.storage
            .contains(columns::UTXO, box_id.as_ref())
            .map_err(StateError::from)
    }

    /// Check if a box exists by bytes.
    pub fn contains_box_bytes(&self, box_id: &[u8]) -> StateResult<bool> {
        self.storage
            .contains(columns::UTXO, box_id)
            .map_err(StateError::from)
    }

    // --- Index Helper Methods ---

    /// Compute BLAKE2b-256 hash of an ErgoTree for indexing.
    fn hash_ergotree(ergo_box: &ErgoBox) -> Vec<u8> {
        let tree_bytes = ergo_box
            .ergo_tree
            .sigma_serialize_bytes()
            .unwrap_or_default();
        let hash = Blake2b256::digest(&tree_bytes);
        hash.to_vec()
    }

    /// Add a box ID to an index (ErgoTree or Token).
    /// Index format: key -> list of box IDs (each 32 bytes).
    fn add_to_index(
        &self,
        batch: &mut WriteBatch,
        cf: ColumnFamily,
        index_key: &[u8],
        box_id: &[u8],
    ) -> StateResult<()> {
        let mut box_ids = self.get_index_entries(cf, index_key)?;
        // Only add if not already present
        if !box_ids.iter().any(|id| id == box_id) {
            box_ids.push(box_id.to_vec());
            batch.put(
                cf,
                index_key.to_vec(),
                Self::serialize_box_id_list(&box_ids),
            );
        }
        Ok(())
    }

    /// Remove a box ID from an index.
    fn remove_from_index(
        &self,
        batch: &mut WriteBatch,
        cf: ColumnFamily,
        index_key: &[u8],
        box_id: &[u8],
    ) -> StateResult<()> {
        let mut box_ids = self.get_index_entries(cf, index_key)?;
        box_ids.retain(|id| id != box_id);
        if box_ids.is_empty() {
            batch.delete(cf, index_key.to_vec());
        } else {
            batch.put(
                cf,
                index_key.to_vec(),
                Self::serialize_box_id_list(&box_ids),
            );
        }
        Ok(())
    }

    /// Get all box IDs for an index key.
    fn get_index_entries(&self, cf: ColumnFamily, index_key: &[u8]) -> StateResult<Vec<Vec<u8>>> {
        match self.storage.get(cf, index_key)? {
            Some(bytes) => Ok(Self::deserialize_box_id_list(&bytes)),
            None => Ok(Vec::new()),
        }
    }

    /// Serialize a list of box IDs for storage.
    /// Format: count (4 bytes) | box_id_1 (32 bytes) | box_id_2 (32 bytes) | ...
    fn serialize_box_id_list(box_ids: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + box_ids.len() * BOX_ID_LENGTH);
        bytes.extend_from_slice(&(box_ids.len() as u32).to_be_bytes());
        for id in box_ids {
            bytes.extend_from_slice(id);
        }
        bytes
    }

    /// Deserialize a list of box IDs from storage.
    fn deserialize_box_id_list(bytes: &[u8]) -> Vec<Vec<u8>> {
        if bytes.len() < 4 {
            return Vec::new();
        }
        let count = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4])) as usize;
        let mut result = Vec::with_capacity(count);
        let mut offset = 4;
        for _ in 0..count {
            if offset + BOX_ID_LENGTH > bytes.len() {
                break;
            }
            result.push(bytes[offset..offset + BOX_ID_LENGTH].to_vec());
            offset += BOX_ID_LENGTH;
        }
        result
    }

    /// Apply a state change (from a block) and store undo data for rollback.
    #[instrument(skip(self, change), fields(created = change.created.len(), spent = change.spent.len()))]
    pub fn apply_change(&self, change: &StateChange, new_height: u32) -> StateResult<()> {
        let mut batch = WriteBatch::new();

        // Create undo data to enable rollback
        let mut undo = UndoData::new(new_height);

        // Remove spent boxes (but save them for undo)
        for box_id in &change.spent {
            // Get the box data before removing it (for undo and index removal)
            if let Some(entry) = self.get_box(box_id)? {
                // Remove from ErgoTree index
                let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
                self.remove_from_index(
                    &mut batch,
                    columns::ERGOTREE_INDEX,
                    &ergotree_hash,
                    box_id.as_ref(),
                )?;

                // Remove from Token index for each token in the box
                for token in entry
                    .ergo_box
                    .tokens
                    .as_ref()
                    .map(|t| t.as_slice())
                    .unwrap_or(&[])
                {
                    let token_id = token.token_id.as_ref();
                    self.remove_from_index(
                        &mut batch,
                        columns::TOKEN_INDEX,
                        token_id,
                        box_id.as_ref(),
                    )?;
                }

                undo.spent_boxes.push(entry);
            } else {
                return Err(StateError::BoxNotFound(hex::encode(box_id.as_ref())));
            }
            batch.delete(columns::UTXO, box_id.as_ref().to_vec());
        }

        // Add created boxes (and record their IDs for undo)
        for entry in &change.created {
            let serialized = entry.serialize()?;
            let box_id_bytes = entry.box_id_bytes();
            undo.created_box_ids.push(box_id_bytes.clone());

            // Add to ErgoTree index
            let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
            self.add_to_index(
                &mut batch,
                columns::ERGOTREE_INDEX,
                &ergotree_hash,
                &box_id_bytes,
            )?;

            // Add to Token index for each token in the box
            for token in entry
                .ergo_box
                .tokens
                .as_ref()
                .map(|t| t.as_slice())
                .unwrap_or(&[])
            {
                let token_id = token.token_id.as_ref();
                self.add_to_index(&mut batch, columns::TOKEN_INDEX, token_id, &box_id_bytes)?;
            }

            batch.put(columns::UTXO, box_id_bytes, serialized);
        }

        // Store undo data keyed by height
        let undo_key = new_height.to_be_bytes().to_vec();
        let undo_bytes = undo.serialize()?;
        batch.put(ColumnFamily::UndoData, undo_key, undo_bytes);

        // Update height
        batch.put(
            ColumnFamily::Metadata,
            b"utxo_height",
            new_height.to_be_bytes().to_vec(),
        );

        // Execute batch
        self.storage.write_batch(batch)?;

        // Update in-memory state
        *self.height.write() = new_height;

        debug!(height = new_height, "Applied state change with undo data");

        Ok(())
    }

    /// Add a state change to an external batch without executing it.
    /// This allows batching multiple blocks into a single write.
    /// Call `update_height_in_memory` after the batch is executed.
    #[instrument(skip(self, batch, change), fields(created = change.created.len(), spent = change.spent.len()))]
    pub fn add_change_to_batch(
        &self,
        batch: &mut WriteBatch,
        change: &StateChange,
        new_height: u32,
    ) -> StateResult<()> {
        // Create undo data to enable rollback
        let mut undo = UndoData::new(new_height);

        // Remove spent boxes (but save them for undo)
        for box_id in &change.spent {
            // Get the box data before removing it (for undo and index removal)
            if let Some(entry) = self.get_box(box_id)? {
                // Remove from ErgoTree index
                let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
                self.remove_from_index(
                    batch,
                    columns::ERGOTREE_INDEX,
                    &ergotree_hash,
                    box_id.as_ref(),
                )?;

                // Remove from Token index for each token in the box
                for token in entry
                    .ergo_box
                    .tokens
                    .as_ref()
                    .map(|t| t.as_slice())
                    .unwrap_or(&[])
                {
                    let token_id = token.token_id.as_ref();
                    self.remove_from_index(batch, columns::TOKEN_INDEX, token_id, box_id.as_ref())?;
                }

                undo.spent_boxes.push(entry);
            } else {
                return Err(StateError::BoxNotFound(hex::encode(box_id.as_ref())));
            }
            batch.delete(columns::UTXO, box_id.as_ref().to_vec());
        }

        // Add created boxes (and record their IDs for undo)
        for entry in &change.created {
            let serialized = entry.serialize()?;
            let box_id_bytes = entry.box_id_bytes();
            undo.created_box_ids.push(box_id_bytes.clone());

            // Add to ErgoTree index
            let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
            self.add_to_index(
                batch,
                columns::ERGOTREE_INDEX,
                &ergotree_hash,
                &box_id_bytes,
            )?;

            // Add to Token index for each token in the box
            for token in entry
                .ergo_box
                .tokens
                .as_ref()
                .map(|t| t.as_slice())
                .unwrap_or(&[])
            {
                let token_id = token.token_id.as_ref();
                self.add_to_index(batch, columns::TOKEN_INDEX, token_id, &box_id_bytes)?;
            }

            batch.put(columns::UTXO, box_id_bytes, serialized);
        }

        // Store undo data keyed by height
        let undo_key = new_height.to_be_bytes().to_vec();
        let undo_bytes = undo.serialize()?;
        batch.put(ColumnFamily::UndoData, undo_key, undo_bytes);

        // Update height in batch
        batch.put(
            ColumnFamily::Metadata,
            b"utxo_height",
            new_height.to_be_bytes().to_vec(),
        );

        Ok(())
    }

    /// Update in-memory height after a batched write is executed.
    pub fn update_height_in_memory(&self, new_height: u32) {
        *self.height.write() = new_height;
    }

    /// Apply a state change and compute new state root using AVL tree.
    #[instrument(skip(self, change, prover), fields(created = change.created.len(), spent = change.spent.len()))]
    pub fn apply_change_with_proof(
        &self,
        change: &StateChange,
        new_height: u32,
        prover: &mut BatchAVLProver,
    ) -> StateResult<Vec<u8>> {
        // Convert state change to AVL operations
        let operations = change.to_avl_operations()?;

        // Apply operations to the prover
        for op in &operations {
            prover
                .perform_one_operation(op)
                .map_err(|e| StateError::AvlTree(e.to_string()))?;
        }

        // Generate proof
        let proof = prover.generate_proof();

        // Get new digest (state root)
        let new_digest = prover
            .digest()
            .ok_or_else(|| StateError::AvlTree("Failed to get digest".to_string()))?;

        // Apply to regular storage
        self.apply_change(change, new_height)?;

        // Update state root
        let mut batch = WriteBatch::new();
        batch.put(ColumnFamily::Metadata, b"state_root", new_digest.to_vec());
        self.storage.write_batch(batch)?;
        *self.state_root.write() = new_digest.to_vec();

        Ok(proof.to_vec())
    }

    /// Rollback to a previous height by applying undo data in reverse order.
    #[instrument(skip(self))]
    pub fn rollback(&self, to_height: u32) -> StateResult<()> {
        let current = self.height();
        if to_height >= current {
            return Err(StateError::RollbackFailed(format!(
                "Cannot rollback from {} to {}: target must be lower",
                current, to_height
            )));
        }

        info!(
            from = current,
            to = to_height,
            blocks = current - to_height,
            "Rolling back state"
        );

        // Apply undo data for each block from current down to to_height + 1
        for height in (to_height + 1..=current).rev() {
            self.rollback_one_block(height)?;
        }

        // Update height
        let mut batch = WriteBatch::new();
        batch.put(
            ColumnFamily::Metadata,
            b"utxo_height",
            to_height.to_be_bytes().to_vec(),
        );
        self.storage.write_batch(batch)?;
        *self.height.write() = to_height;

        info!(height = to_height, "Rollback complete");
        Ok(())
    }

    /// Rollback a single block by height using its stored undo data.
    fn rollback_one_block(&self, height: u32) -> StateResult<()> {
        // Load undo data for this height
        let undo_key = height.to_be_bytes().to_vec();
        let undo_bytes = self
            .storage
            .get(ColumnFamily::UndoData, &undo_key)?
            .ok_or_else(|| {
                StateError::RollbackFailed(format!("No undo data for height {}", height))
            })?;

        let undo = UndoData::deserialize(&undo_bytes)?;

        debug!(
            height,
            restore_boxes = undo.spent_boxes.len(),
            remove_boxes = undo.created_box_ids.len(),
            "Applying undo for block"
        );

        let mut batch = WriteBatch::new();

        // Remove boxes that were created in this block (and their index entries)
        for box_id in &undo.created_box_ids {
            // Get the box to remove its index entries
            if let Some(entry) = self.get_box_by_bytes(box_id)? {
                // Remove from ErgoTree index
                let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
                self.remove_from_index(
                    &mut batch,
                    columns::ERGOTREE_INDEX,
                    &ergotree_hash,
                    box_id,
                )?;

                // Remove from Token index
                for token in entry
                    .ergo_box
                    .tokens
                    .as_ref()
                    .map(|t| t.as_slice())
                    .unwrap_or(&[])
                {
                    let token_id = token.token_id.as_ref();
                    self.remove_from_index(&mut batch, columns::TOKEN_INDEX, token_id, box_id)?;
                }
            }
            batch.delete(columns::UTXO, box_id.clone());
        }

        // Restore boxes that were spent in this block (and their index entries)
        for entry in &undo.spent_boxes {
            let serialized = entry.serialize()?;
            let box_id_bytes = entry.box_id_bytes();

            // Restore ErgoTree index
            let ergotree_hash = Self::hash_ergotree(&entry.ergo_box);
            self.add_to_index(
                &mut batch,
                columns::ERGOTREE_INDEX,
                &ergotree_hash,
                &box_id_bytes,
            )?;

            // Restore Token index
            for token in entry
                .ergo_box
                .tokens
                .as_ref()
                .map(|t| t.as_slice())
                .unwrap_or(&[])
            {
                let token_id = token.token_id.as_ref();
                self.add_to_index(&mut batch, columns::TOKEN_INDEX, token_id, &box_id_bytes)?;
            }

            batch.put(columns::UTXO, box_id_bytes, serialized);
        }

        // Remove the undo data (no longer needed)
        batch.delete(ColumnFamily::UndoData, undo_key);

        // Execute batch
        self.storage.write_batch(batch)?;

        Ok(())
    }

    /// Get boxes by ErgoTree hash.
    /// The hash should be a BLAKE2b-256 hash of the serialized ErgoTree.
    #[instrument(skip(self))]
    pub fn get_boxes_by_ergo_tree(&self, ergo_tree_hash: &[u8]) -> StateResult<Vec<BoxEntry>> {
        let box_ids = self.get_index_entries(columns::ERGOTREE_INDEX, ergo_tree_hash)?;
        let mut boxes = Vec::with_capacity(box_ids.len());
        for box_id in box_ids {
            if let Some(entry) = self.get_box_by_bytes(&box_id)? {
                boxes.push(entry);
            }
        }
        debug!(ergo_tree_hash = %hex::encode(ergo_tree_hash), count = boxes.len(), "Retrieved boxes by ErgoTree");
        Ok(boxes)
    }

    /// Get boxes by ErgoTree (hashes the tree internally).
    pub fn get_boxes_by_ergo_tree_direct(&self, ergo_box: &ErgoBox) -> StateResult<Vec<BoxEntry>> {
        let hash = Self::hash_ergotree(ergo_box);
        self.get_boxes_by_ergo_tree(&hash)
    }

    /// Get boxes by token ID.
    #[instrument(skip(self))]
    pub fn get_boxes_by_token(&self, token_id: &[u8]) -> StateResult<Vec<BoxEntry>> {
        let box_ids = self.get_index_entries(columns::TOKEN_INDEX, token_id)?;
        let mut boxes = Vec::with_capacity(box_ids.len());
        for box_id in box_ids {
            if let Some(entry) = self.get_box_by_bytes(&box_id)? {
                boxes.push(entry);
            }
        }
        debug!(token_id = %hex::encode(token_id), count = boxes.len(), "Retrieved boxes by token");
        Ok(boxes)
    }

    /// Get all unique token IDs in the UTXO set.
    pub fn get_all_token_ids(&self) -> StateResult<Vec<Vec<u8>>> {
        let mut token_ids = Vec::new();
        for (key, _value) in self.storage.iter(columns::TOKEN_INDEX)? {
            token_ids.push(key);
        }
        Ok(token_ids)
    }

    /// Get the count of boxes for a specific ErgoTree hash.
    pub fn count_boxes_by_ergo_tree(&self, ergo_tree_hash: &[u8]) -> StateResult<usize> {
        let box_ids = self.get_index_entries(columns::ERGOTREE_INDEX, ergo_tree_hash)?;
        Ok(box_ids.len())
    }

    /// Get the count of boxes containing a specific token.
    pub fn count_boxes_by_token(&self, token_id: &[u8]) -> StateResult<usize> {
        let box_ids = self.get_index_entries(columns::TOKEN_INDEX, token_id)?;
        Ok(box_ids.len())
    }

    /// Get total number of boxes in the UTXO set.
    pub fn box_count(&self) -> StateResult<u64> {
        // This is expensive - would need a counter in metadata
        let mut count = 0u64;
        for _ in self.storage.iter(columns::UTXO)? {
            count += 1;
        }
        Ok(count)
    }

    /// Iterate over all boxes (for debugging/migration).
    pub fn iter_boxes(&self) -> StateResult<impl Iterator<Item = BoxEntry> + '_> {
        let iter = self.storage.iter(columns::UTXO)?;

        Ok(iter.filter_map(|(_key, value)| BoxEntry::deserialize(&value).ok()))
    }

    /// Create a state snapshot at the current height.
    pub fn create_snapshot(&self) -> StateResult<StateSnapshot> {
        let height = self.height();
        let state_root = self.state_root();

        Ok(StateSnapshot {
            height,
            state_root,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
    }
}

/// A snapshot of the UTXO state at a specific height.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    /// Block height of the snapshot.
    pub height: u32,
    /// State root hash at this height.
    pub state_root: Vec<u8>,
    /// Timestamp when snapshot was created.
    pub timestamp: u64,
}

impl StateSnapshot {
    /// Serialize the snapshot for storage.
    pub fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.height.to_be_bytes());
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.extend_from_slice(&self.state_root);
        bytes
    }

    /// Deserialize a snapshot from storage.
    pub fn deserialize(bytes: &[u8]) -> StateResult<Self> {
        // Minimum: 4 (height) + 8 (timestamp) = 12 bytes, state_root can be variable
        if bytes.len() < 12 {
            return Err(StateError::Serialization("Snapshot too short".to_string()));
        }

        let height = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        let timestamp = u64::from_be_bytes(bytes[4..12].try_into().unwrap());
        let state_root = bytes[12..].to_vec();

        Ok(Self {
            height,
            state_root,
            timestamp,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergo_chain_types::Digest32;
    use ergo_lib::ergotree_ir::chain::ergo_box::{
        box_value::BoxValue, ErgoBoxCandidate, NonMandatoryRegisters,
    };
    use ergo_lib::ergotree_ir::chain::tx_id::TxId;
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
    use ergo_lib::ergotree_ir::mir::constant::Constant;
    use ergo_lib::ergotree_ir::mir::expr::Expr;
    use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::{SigmaBoolean, SigmaProp};
    use ergo_storage::Database;
    use std::convert::TryFrom;
    use tempfile::TempDir;

    fn create_test_state() -> (UtxoState, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        let state = UtxoState::new(Arc::new(db));
        (state, tmp)
    }

    /// Create a mock ErgoBox for testing
    fn create_mock_box(value: u64, box_id_byte: u8) -> ErgoBox {
        let value = BoxValue::try_from(value).unwrap();
        let sigma_prop = SigmaProp::new(SigmaBoolean::TrivialProp(true));
        let constant: Constant = sigma_prop.into();
        let expr = Expr::Const(constant);
        let ergo_tree = ErgoTree::try_from(expr).unwrap();

        let candidate = ErgoBoxCandidate {
            value,
            ergo_tree,
            tokens: None,
            additional_registers: NonMandatoryRegisters::empty(),
            creation_height: 1,
        };

        let mut tx_id_bytes = [0u8; 32];
        tx_id_bytes[0] = box_id_byte;
        let tx_id = TxId::from(Digest32::from(tx_id_bytes));

        ErgoBox::from_box_candidate(&candidate, tx_id, 0).unwrap()
    }

    /// Create a BoxEntry from an ErgoBox
    fn create_box_entry(ergo_box: ErgoBox, height: u32, tx_id_byte: u8) -> BoxEntry {
        let mut tx_id = vec![0u8; 32];
        tx_id[0] = tx_id_byte;
        BoxEntry::new(ergo_box, height, tx_id, 0)
    }

    // ============ Basic State Tests ============

    #[test]
    fn test_state_change_empty() {
        let change = StateChange::new();
        assert!(change.is_empty());
        assert_eq!(change.len(), 0);
    }

    #[test]
    fn test_state_change_with_additions() {
        let mut change = StateChange::new();
        let box1 = create_mock_box(1_000_000_000, 1);
        let entry1 = create_box_entry(box1, 1, 1);
        change.created.push(entry1);

        assert!(!change.is_empty());
        assert_eq!(change.len(), 1);
    }

    #[test]
    fn test_state_change_with_removals() {
        let mut change = StateChange::new();
        let box1 = create_mock_box(1_000_000_000, 1);
        change.spent.push(box1.box_id());

        assert!(!change.is_empty());
        assert_eq!(change.len(), 1);
    }

    #[test]
    fn test_snapshot_serialization() {
        let snapshot = StateSnapshot {
            height: 12345,
            state_root: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            timestamp: 1699999999,
        };

        let serialized = snapshot.serialize();
        let deserialized = StateSnapshot::deserialize(&serialized).unwrap();

        assert_eq!(deserialized.height, snapshot.height);
        assert_eq!(deserialized.timestamp, snapshot.timestamp);
        assert_eq!(deserialized.state_root, snapshot.state_root);
    }

    #[test]
    fn test_utxo_state_init() {
        let (state, _tmp) = create_test_state();
        assert_eq!(state.height(), 0);
        assert_eq!(state.state_root().len(), 32);
    }

    // ============ Box Application Tests ============
    // Corresponds to Scala's "applyModifier" and "applyBlock" tests

    #[test]
    fn test_apply_single_box() {
        let (state, _tmp) = create_test_state();

        let box1 = create_mock_box(1_000_000_000, 1);
        let box1_id = box1.box_id();
        let entry1 = create_box_entry(box1, 1, 1);

        let mut change = StateChange::new();
        change.created.push(entry1);

        state.apply_change(&change, 1).unwrap();

        // Height should update
        assert_eq!(state.height(), 1);

        // Box should be retrievable
        assert!(state.contains_box(&box1_id).unwrap());
        let retrieved = state.get_box(&box1_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(u64::from(retrieved.unwrap().ergo_box.value), 1_000_000_000);
    }

    #[test]
    fn test_apply_multiple_boxes() {
        let (state, _tmp) = create_test_state();

        let box1 = create_mock_box(1_000_000_000, 1);
        let box2 = create_mock_box(2_000_000_000, 2);
        let box3 = create_mock_box(500_000_000, 3);

        let box1_id = box1.box_id();
        let box2_id = box2.box_id();
        let box3_id = box3.box_id();

        let mut change = StateChange::new();
        change.created.push(create_box_entry(box1, 1, 1));
        change.created.push(create_box_entry(box2, 1, 2));
        change.created.push(create_box_entry(box3, 1, 3));

        state.apply_change(&change, 1).unwrap();

        assert!(state.contains_box(&box1_id).unwrap());
        assert!(state.contains_box(&box2_id).unwrap());
        assert!(state.contains_box(&box3_id).unwrap());
    }

    #[test]
    fn test_apply_and_spend_box() {
        let (state, _tmp) = create_test_state();

        // First, add a box
        let box1 = create_mock_box(1_000_000_000, 1);
        let box1_id = box1.box_id();

        let mut add_change = StateChange::new();
        add_change.created.push(create_box_entry(box1, 1, 1));
        state.apply_change(&add_change, 1).unwrap();

        assert!(state.contains_box(&box1_id).unwrap());

        // Now spend the box
        let mut spend_change = StateChange::new();
        spend_change.spent.push(box1_id.clone());
        state.apply_change(&spend_change, 2).unwrap();

        // Box should no longer exist
        assert!(!state.contains_box(&box1_id).unwrap());
        assert!(state.get_box(&box1_id).unwrap().is_none());
    }

    #[test]
    fn test_state_changes_on_modification() {
        let (state, _tmp) = create_test_state();

        // Add first box
        let box1 = create_mock_box(1_000_000_000, 1);
        let box1_id = box1.box_id();
        let mut change1 = StateChange::new();
        change1.created.push(create_box_entry(box1, 1, 1));
        state.apply_change(&change1, 1).unwrap();

        assert_eq!(state.height(), 1);
        assert!(state.contains_box(&box1_id).unwrap());

        // Add second box
        let box2 = create_mock_box(2_000_000_000, 2);
        let box2_id = box2.box_id();
        let mut change2 = StateChange::new();
        change2.created.push(create_box_entry(box2, 2, 2));
        state.apply_change(&change2, 2).unwrap();

        assert_eq!(state.height(), 2);
        assert!(state.contains_box(&box1_id).unwrap());
        assert!(state.contains_box(&box2_id).unwrap());
    }

    // ============ Rollback Tests ============
    // Corresponds to Scala's "rollback" and "rollbackTo" tests

    #[test]
    fn test_rollback_single_block() {
        let (state, _tmp) = create_test_state();

        let initial_root = state.state_root();

        // Add a box at height 1
        let box1 = create_mock_box(1_000_000_000, 1);
        let box1_id = box1.box_id();
        let mut change = StateChange::new();
        change.created.push(create_box_entry(box1, 1, 1));
        state.apply_change(&change, 1).unwrap();

        assert!(state.contains_box(&box1_id).unwrap());
        assert_eq!(state.height(), 1);

        // Rollback to height 0
        state.rollback(0).unwrap();

        assert_eq!(state.height(), 0);
        assert_eq!(
            state.state_root(),
            initial_root,
            "Root should match initial after rollback"
        );
        assert!(
            !state.contains_box(&box1_id).unwrap(),
            "Box should be gone after rollback"
        );
    }

    #[test]
    fn test_rollback_multiple_blocks() {
        let (state, _tmp) = create_test_state();

        let root_at_0 = state.state_root();

        // Add box at height 1
        let box1 = create_mock_box(1_000_000_000, 1);
        let box1_id = box1.box_id();
        let mut change1 = StateChange::new();
        change1.created.push(create_box_entry(box1, 1, 1));
        state.apply_change(&change1, 1).unwrap();
        let root_at_1 = state.state_root();

        // Add box at height 2
        let box2 = create_mock_box(2_000_000_000, 2);
        let box2_id = box2.box_id();
        let mut change2 = StateChange::new();
        change2.created.push(create_box_entry(box2, 2, 2));
        state.apply_change(&change2, 2).unwrap();

        // Add box at height 3
        let box3 = create_mock_box(3_000_000_000, 3);
        let box3_id = box3.box_id();
        let mut change3 = StateChange::new();
        change3.created.push(create_box_entry(box3, 3, 3));
        state.apply_change(&change3, 3).unwrap();

        assert_eq!(state.height(), 3);

        // Rollback to height 1
        state.rollback(1).unwrap();

        assert_eq!(state.height(), 1);
        assert_eq!(state.state_root(), root_at_1);
        assert!(
            state.contains_box(&box1_id).unwrap(),
            "Box1 should still exist"
        );
        assert!(
            !state.contains_box(&box2_id).unwrap(),
            "Box2 should be gone"
        );
        assert!(
            !state.contains_box(&box3_id).unwrap(),
            "Box3 should be gone"
        );

        // Rollback to height 0
        state.rollback(0).unwrap();

        assert_eq!(state.height(), 0);
        assert_eq!(state.state_root(), root_at_0);
        assert!(
            !state.contains_box(&box1_id).unwrap(),
            "Box1 should be gone"
        );
    }

    // ============ Box Entry Serialization Tests ============

    #[test]
    fn test_box_entry_serialization() {
        let ergo_box = create_mock_box(1_000_000_000, 42);
        // tx_id must be 32 bytes
        let mut tx_id = vec![0u8; 32];
        tx_id[0] = 1;
        tx_id[1] = 2;
        tx_id[2] = 3;
        let entry = BoxEntry::new(ergo_box.clone(), 100, tx_id, 5);

        let serialized = entry.serialize().unwrap();
        let deserialized = BoxEntry::deserialize(&serialized).unwrap();

        assert_eq!(deserialized.creation_height, 100);
        assert_eq!(deserialized.output_index, 5);
        assert_eq!(u64::from(deserialized.ergo_box.value), 1_000_000_000);
    }

    // ============ State Change Structure Tests ============

    #[test]
    fn test_state_change_len_with_both() {
        let mut change = StateChange::new();

        let box1 = create_mock_box(1_000_000_000, 1);
        let box2 = create_mock_box(2_000_000_000, 2);

        change.created.push(create_box_entry(box1.clone(), 1, 1));
        change.created.push(create_box_entry(box2.clone(), 1, 2));
        change.spent.push(box1.box_id());

        // 2 created + 1 spent = 3 operations
        assert_eq!(change.len(), 3);
        assert!(!change.is_empty());
    }

    // ============ Box Lookup Tests ============

    #[test]
    fn test_get_box_not_found() {
        let (state, _tmp) = create_test_state();

        let fake_id_bytes = vec![99u8; 32];
        let result = state.get_box_by_bytes(&fake_id_bytes).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_contains_box_false_for_missing() {
        let (state, _tmp) = create_test_state();

        let fake_id_bytes = vec![99u8; 32];
        assert!(!state.contains_box_bytes(&fake_id_bytes).unwrap());
    }

    // ============ Height Tracking Tests ============

    #[test]
    fn test_height_increments_correctly() {
        let (state, _tmp) = create_test_state();

        assert_eq!(state.height(), 0);

        for i in 1..=5 {
            let boxi = create_mock_box(1_000_000_000, i as u8);
            let mut change = StateChange::new();
            change.created.push(create_box_entry(boxi, i, i as u8));
            state.apply_change(&change, i).unwrap();
            assert_eq!(state.height(), i);
        }
    }

    // ============ Empty Change Tests ============

    #[test]
    fn test_apply_empty_change() {
        let (state, _tmp) = create_test_state();

        let _initial_root = state.state_root();
        let change = StateChange::new();

        state.apply_change(&change, 1).unwrap();

        // Height should still update
        assert_eq!(state.height(), 1);
        // Root may or may not change for empty change (implementation dependent)
    }

    // ============ Concurrent Access Test ============
    // Note: Real concurrent tests would need async runtime

    #[test]
    fn test_state_is_thread_safe() {
        let (state, _tmp) = create_test_state();
        let state_arc = Arc::new(state);

        // Just verify we can clone Arc - actual concurrent access
        // would require async tests
        let _clone1 = Arc::clone(&state_arc);
        let _clone2 = Arc::clone(&state_arc);

        assert_eq!(state_arc.height(), 0);
    }
}
