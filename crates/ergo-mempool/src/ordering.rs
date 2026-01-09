//! Transaction ordering by weight (fee + child transaction weights).
//!
//! This module implements the weighted transaction ordering from the Scala node's
//! `OrderedTxPool`. When a transaction spends outputs of another mempool transaction,
//! the parent's weight is increased by the child's weight, ensuring parents are
//! always processed before children.

use std::cmp::Ordering;

/// Weighted transaction identifier for ordering.
///
/// Corresponds to Scala's `WeightedTxId`. Weight starts as fee_per_factor but
/// increases when child transactions arrive that spend this transaction's outputs.
#[derive(Debug, Clone)]
pub struct WeightedTxId {
    /// Transaction ID.
    pub tx_id: Vec<u8>,
    /// Weight of transaction (fee_per_factor + accumulated child weights).
    /// Higher weight = higher priority.
    pub weight: i64,
    /// Original fee per factor (fee * 1024 / size for precision).
    pub fee_per_factor: i64,
    /// Transaction size in bytes (the factor).
    pub size: usize,
    /// Creation/arrival time (unix timestamp in millis).
    pub created: u64,
}

impl WeightedTxId {
    /// Create a new weighted transaction ID.
    ///
    /// Initial weight equals fee_per_factor. Weight may be increased later
    /// when child transactions arrive.
    pub fn new(tx_id: Vec<u8>, fee: u64, size: usize, created: u64) -> Self {
        // Multiply by 1024 for better precision (matches Scala implementation)
        let fee_per_factor = if size == 0 {
            0
        } else {
            (fee as i64 * 1024) / size as i64
        };
        Self {
            tx_id,
            weight: fee_per_factor,
            fee_per_factor,
            size,
            created,
        }
    }

    /// Create with explicit weight (used when updating family weights).
    pub fn with_weight(
        tx_id: Vec<u8>,
        weight: i64,
        fee_per_factor: i64,
        size: usize,
        created: u64,
    ) -> Self {
        Self {
            tx_id,
            weight,
            fee_per_factor,
            size,
            created,
        }
    }

    /// Get the fee per byte (for display/stats).
    pub fn fee_per_byte(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            self.fee_per_factor as f64 / 1024.0
        }
    }

    /// Get original fee (reconstructed from fee_per_factor).
    pub fn fee(&self) -> u64 {
        ((self.fee_per_factor as i128 * self.size as i128) / 1024) as u64
    }
}

impl PartialEq for WeightedTxId {
    fn eq(&self, other: &Self) -> bool {
        // ID uniquely identifies the transaction (matches Scala)
        self.tx_id == other.tx_id
    }
}

impl Eq for WeightedTxId {}

impl std::hash::Hash for WeightedTxId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tx_id.hash(state);
    }
}

impl PartialOrd for WeightedTxId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WeightedTxId {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher weight = higher priority (comes first in iteration)
        // Matches Scala: Ordering[(Long, ModifierId)].on(x => (-x.weight, x.id))
        match other.weight.cmp(&self.weight) {
            Ordering::Equal => {
                // If equal weight, order by ID for determinism
                self.tx_id.cmp(&other.tx_id)
            }
            ord => ord,
        }
    }
}

/// Legacy fee ordering (for backwards compatibility).
/// Deprecated: Use WeightedTxId instead.
#[derive(Debug, Clone)]
pub struct FeeOrdering {
    /// Transaction ID.
    pub tx_id: Vec<u8>,
    /// Transaction fee in nanoERG.
    pub fee: u64,
    /// Transaction size in bytes.
    pub size: usize,
    /// Arrival time (unix timestamp in millis).
    pub arrival_time: u64,
}

impl FeeOrdering {
    /// Create a new fee ordering entry.
    pub fn new(tx_id: Vec<u8>, fee: u64, size: usize, arrival_time: u64) -> Self {
        Self {
            tx_id,
            fee,
            size,
            arrival_time,
        }
    }

    /// Calculate fee per byte (for ordering).
    pub fn fee_per_byte(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            self.fee as f64 / self.size as f64
        }
    }
}

impl PartialEq for FeeOrdering {
    fn eq(&self, other: &Self) -> bool {
        self.tx_id == other.tx_id
    }
}

impl Eq for FeeOrdering {}

impl PartialOrd for FeeOrdering {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FeeOrdering {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher fee per byte = higher priority
        let self_fpb = self.fee_per_byte();
        let other_fpb = other.fee_per_byte();

        // Compare by fee per byte first (reverse so higher fee comes first in BTreeSet)
        match other_fpb.partial_cmp(&self_fpb) {
            Some(Ordering::Equal) | None => {
                // If equal, prefer earlier arrival (lower timestamp first)
                self.arrival_time.cmp(&other.arrival_time)
            }
            Some(ord) => ord,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn test_fee_ordering() {
        let tx1 = FeeOrdering::new(vec![1], 1000, 100, 1000); // 10 per byte
        let tx2 = FeeOrdering::new(vec![2], 2000, 100, 1001); // 20 per byte
        let tx3 = FeeOrdering::new(vec![3], 1000, 100, 999); // 10 per byte, earlier

        let mut set = BTreeSet::new();
        set.insert(tx1.clone());
        set.insert(tx2.clone());
        set.insert(tx3.clone());

        let ordered: Vec<_> = set.into_iter().collect();

        // tx2 should be first (highest fee), then tx3 (earlier), then tx1
        assert_eq!(ordered[0].tx_id, vec![2]);
        assert_eq!(ordered[1].tx_id, vec![3]);
        assert_eq!(ordered[2].tx_id, vec![1]);
    }

    #[test]
    fn test_weighted_tx_id_ordering() {
        // tx1: 1000 fee, 100 bytes -> fee_per_factor = 1000 * 1024 / 100 = 10240
        let tx1 = WeightedTxId::new(vec![1], 1000, 100, 1000);
        // tx2: 2000 fee, 100 bytes -> fee_per_factor = 2000 * 1024 / 100 = 20480
        let tx2 = WeightedTxId::new(vec![2], 2000, 100, 1001);
        // tx3: 500 fee, 100 bytes -> fee_per_factor = 500 * 1024 / 100 = 5120
        let tx3 = WeightedTxId::new(vec![3], 500, 100, 999);

        let mut set = BTreeSet::new();
        set.insert(tx1.clone());
        set.insert(tx2.clone());
        set.insert(tx3.clone());

        let ordered: Vec<_> = set.into_iter().collect();

        // tx2 should be first (highest weight), then tx1, then tx3
        assert_eq!(ordered[0].tx_id, vec![2]);
        assert_eq!(ordered[1].tx_id, vec![1]);
        assert_eq!(ordered[2].tx_id, vec![3]);
    }

    #[test]
    fn test_weighted_tx_id_with_child_weight() {
        // Parent tx with low fee
        let parent = WeightedTxId::new(vec![1], 100, 100, 1000);
        // Child tx with high fee
        let child = WeightedTxId::new(vec![2], 5000, 100, 1001);

        // Parent's weight should increase by child's weight
        let parent_updated = WeightedTxId::with_weight(
            parent.tx_id.clone(),
            parent.weight + child.weight,
            parent.fee_per_factor,
            parent.size,
            parent.created,
        );

        // Now parent should have higher priority than a tx with medium fee
        let medium = WeightedTxId::new(vec![3], 2000, 100, 1002);

        let mut set = BTreeSet::new();
        set.insert(parent_updated.clone());
        set.insert(child.clone());
        set.insert(medium.clone());

        let ordered: Vec<_> = set.into_iter().collect();

        // parent_updated has weight = 1024 + 51200 = 52224 (highest)
        // child has weight = 51200
        // medium has weight = 20480
        assert_eq!(ordered[0].tx_id, vec![1]); // parent (with child weight)
        assert_eq!(ordered[1].tx_id, vec![2]); // child
        assert_eq!(ordered[2].tx_id, vec![3]); // medium
    }

    #[test]
    fn test_weighted_tx_id_equality() {
        let tx1 = WeightedTxId::new(vec![1], 1000, 100, 1000);
        let tx2 = WeightedTxId::with_weight(vec![1], 5000, 1000, 100, 2000);

        // Same ID means equal, even with different weights
        assert_eq!(tx1, tx2);
    }
}
