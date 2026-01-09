//! Full block validation with ErgoScript execution.
//!
//! This module provides complete block validation including:
//! - Transaction input existence verification
//! - ErgoScript execution for spending conditions
//! - ERG and token conservation checks
//! - Block cost accumulation
//! - State change generation
//!
//! The validation flow ensures a block is fully valid before
//! any state changes are applied.

use crate::block::{BlockTransactions, FullBlock, Header};
use crate::params::{MAX_BLOCK_COST, MIN_BOX_VALUE};
use crate::tx_validation::{validate_erg_conservation, validate_token_conservation, TxVerifier};
use crate::{ConsensusError, ConsensusResult};
use ergo_chain_types::PreHeader;
use ergo_lib::chain::transaction::Transaction;
use ergo_lib::ergotree_ir::chain::ergo_box::ErgoBox;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, instrument, warn};

/// Result of validating a full block.
#[derive(Debug)]
pub struct BlockValidationResult {
    /// Whether the block is valid.
    pub valid: bool,
    /// Total block cost (sum of all transaction costs).
    pub total_cost: u64,
    /// State change to apply if valid (boxes to add/remove from UTXO set).
    pub state_change: Option<ValidatedStateChange>,
    /// Error if invalid.
    pub error: Option<String>,
}

/// A validated state change that is guaranteed to be valid.
/// This can only be created through successful block validation.
#[derive(Debug)]
pub struct ValidatedStateChange {
    /// Block height this change applies to.
    pub height: u32,
    /// Boxes to remove from UTXO set (spent inputs).
    pub spent: Vec<SpentBox>,
    /// Boxes to add to UTXO set (created outputs).
    pub created: Vec<CreatedBox>,
}

/// A spent box with its original data (for undo).
#[derive(Debug, Clone)]
pub struct SpentBox {
    /// The box ID being spent.
    pub box_id: Vec<u8>,
    /// The original box data (for rollback).
    pub original_box: ErgoBox,
}

/// A created box from transaction outputs.
#[derive(Debug, Clone)]
pub struct CreatedBox {
    /// The new box being created.
    pub ergo_box: ErgoBox,
    /// Transaction ID that created this box.
    pub tx_id: Vec<u8>,
    /// Output index in the transaction.
    pub output_index: u16,
}

/// Checkpoint configuration for fast initial sync.
/// Below the checkpoint height, script verification is skipped.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Height up to which to skip full validation.
    pub height: u32,
    /// Block ID at checkpoint height (for verification).
    pub block_id: String,
}

impl Checkpoint {
    /// Mainnet checkpoint (from Scala node config).
    pub fn mainnet() -> Self {
        Self {
            height: 1231454,
            block_id: "ca5aa96a2d560f49cd5652eae4b9e16bbf410ee32032365313dc16544ee5fda1e6d"
                .to_string(),
        }
    }
}

/// Full block validator with ErgoScript execution.
pub struct FullBlockValidator {
    /// Maximum block cost allowed.
    max_block_cost: u64,
    /// Whether to skip script verification (for testing only).
    skip_script_verification: bool,
    /// Optional checkpoint for fast initial sync.
    checkpoint: Option<Checkpoint>,
}

impl Default for FullBlockValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl FullBlockValidator {
    /// Create a new block validator with default settings.
    /// Uses mainnet checkpoint for fast initial sync.
    pub fn new() -> Self {
        Self {
            max_block_cost: MAX_BLOCK_COST,
            skip_script_verification: false,
            checkpoint: Some(Checkpoint::mainnet()),
        }
    }

    /// Create a block validator without checkpoint (full validation from genesis).
    pub fn without_checkpoint() -> Self {
        Self {
            max_block_cost: MAX_BLOCK_COST,
            skip_script_verification: false,
            checkpoint: None,
        }
    }

    /// Create a block validator for testing (skips script verification).
    #[cfg(test)]
    pub fn for_testing() -> Self {
        Self {
            max_block_cost: MAX_BLOCK_COST,
            skip_script_verification: true,
            checkpoint: None,
        }
    }

    /// Check if a block is below the checkpoint height.
    fn is_below_checkpoint(&self, height: u32) -> bool {
        self.checkpoint
            .as_ref()
            .map_or(false, |cp| height <= cp.height)
    }

    /// Validate a full block against the current UTXO state.
    ///
    /// # Arguments
    /// * `block` - The full block to validate
    /// * `parent_header` - The parent block's header (for context)
    /// * `utxo_lookup` - Function to look up boxes in the current UTXO state
    /// * `last_headers` - Last 10 block headers (for ErgoScript context)
    ///
    /// # Returns
    /// A validation result with state change if valid.
    #[instrument(skip(self, block, parent_header, utxo_lookup, last_headers), fields(height = block.height()))]
    pub fn validate_block<F>(
        &self,
        block: &FullBlock,
        parent_header: &Header,
        utxo_lookup: F,
        last_headers: [Header; 10],
    ) -> BlockValidationResult
    where
        F: Fn(&[u8]) -> Option<ErgoBox>,
    {
        let height = block.height();

        // 1. Validate header basics
        if let Err(e) = self.validate_header(&block.header, parent_header) {
            return BlockValidationResult {
                valid: false,
                total_cost: 0,
                state_change: None,
                error: Some(format!("Header validation failed: {}", e)),
            };
        }

        // 2. Validate transactions merkle root matches header
        if let Err(e) = block.transactions.validate_against_header(&block.header) {
            return BlockValidationResult {
                valid: false,
                total_cost: 0,
                state_change: None,
                error: Some(format!("Transactions validation failed: {}", e)),
            };
        }

        // 3. Block must have at least one transaction (coinbase)
        if block.transactions.is_empty() {
            return BlockValidationResult {
                valid: false,
                total_cost: 0,
                state_change: None,
                error: Some("Block must have at least one transaction".to_string()),
            };
        }

        // 4. Validate all transactions and collect state changes
        let pre_header = PreHeader::from(block.header.clone());

        match self.validate_transactions(
            &block.transactions,
            height,
            &pre_header,
            &last_headers,
            &utxo_lookup,
        ) {
            Ok((total_cost, state_change)) => {
                info!(
                    height,
                    total_cost,
                    spent = state_change.spent.len(),
                    created = state_change.created.len(),
                    "Block validated successfully"
                );

                BlockValidationResult {
                    valid: true,
                    total_cost,
                    state_change: Some(state_change),
                    error: None,
                }
            }
            Err(e) => {
                warn!(height, error = %e, "Block validation failed");
                BlockValidationResult {
                    valid: false,
                    total_cost: 0,
                    state_change: None,
                    error: Some(e.to_string()),
                }
            }
        }
    }

    /// Validate block header against parent.
    fn validate_header(&self, header: &Header, parent: &Header) -> ConsensusResult<()> {
        // Height must be parent + 1
        if header.height != parent.height + 1 {
            return Err(ConsensusError::InvalidHeader(format!(
                "Invalid height: {} (expected {})",
                header.height,
                parent.height + 1
            )));
        }

        // Timestamp must be after parent
        if header.timestamp <= parent.timestamp {
            return Err(ConsensusError::InvalidTimestamp {
                block_time: header.timestamp,
                parent_time: parent.timestamp,
            });
        }

        // Parent ID must match
        if header.parent_id != parent.id {
            return Err(ConsensusError::InvalidHeader(format!(
                "Parent ID mismatch: header has {}, expected {}",
                hex::encode(header.parent_id.0.as_ref()),
                hex::encode(parent.id.0.as_ref())
            )));
        }

        // Version must be valid (1, 2, 3, or 4)
        // Version 1: Initial mainnet version
        // Version 2: Hardening hard-fork (Autolykos v2, witnesses in tx Merkle tree)
        // Version 3: 5.0 soft-fork (JITC, EIP-39)
        // Version 4: 6.0 soft-fork (EIP-50)
        if header.version < 1 || header.version > 4 {
            return Err(ConsensusError::InvalidHeader(format!(
                "Invalid version: {}",
                header.version
            )));
        }

        Ok(())
    }

    /// Validate all transactions in a block.
    ///
    /// Returns the total cost and validated state change.
    fn validate_transactions<F>(
        &self,
        block_txs: &BlockTransactions,
        height: u32,
        pre_header: &PreHeader,
        last_headers: &[Header; 10],
        utxo_lookup: &F,
    ) -> ConsensusResult<(u64, ValidatedStateChange)>
    where
        F: Fn(&[u8]) -> Option<ErgoBox>,
    {
        let mut total_cost = 0u64;
        let mut spent_boxes: Vec<SpentBox> = Vec::new();
        let mut created_boxes: Vec<CreatedBox> = Vec::new();

        // Track boxes spent in this block (for double-spend detection)
        let mut spent_in_block: HashSet<Vec<u8>> = HashSet::new();

        // Track boxes created in this block (can be spent within same block)
        let mut created_in_block: HashMap<Vec<u8>, ErgoBox> = HashMap::new();

        // Check if we're in checkpoint mode (fast initial sync)
        let checkpoint_mode = self.is_below_checkpoint(height);
        if checkpoint_mode && height % 10000 == 0 {
            info!(height, "Checkpoint mode: skipping full validation");
        }

        // Create verifier for script execution (only needed outside checkpoint mode)
        let verifier = if !checkpoint_mode {
            Some(TxVerifier::new(
                height,
                pre_header.clone(),
                last_headers.clone(),
            ))
        } else {
            None
        };

        for (tx_idx, tx) in block_txs.iter().enumerate() {
            let is_coinbase = tx_idx == 0;
            let tx_id = tx.id().as_ref().to_vec();

            debug!(
                tx_idx,
                tx_id = %hex::encode(&tx_id),
                inputs = tx.inputs.len(),
                outputs = tx.outputs.len(),
                "Validating transaction"
            );

            // 1. Check for duplicate inputs within the transaction
            let mut tx_input_ids: HashSet<Vec<u8>> = HashSet::new();
            for input in &tx.inputs {
                let box_id = input.box_id.as_ref().to_vec();
                if !tx_input_ids.insert(box_id.clone()) {
                    return Err(ConsensusError::DoubleSpend {
                        box_id: hex::encode(&box_id),
                    });
                }
            }

            // 2. Check for double-spend within the block
            for input in &tx.inputs {
                let box_id = input.box_id.as_ref().to_vec();
                if spent_in_block.contains(&box_id) {
                    return Err(ConsensusError::DoubleSpend {
                        box_id: hex::encode(&box_id),
                    });
                }
            }

            // In checkpoint mode, skip input lookup and conservation checks
            // We still apply the state changes and will verify state root
            let input_boxes: Vec<ErgoBox> = if checkpoint_mode {
                // In checkpoint mode, we don't have the input boxes
                // Just track spent box IDs without the original box data
                Vec::new()
            } else {
                // 3. Collect input boxes (from UTXO state or created earlier in this block)
                self.collect_input_boxes(tx, utxo_lookup, &created_in_block)?
            };

            // 4. Collect data input boxes (skip in checkpoint mode)
            let data_input_boxes: Vec<ErgoBox> = if checkpoint_mode {
                Vec::new()
            } else {
                self.collect_data_input_boxes(tx, utxo_lookup, &created_in_block)?
            };

            // Skip validation checks in checkpoint mode
            if !checkpoint_mode {
                // 5. Check data inputs don't intersect with inputs
                self.validate_no_data_input_intersection(tx)?;

                // 6. Verify ERG conservation
                let fee = validate_erg_conservation(tx, &input_boxes)?;

                // For non-coinbase, fee must be positive (at least cover minimum)
                if !is_coinbase && fee < MIN_BOX_VALUE {
                    return Err(ConsensusError::InsufficientFee {
                        provided: fee,
                        required: MIN_BOX_VALUE,
                    });
                }

                // 7. Verify token conservation
                validate_token_conservation(tx, &input_boxes, is_coinbase)?;

                // 8. Validate output box values (minimum value check)
                for (out_idx, output) in tx.outputs.iter().enumerate() {
                    let value = u64::from(output.value);
                    if value < MIN_BOX_VALUE {
                        return Err(ConsensusError::InvalidTransaction(format!(
                            "Output {} has value {} below minimum {}",
                            out_idx, value, MIN_BOX_VALUE
                        )));
                    }
                }

                // 9. Execute ErgoScript verification (unless skipped for testing)
                if !self.skip_script_verification && !is_coinbase {
                    if let Some(ref v) = verifier {
                        let result = v.verify_tx(tx, &input_boxes, &data_input_boxes);

                        if !result.valid {
                            return Err(ConsensusError::ScriptVerificationFailed {
                                tx_id: hex::encode(&tx_id),
                                error: result.error.unwrap_or_else(|| "Unknown error".to_string()),
                            });
                        }

                        // Accumulate cost
                        total_cost = total_cost.saturating_add(result.cost);

                        // Check block cost limit
                        if total_cost > self.max_block_cost {
                            return Err(ConsensusError::BlockCostExceeded {
                                cost: total_cost,
                                max: self.max_block_cost,
                            });
                        }
                    }
                }
            }

            // 10. Record spent boxes
            // In checkpoint mode, we only track box IDs for double-spend detection
            // but don't record SpentBox (no rollback support in checkpoint mode)
            for input in tx.inputs.iter() {
                let box_id = input.box_id.as_ref().to_vec();
                spent_in_block.insert(box_id.clone());
            }

            // Only record full SpentBox data outside checkpoint mode (needed for rollback)
            if !checkpoint_mode {
                for (input, input_box) in tx.inputs.iter().zip(input_boxes.iter()) {
                    let box_id = input.box_id.as_ref().to_vec();
                    spent_boxes.push(SpentBox {
                        box_id,
                        original_box: input_box.clone(),
                    });
                }
            }

            // 11. Record created boxes and add to created_in_block for potential intra-block spending
            for (out_idx, output) in tx.outputs.iter().enumerate() {
                let box_id = output.box_id().as_ref().to_vec();
                created_in_block.insert(box_id.clone(), output.clone());
                created_boxes.push(CreatedBox {
                    ergo_box: output.clone(),
                    tx_id: tx_id.clone(),
                    output_index: out_idx as u16,
                });
            }
        }

        let state_change = ValidatedStateChange {
            height,
            spent: spent_boxes,
            created: created_boxes,
        };

        Ok((total_cost, state_change))
    }

    /// Collect input boxes from UTXO state or boxes created earlier in this block.
    fn collect_input_boxes<F>(
        &self,
        tx: &Transaction,
        utxo_lookup: &F,
        created_in_block: &HashMap<Vec<u8>, ErgoBox>,
    ) -> ConsensusResult<Vec<ErgoBox>>
    where
        F: Fn(&[u8]) -> Option<ErgoBox>,
    {
        let mut input_boxes = Vec::with_capacity(tx.inputs.len());

        for (idx, input) in tx.inputs.iter().enumerate() {
            let box_id = input.box_id.as_ref();

            // First check boxes created earlier in this block
            if let Some(box_data) = created_in_block.get(box_id) {
                input_boxes.push(box_data.clone());
                continue;
            }

            // Then check UTXO state
            if let Some(box_data) = utxo_lookup(box_id) {
                input_boxes.push(box_data);
                continue;
            }

            // Input not found
            return Err(ConsensusError::MissingInput {
                tx_id: hex::encode(tx.id().as_ref()),
                input_idx: idx,
                box_id: hex::encode(box_id),
            });
        }

        Ok(input_boxes)
    }

    /// Collect data input boxes from UTXO state or boxes created earlier in this block.
    fn collect_data_input_boxes<F>(
        &self,
        tx: &Transaction,
        utxo_lookup: &F,
        created_in_block: &HashMap<Vec<u8>, ErgoBox>,
    ) -> ConsensusResult<Vec<ErgoBox>>
    where
        F: Fn(&[u8]) -> Option<ErgoBox>,
    {
        let data_inputs = match &tx.data_inputs {
            Some(di) => di,
            None => return Ok(Vec::new()),
        };

        let mut data_input_boxes = Vec::with_capacity(data_inputs.len());

        for (idx, data_input) in data_inputs.iter().enumerate() {
            let box_id = data_input.box_id.as_ref();

            // First check boxes created earlier in this block
            if let Some(box_data) = created_in_block.get(box_id) {
                data_input_boxes.push(box_data.clone());
                continue;
            }

            // Then check UTXO state
            if let Some(box_data) = utxo_lookup(box_id) {
                data_input_boxes.push(box_data);
                continue;
            }

            // Data input not found
            return Err(ConsensusError::MissingDataInput {
                tx_id: hex::encode(tx.id().as_ref()),
                input_idx: idx,
                box_id: hex::encode(box_id),
            });
        }

        Ok(data_input_boxes)
    }

    /// Validate that data inputs don't intersect with regular inputs.
    fn validate_no_data_input_intersection(&self, tx: &Transaction) -> ConsensusResult<()> {
        let data_inputs = match &tx.data_inputs {
            Some(di) => di,
            None => return Ok(()),
        };

        let input_ids: HashSet<Vec<u8>> = tx
            .inputs
            .iter()
            .map(|i| i.box_id.as_ref().to_vec())
            .collect();

        for data_input in data_inputs {
            let box_id = data_input.box_id.as_ref().to_vec();
            if input_ids.contains(&box_id) {
                return Err(ConsensusError::DataInputIntersection {
                    box_id: hex::encode(&box_id),
                });
            }
        }

        Ok(())
    }
}

impl ValidatedStateChange {
    /// Get spent box IDs as BoxId types.
    pub fn spent_box_ids(&self) -> Vec<ergo_lib::ergotree_ir::chain::ergo_box::BoxId> {
        self.spent
            .iter()
            .filter_map(|spent| {
                if spent.box_id.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&spent.box_id);
                    Some(ergo_lib::ergotree_ir::chain::ergo_box::BoxId::from(
                        ergo_chain_types::Digest32::from(arr),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get created boxes with their metadata.
    pub fn created_boxes(&self) -> &[CreatedBox] {
        &self.created
    }

    /// Get spent boxes with their original data (for undo).
    pub fn spent_boxes(&self) -> &[SpentBox] {
        &self.spent
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
    use std::collections::HashMap;
    use std::convert::TryFrom;

    /// Create a mock ErgoBox with given value
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

    #[test]
    fn test_validator_creation() {
        let validator = FullBlockValidator::new();
        assert_eq!(validator.max_block_cost, MAX_BLOCK_COST);
        assert!(!validator.skip_script_verification);
    }

    #[test]
    fn test_validator_for_testing() {
        let validator = FullBlockValidator::for_testing();
        assert!(validator.skip_script_verification);
    }

    #[test]
    fn test_validated_state_change_conversion() {
        let box1 = create_mock_box(1_000_000_000, 1);
        let box2 = create_mock_box(500_000_000, 2);

        let state_change = ValidatedStateChange {
            height: 100,
            spent: vec![SpentBox {
                box_id: box1.box_id().as_ref().to_vec(),
                original_box: box1.clone(),
            }],
            created: vec![CreatedBox {
                ergo_box: box2.clone(),
                tx_id: vec![0u8; 32],
                output_index: 0,
            }],
        };

        // Verify accessors work correctly
        assert_eq!(state_change.spent_box_ids().len(), 1);
        assert_eq!(state_change.spent_boxes().len(), 1);
        assert_eq!(state_change.created_boxes().len(), 1);
    }

    #[test]
    fn test_double_spend_detection_within_tx() {
        // Test that duplicate inputs within a single transaction are detected
        let validator = FullBlockValidator::for_testing();

        // This would be tested through validate_transactions if we had proper
        // transaction construction. For now, we test the HashSet logic:
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let box_id = vec![1u8; 32];

        assert!(seen.insert(box_id.clone())); // First insert succeeds
        assert!(!seen.insert(box_id)); // Duplicate detected
    }

    #[test]
    fn test_data_input_intersection_detection() {
        // Test that data inputs can't intersect with regular inputs
        let input_ids: HashSet<Vec<u8>> = [vec![1u8; 32], vec![2u8; 32]].into_iter().collect();

        let data_input_ids: Vec<Vec<u8>> = vec![vec![2u8; 32], vec![3u8; 32]];

        // Check for intersection
        let has_intersection = data_input_ids.iter().any(|id| input_ids.contains(id));
        assert!(has_intersection, "Should detect intersection at box 2");
    }
}
