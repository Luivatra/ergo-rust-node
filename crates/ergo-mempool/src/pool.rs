//! Transaction pool implementation with dependency tracking.
//!
//! This module implements an ordered transaction pool similar to the Scala node's
//! `OrderedTxPool`. Key features:
//!
//! - Transactions are ordered by weight (not just fee)
//! - When a transaction spends outputs of another mempool transaction, the parent's
//!   weight is increased by the child's weight
//! - This ensures parents are always processed before children
//! - Double-spend detection prevents conflicting transactions

use crate::ordering::WeightedTxId;
use crate::{MempoolError, MempoolResult};
use crate::{DEFAULT_MAX_SIZE, DEFAULT_MAX_TXS, DEFAULT_TX_EXPIRY_SECS};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::{BTreeSet, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, instrument, warn};

/// Mempool configuration.
#[derive(Debug, Clone)]
pub struct MempoolConfig {
    /// Maximum total size in bytes.
    pub max_size: usize,
    /// Maximum number of transactions.
    pub max_transactions: usize,
    /// Transaction expiry time in seconds.
    pub tx_expiry_secs: u64,
    /// Minimum fee per byte.
    pub min_fee_per_byte: f64,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_SIZE,
            max_transactions: DEFAULT_MAX_TXS,
            tx_expiry_secs: DEFAULT_TX_EXPIRY_SECS,
            min_fee_per_byte: 0.001, // 0.001 nanoERG per byte
        }
    }
}

/// Stored transaction in the mempool.
#[derive(Debug, Clone)]
pub struct PooledTransaction {
    /// Transaction ID.
    pub id: Vec<u8>,
    /// Serialized transaction bytes.
    pub bytes: Vec<u8>,
    /// Transaction fee.
    pub fee: u64,
    /// Input box IDs (boxes being spent).
    pub inputs: Vec<Vec<u8>>,
    /// Output box IDs (boxes being created).
    pub outputs: Vec<Vec<u8>>,
    /// Arrival timestamp.
    pub arrival_time: u64,
}

/// Mempool statistics.
#[derive(Debug, Clone, Default)]
pub struct MempoolStats {
    /// Number of transactions.
    pub tx_count: usize,
    /// Total size in bytes.
    pub total_size: usize,
    /// Minimum fee per byte.
    pub min_fee_per_byte: f64,
    /// Maximum fee per byte.
    pub max_fee_per_byte: f64,
}

/// Maximum depth for parent transaction weight updates.
/// Prevents DoS from deeply nested transaction chains.
const MAX_PARENT_SCAN_DEPTH: usize = 500;

/// Maximum time (in ms) for parent transaction weight updates.
const MAX_PARENT_SCAN_TIME_MS: u128 = 500;

/// Transaction mempool with dependency tracking.
///
/// Implements weighted ordering similar to Scala's `OrderedTxPool`:
/// - Transactions are ordered by weight (highest first)
/// - Parent transactions (whose outputs are spent by mempool txs) have their
///   weight increased by child transaction weights
/// - This ensures proper ordering for transaction chains
pub struct Mempool {
    /// Configuration.
    config: MempoolConfig,

    /// Transactions by ID.
    transactions: DashMap<Vec<u8>, PooledTransaction>,

    /// Transaction registry: tx_id -> WeightedTxId.
    /// Used for quick lookup of weight info.
    registry: DashMap<Vec<u8>, WeightedTxId>,

    /// Weight-ordered transaction set.
    /// Higher weight = higher priority (comes first in iteration).
    weight_order: RwLock<BTreeSet<WeightedTxId>>,

    /// Output box to transaction mapping.
    /// Maps output box ID -> tx_id of the transaction that creates it.
    /// Used to find parent transactions when a child arrives.
    output_to_tx: DashMap<Vec<u8>, Vec<u8>>,

    /// Input box to transaction mapping (for double-spend detection).
    /// Maps input box ID -> tx_id of the transaction that spends it.
    input_to_tx: DashMap<Vec<u8>, Vec<u8>>,

    /// Current total size.
    total_size: RwLock<usize>,
}

impl Mempool {
    /// Create a new mempool with the given configuration.
    pub fn new(config: MempoolConfig) -> Self {
        Self {
            config,
            transactions: DashMap::new(),
            registry: DashMap::new(),
            weight_order: RwLock::new(BTreeSet::new()),
            output_to_tx: DashMap::new(),
            input_to_tx: DashMap::new(),
            total_size: RwLock::new(0),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(MempoolConfig::default())
    }

    /// Add a transaction to the mempool.
    ///
    /// This will:
    /// 1. Check for double spends
    /// 2. Add the transaction with initial weight = fee_per_factor
    /// 3. Update parent transaction weights (if any parents are in mempool)
    /// 4. Evict lowest weight transaction if pool is full
    #[instrument(skip(self, tx), fields(tx_id = hex::encode(&tx.id)))]
    pub fn add(&self, tx: PooledTransaction) -> MempoolResult<()> {
        // Check if already exists
        if self.transactions.contains_key(&tx.id) {
            return Err(MempoolError::AlreadyExists(hex::encode(&tx.id)));
        }

        // Check size
        let tx_size = tx.bytes.len();
        if tx_size > self.config.max_size / 10 {
            return Err(MempoolError::TooLarge {
                size: tx_size,
                max: self.config.max_size / 10,
            });
        }

        // Check fee
        let fee_per_byte = tx.fee as f64 / tx_size as f64;
        if fee_per_byte < self.config.min_fee_per_byte {
            return Err(MempoolError::FeeTooLow {
                fee: tx.fee,
                min: (self.config.min_fee_per_byte * tx_size as f64) as u64,
            });
        }

        // Check for double spends
        for input in &tx.inputs {
            if self.input_to_tx.contains_key(input) {
                return Err(MempoolError::DoubleSpend(hex::encode(input)));
            }
        }

        // Create weighted tx id
        let wtx = WeightedTxId::new(tx.id.clone(), tx.fee, tx_size, tx.arrival_time);

        // Add to registry
        self.registry.insert(tx.id.clone(), wtx.clone());

        // Add input mappings
        for input in &tx.inputs {
            self.input_to_tx.insert(input.clone(), tx.id.clone());
        }

        // Add output mappings
        for output in &tx.outputs {
            self.output_to_tx.insert(output.clone(), tx.id.clone());
        }

        // Add to weight order
        self.weight_order.write().insert(wtx.clone());

        // Update size
        *self.total_size.write() += tx_size;

        // Store transaction
        self.transactions.insert(tx.id.clone(), tx.clone());

        // Update family weights (parents get weight from this child)
        self.update_family(&tx, wtx.weight);

        // Check capacity and evict if needed
        self.maybe_evict();

        debug!(
            count = self.transactions.len(),
            "Transaction added to mempool"
        );
        Ok(())
    }

    /// Update weights of parent transactions.
    ///
    /// When a new transaction arrives that spends outputs of existing mempool
    /// transactions, those parent transactions should have their weight increased.
    /// This ensures parents are processed before children.
    ///
    /// Matches Scala's `updateFamily` method.
    fn update_family(&self, tx: &PooledTransaction, weight_delta: i64) {
        let start_time = Instant::now();
        self.update_family_recursive(tx, weight_delta, start_time, 0);
    }

    fn update_family_recursive(
        &self,
        tx: &PooledTransaction,
        weight_delta: i64,
        start_time: Instant,
        depth: usize,
    ) {
        // Check limits
        if depth > MAX_PARENT_SCAN_DEPTH {
            warn!(
                tx_id = hex::encode(&tx.id),
                depth, "updateFamily exceeded max depth"
            );
            return;
        }

        let elapsed = start_time.elapsed().as_millis();
        if elapsed > MAX_PARENT_SCAN_TIME_MS {
            warn!(
                tx_id = hex::encode(&tx.id),
                elapsed_ms = elapsed,
                "updateFamily exceeded max time"
            );
            return;
        }

        // Find parent transactions (transactions whose outputs are spent by this tx)
        let mut parent_tx_ids: HashSet<Vec<u8>> = HashSet::new();
        for input in &tx.inputs {
            if let Some(parent_id) = self.output_to_tx.get(input) {
                parent_tx_ids.insert(parent_id.clone());
            }
        }

        // Update each parent's weight
        for parent_id in parent_tx_ids {
            // Skip if parent is this same transaction
            if parent_id == tx.id {
                continue;
            }

            // Get current weight info
            let old_wtx = match self.registry.get(&parent_id) {
                Some(wtx) => wtx.clone(),
                None => continue,
            };

            // Get parent transaction for recursive call
            let parent_tx = match self.transactions.get(&parent_id) {
                Some(tx) => tx.clone(),
                None => continue,
            };

            // Calculate new weight
            let new_weight = old_wtx.weight + weight_delta;
            let new_wtx = WeightedTxId::with_weight(
                old_wtx.tx_id.clone(),
                new_weight,
                old_wtx.fee_per_factor,
                old_wtx.size,
                old_wtx.created,
            );

            // Update registry
            self.registry.insert(parent_id.clone(), new_wtx.clone());

            // Update weight order (remove old, insert new)
            {
                let mut order = self.weight_order.write();
                order.remove(&old_wtx);
                order.insert(new_wtx);
            }

            // Recursively update grandparents
            self.update_family_recursive(&parent_tx, weight_delta, start_time, depth + 1);
        }
    }

    /// Evict lowest weight transactions if pool is over capacity.
    fn maybe_evict(&self) {
        // Check transaction count
        while self.transactions.len() > self.config.max_transactions {
            if self.evict_lowest_weight().is_err() {
                break;
            }
        }

        // Check size
        while *self.total_size.read() > self.config.max_size {
            if self.evict_lowest_weight().is_err() {
                break;
            }
        }
    }

    /// Evict the transaction with the lowest weight.
    fn evict_lowest_weight(&self) -> MempoolResult<()> {
        let lowest_id = {
            let order = self.weight_order.read();
            order.iter().last().map(|wtx| wtx.tx_id.clone())
        };

        if let Some(tx_id) = lowest_id {
            self.remove(&tx_id)?;
            warn!("Evicted lowest weight transaction");
        }

        Ok(())
    }

    /// Remove a transaction by ID.
    #[instrument(skip(self), fields(tx_id = hex::encode(tx_id)))]
    pub fn remove(&self, tx_id: &[u8]) -> MempoolResult<PooledTransaction> {
        // Get transaction
        let (_, tx) = self
            .transactions
            .remove(tx_id)
            .ok_or_else(|| MempoolError::NotFound(hex::encode(tx_id)))?;

        // Get weight info
        let wtx = self.registry.remove(tx_id).map(|(_, w)| w);

        // Remove from weight order
        if let Some(ref wtx) = wtx {
            self.weight_order.write().remove(wtx);
        }

        // Remove input mappings
        for input in &tx.inputs {
            self.input_to_tx.remove(input);
        }

        // Remove output mappings
        for output in &tx.outputs {
            self.output_to_tx.remove(output);
        }

        // Update size
        *self.total_size.write() -= tx.bytes.len();

        // Update family weights (subtract this tx's weight from parents)
        if let Some(wtx) = wtx {
            self.update_family(&tx, -wtx.weight);
        }

        debug!(
            count = self.transactions.len(),
            "Transaction removed from mempool"
        );
        Ok(tx)
    }

    /// Get a transaction by ID.
    pub fn get(&self, tx_id: &[u8]) -> Option<PooledTransaction> {
        self.transactions.get(tx_id).map(|r| r.clone())
    }

    /// Check if a transaction exists.
    pub fn contains(&self, tx_id: &[u8]) -> bool {
        self.transactions.contains_key(tx_id)
    }

    /// Check if an input is already spent by a mempool transaction.
    pub fn is_input_spent(&self, input_id: &[u8]) -> bool {
        self.input_to_tx.contains_key(input_id)
    }

    /// Get the transaction that spends a given input.
    pub fn get_spending_tx(&self, input_id: &[u8]) -> Option<Vec<u8>> {
        self.input_to_tx.get(input_id).map(|r| r.clone())
    }

    /// Get the transaction that creates a given output.
    pub fn get_creating_tx(&self, output_id: &[u8]) -> Option<Vec<u8>> {
        self.output_to_tx.get(output_id).map(|r| r.clone())
    }

    /// Get transactions ordered by weight (highest first).
    ///
    /// This respects transaction dependencies: parent transactions will have
    /// higher weight than their children (if children are also in mempool).
    pub fn get_by_weight(&self, limit: usize) -> Vec<PooledTransaction> {
        let order = self.weight_order.read();
        order
            .iter()
            .take(limit)
            .filter_map(|wtx| self.get(&wtx.tx_id))
            .collect()
    }

    /// Get transactions ordered by fee (legacy method, uses weight ordering).
    pub fn get_by_fee(&self, limit: usize) -> Vec<PooledTransaction> {
        self.get_by_weight(limit)
    }

    /// Get all transaction IDs.
    pub fn get_all_ids(&self) -> Vec<Vec<u8>> {
        self.transactions.iter().map(|r| r.key().clone()).collect()
    }

    /// Get mempool statistics.
    pub fn stats(&self) -> MempoolStats {
        let order = self.weight_order.read();

        let (min_fpb, max_fpb) = if order.is_empty() {
            (0.0, 0.0)
        } else {
            let min = order.iter().last().map(|o| o.fee_per_byte()).unwrap_or(0.0);
            let max = order.iter().next().map(|o| o.fee_per_byte()).unwrap_or(0.0);
            (min, max)
        };

        MempoolStats {
            tx_count: self.transactions.len(),
            total_size: *self.total_size.read(),
            min_fee_per_byte: min_fpb,
            max_fee_per_byte: max_fpb,
        }
    }

    /// Clear all transactions.
    pub fn clear(&self) {
        self.transactions.clear();
        self.registry.clear();
        self.input_to_tx.clear();
        self.output_to_tx.clear();
        self.weight_order.write().clear();
        *self.total_size.write() = 0;
        info!("Mempool cleared");
    }

    /// Remove transactions that conflict with a new block.
    pub fn remove_confirmed(&self, tx_ids: &[Vec<u8>], spent_inputs: &[Vec<u8>]) {
        // Remove confirmed transactions
        for tx_id in tx_ids {
            let _ = self.remove(tx_id);
        }

        // Remove transactions that spend now-confirmed inputs
        let mut to_remove = Vec::new();
        for input in spent_inputs {
            if let Some(tx_id) = self.get_spending_tx(input) {
                to_remove.push(tx_id);
            }
        }

        for tx_id in to_remove {
            let _ = self.remove(&tx_id);
        }
    }

    /// Remove expired transactions.
    pub fn remove_expired(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let expiry_ms = self.config.tx_expiry_secs * 1000;

        let expired: Vec<_> = self
            .transactions
            .iter()
            .filter(|r| now - r.arrival_time > expiry_ms)
            .map(|r| r.key().clone())
            .collect();

        for tx_id in expired {
            let _ = self.remove(&tx_id);
        }
    }

    /// Get the weight of a transaction.
    pub fn get_weight(&self, tx_id: &[u8]) -> Option<i64> {
        self.registry.get(tx_id).map(|r| r.weight)
    }

    /// Get the number of transactions in the pool.
    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    /// Check if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_tx(id: u8, fee: u64, size: usize) -> PooledTransaction {
        PooledTransaction {
            id: vec![id],
            bytes: vec![0; size],
            fee,
            inputs: vec![vec![id, 1]],
            outputs: vec![vec![id, 2]], // Each tx creates an output
            arrival_time: 1000,
        }
    }

    fn create_test_tx_with_outputs(
        id: u8,
        fee: u64,
        size: usize,
        inputs: Vec<Vec<u8>>,
        outputs: Vec<Vec<u8>>,
    ) -> PooledTransaction {
        PooledTransaction {
            id: vec![id],
            bytes: vec![0; size],
            fee,
            inputs,
            outputs,
            arrival_time: 1000 + id as u64,
        }
    }

    #[test]
    fn test_add_and_get() {
        let pool = Mempool::with_defaults();
        let tx = create_test_tx(1, 1000, 100);

        pool.add(tx.clone()).unwrap();

        assert!(pool.contains(&[1]));
        assert_eq!(pool.get(&[1]).unwrap().fee, 1000);
    }

    #[test]
    fn test_double_spend_detection() {
        let pool = Mempool::with_defaults();

        let tx1 = PooledTransaction {
            id: vec![1],
            bytes: vec![0; 100],
            fee: 1000,
            inputs: vec![vec![10, 10]],
            outputs: vec![vec![1, 1]],
            arrival_time: 1000,
        };

        let tx2 = PooledTransaction {
            id: vec![2],
            bytes: vec![0; 100],
            fee: 2000,
            inputs: vec![vec![10, 10]], // Same input!
            outputs: vec![vec![2, 2]],
            arrival_time: 1001,
        };

        pool.add(tx1).unwrap();

        let result = pool.add(tx2);
        assert!(matches!(result, Err(MempoolError::DoubleSpend(_))));
    }

    #[test]
    fn test_weight_ordering() {
        let pool = Mempool::with_defaults();

        pool.add(create_test_tx(1, 100, 100)).unwrap(); // weight ~1024
        pool.add(create_test_tx(2, 300, 100)).unwrap(); // weight ~3072
        pool.add(create_test_tx(3, 200, 100)).unwrap(); // weight ~2048

        let ordered = pool.get_by_weight(10);

        assert_eq!(ordered[0].id, vec![2]); // Highest weight first
        assert_eq!(ordered[1].id, vec![3]);
        assert_eq!(ordered[2].id, vec![1]);
    }

    // ============ Transaction Dependency Tests ============
    // Test that parent transactions get weight from children

    #[test]
    fn test_parent_weight_increases_with_child() {
        let pool = Mempool::with_defaults();

        // Parent transaction with low fee, creates output [1, 2]
        let parent = create_test_tx_with_outputs(
            1,
            100, // Low fee
            100,
            vec![vec![0, 0]], // Spends some external input
            vec![vec![1, 2]], // Creates output [1, 2]
        );

        pool.add(parent).unwrap();
        let parent_weight_before = pool.get_weight(&[1]).unwrap();

        // Child transaction with high fee, spends parent's output [1, 2]
        let child = create_test_tx_with_outputs(
            2,
            5000, // High fee
            100,
            vec![vec![1, 2]], // Spends parent's output
            vec![vec![2, 2]], // Creates its own output
        );

        pool.add(child).unwrap();
        let parent_weight_after = pool.get_weight(&[1]).unwrap();

        // Parent's weight should have increased by child's weight
        assert!(
            parent_weight_after > parent_weight_before,
            "Parent weight should increase: {} > {}",
            parent_weight_after,
            parent_weight_before
        );

        // Child's weight should be added to parent's
        let child_weight = pool.get_weight(&[2]).unwrap();
        assert_eq!(
            parent_weight_after,
            parent_weight_before + child_weight,
            "Parent weight should be original + child weight"
        );
    }

    #[test]
    fn test_parent_ordered_before_child() {
        let pool = Mempool::with_defaults();

        // Parent with very low fee
        let parent = create_test_tx_with_outputs(
            1,
            10, // Very low fee
            100,
            vec![vec![0, 0]],
            vec![vec![1, 2]],
        );

        // Unrelated transaction with medium fee
        let unrelated = create_test_tx_with_outputs(
            3,
            500, // Medium fee
            100,
            vec![vec![3, 0]],
            vec![vec![3, 2]],
        );

        // Child with high fee that spends parent's output
        let child = create_test_tx_with_outputs(
            2,
            10000, // Very high fee
            100,
            vec![vec![1, 2]], // Spends parent
            vec![vec![2, 2]],
        );

        pool.add(parent).unwrap();
        pool.add(unrelated).unwrap();
        pool.add(child).unwrap();

        let ordered = pool.get_by_weight(10);

        // Find positions
        let parent_pos = ordered.iter().position(|tx| tx.id == vec![1]).unwrap();
        let child_pos = ordered.iter().position(|tx| tx.id == vec![2]).unwrap();

        // Parent should come before child (lower position = higher priority)
        assert!(
            parent_pos < child_pos,
            "Parent (pos {}) should come before child (pos {})",
            parent_pos,
            child_pos
        );
    }

    #[test]
    fn test_grandparent_weight_propagation() {
        let pool = Mempool::with_defaults();

        // Grandparent
        let grandparent =
            create_test_tx_with_outputs(1, 100, 100, vec![vec![0, 0]], vec![vec![1, 2]]);

        // Parent spends grandparent's output
        let parent = create_test_tx_with_outputs(2, 100, 100, vec![vec![1, 2]], vec![vec![2, 2]]);

        // Child spends parent's output
        let child = create_test_tx_with_outputs(3, 10000, 100, vec![vec![2, 2]], vec![vec![3, 2]]);

        pool.add(grandparent).unwrap();
        pool.add(parent).unwrap();

        let grandparent_weight_before = pool.get_weight(&[1]).unwrap();
        let parent_weight_before = pool.get_weight(&[2]).unwrap();

        pool.add(child).unwrap();

        let grandparent_weight_after = pool.get_weight(&[1]).unwrap();
        let parent_weight_after = pool.get_weight(&[2]).unwrap();
        let child_weight = pool.get_weight(&[3]).unwrap();

        // Both parent and grandparent should have increased weight
        assert!(grandparent_weight_after > grandparent_weight_before);
        assert!(parent_weight_after > parent_weight_before);

        // Check ordering: grandparent < parent < child in position
        let ordered = pool.get_by_weight(10);
        let gp_pos = ordered.iter().position(|tx| tx.id == vec![1]).unwrap();
        let p_pos = ordered.iter().position(|tx| tx.id == vec![2]).unwrap();
        let c_pos = ordered.iter().position(|tx| tx.id == vec![3]).unwrap();

        assert!(gp_pos < p_pos, "Grandparent should come before parent");
        assert!(p_pos < c_pos, "Parent should come before child");
    }

    #[test]
    fn test_remove_decreases_parent_weight() {
        let pool = Mempool::with_defaults();

        let parent = create_test_tx_with_outputs(1, 100, 100, vec![vec![0, 0]], vec![vec![1, 2]]);

        let child = create_test_tx_with_outputs(2, 5000, 100, vec![vec![1, 2]], vec![vec![2, 2]]);

        pool.add(parent).unwrap();
        let parent_weight_initial = pool.get_weight(&[1]).unwrap();

        pool.add(child).unwrap();
        let parent_weight_with_child = pool.get_weight(&[1]).unwrap();

        // Remove child
        pool.remove(&[2]).unwrap();
        let parent_weight_after_remove = pool.get_weight(&[1]).unwrap();

        // Weight should return to original
        assert_eq!(
            parent_weight_after_remove, parent_weight_initial,
            "Parent weight should return to original after child removal"
        );
        assert!(parent_weight_with_child > parent_weight_after_remove);
    }

    // ============ Transaction Removal Tests ============

    #[test]
    fn test_remove_transaction() {
        let pool = Mempool::with_defaults();
        let tx = create_test_tx(1, 1000, 100);

        pool.add(tx).unwrap();
        assert!(pool.contains(&[1]));

        let removed = pool.remove(&[1]).unwrap();
        assert_eq!(removed.id, vec![1]);
        assert!(!pool.contains(&[1]));
    }

    #[test]
    fn test_remove_nonexistent() {
        let pool = Mempool::with_defaults();
        let result = pool.remove(&[99]);
        assert!(matches!(result, Err(MempoolError::NotFound(_))));
    }

    #[test]
    fn test_remove_frees_inputs() {
        let pool = Mempool::with_defaults();

        let tx1 = PooledTransaction {
            id: vec![1],
            bytes: vec![0; 100],
            fee: 1000,
            inputs: vec![vec![10, 10]],
            outputs: vec![vec![1, 1]],
            arrival_time: 1000,
        };

        pool.add(tx1).unwrap();
        assert!(pool.is_input_spent(&[10, 10]));

        pool.remove(&[1]).unwrap();
        assert!(!pool.is_input_spent(&[10, 10]));

        // Now we can add a tx spending the same input
        let tx2 = PooledTransaction {
            id: vec![2],
            bytes: vec![0; 100],
            fee: 2000,
            inputs: vec![vec![10, 10]],
            outputs: vec![vec![2, 2]],
            arrival_time: 1001,
        };

        assert!(pool.add(tx2).is_ok());
    }

    // ============ Pool Overflow Tests ============

    #[test]
    fn test_max_transactions_limit() {
        let config = MempoolConfig {
            max_transactions: 3,
            ..Default::default()
        };
        let pool = Mempool::new(config);

        pool.add(create_test_tx(1, 100, 100)).unwrap();
        pool.add(create_test_tx(2, 200, 100)).unwrap();
        pool.add(create_test_tx(3, 300, 100)).unwrap();

        // Pool is full, adding should evict lowest weight tx
        pool.add(create_test_tx(4, 400, 100)).unwrap();

        assert!(!pool.contains(&[1])); // Lowest weight evicted
        assert!(pool.contains(&[2]));
        assert!(pool.contains(&[3]));
        assert!(pool.contains(&[4]));
    }

    #[test]
    fn test_max_size_limit() {
        let config = MempoolConfig {
            max_size: 350,
            max_transactions: 100,
            ..Default::default()
        };
        let pool = Mempool::new(config);

        // Use 30-byte txs (which fit in max_size/10 = 35)
        for i in 1..=11 {
            pool.add(create_test_tx(i, i as u64 * 100, 30)).unwrap();
        }

        // Adding another should evict lowest weight
        pool.add(create_test_tx(12, 1200, 30)).unwrap();

        assert!(!pool.contains(&[1])); // Lowest weight evicted
        assert!(pool.contains(&[12]));
    }

    // ============ Stats Tests ============

    #[test]
    fn test_stats_tracking() {
        let pool = Mempool::with_defaults();

        let stats = pool.stats();
        assert_eq!(stats.tx_count, 0);
        assert_eq!(stats.total_size, 0);

        pool.add(create_test_tx(1, 1000, 100)).unwrap();
        pool.add(create_test_tx(2, 2000, 200)).unwrap();

        let stats = pool.stats();
        assert_eq!(stats.tx_count, 2);
        assert_eq!(stats.total_size, 300);
    }

    // ============ Clear Tests ============

    #[test]
    fn test_clear() {
        let pool = Mempool::with_defaults();

        pool.add(create_test_tx(1, 1000, 100)).unwrap();
        pool.add(create_test_tx(2, 2000, 100)).unwrap();
        pool.add(create_test_tx(3, 3000, 100)).unwrap();

        assert_eq!(pool.stats().tx_count, 3);

        pool.clear();

        assert_eq!(pool.stats().tx_count, 0);
        assert_eq!(pool.stats().total_size, 0);
        assert!(!pool.contains(&[1]));
        assert!(!pool.contains(&[2]));
        assert!(!pool.contains(&[3]));
    }

    // ============ Remove Confirmed Tests ============

    #[test]
    fn test_remove_confirmed() {
        let pool = Mempool::with_defaults();

        pool.add(create_test_tx(1, 1000, 100)).unwrap();
        pool.add(create_test_tx(2, 2000, 100)).unwrap();
        pool.add(create_test_tx(3, 3000, 100)).unwrap();

        let confirmed_ids = vec![vec![1u8], vec![2u8]];
        let spent_inputs: Vec<Vec<u8>> = vec![];

        pool.remove_confirmed(&confirmed_ids, &spent_inputs);

        assert!(!pool.contains(&[1]));
        assert!(!pool.contains(&[2]));
        assert!(pool.contains(&[3]));
    }

    #[test]
    fn test_remove_confirmed_by_spent_inputs() {
        let pool = Mempool::with_defaults();

        let tx1 = PooledTransaction {
            id: vec![1],
            bytes: vec![0; 100],
            fee: 1000,
            inputs: vec![vec![10, 10]],
            outputs: vec![vec![1, 1]],
            arrival_time: 1000,
        };

        let tx2 = PooledTransaction {
            id: vec![2],
            bytes: vec![0; 100],
            fee: 2000,
            inputs: vec![vec![20, 20]],
            outputs: vec![vec![2, 2]],
            arrival_time: 1001,
        };

        pool.add(tx1).unwrap();
        pool.add(tx2).unwrap();

        let confirmed_ids: Vec<Vec<u8>> = vec![];
        let spent_inputs = vec![vec![10u8, 10u8]];

        pool.remove_confirmed(&confirmed_ids, &spent_inputs);

        assert!(!pool.contains(&[1]));
        assert!(pool.contains(&[2]));
    }

    // ============ Output Tracking Tests ============

    #[test]
    fn test_output_tracking() {
        let pool = Mempool::with_defaults();

        let tx = PooledTransaction {
            id: vec![1],
            bytes: vec![0; 100],
            fee: 1000,
            inputs: vec![vec![0, 0]],
            outputs: vec![vec![1, 1], vec![1, 2]],
            arrival_time: 1000,
        };

        pool.add(tx).unwrap();

        assert_eq!(pool.get_creating_tx(&[1, 1]), Some(vec![1]));
        assert_eq!(pool.get_creating_tx(&[1, 2]), Some(vec![1]));
        assert_eq!(pool.get_creating_tx(&[9, 9]), None);
    }

    #[test]
    fn test_output_tracking_removed_on_remove() {
        let pool = Mempool::with_defaults();

        let tx = PooledTransaction {
            id: vec![1],
            bytes: vec![0; 100],
            fee: 1000,
            inputs: vec![vec![0, 0]],
            outputs: vec![vec![1, 1]],
            arrival_time: 1000,
        };

        pool.add(tx).unwrap();
        assert!(pool.get_creating_tx(&[1, 1]).is_some());

        pool.remove(&[1]).unwrap();
        assert!(pool.get_creating_tx(&[1, 1]).is_none());
    }
}
