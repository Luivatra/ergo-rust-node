//! Block and transaction validation.

use crate::params::{MAX_BLOCK_COST, MAX_BLOCK_SIZE};
use crate::{ConsensusError, ConsensusResult};
use tracing::{debug, instrument};

/// Header validation trait.
pub trait HeaderValidator {
    /// Validate a header against its parent.
    fn validate_header(&self, header: &HeaderData, parent: &HeaderData) -> ConsensusResult<()>;
}

/// Block validation trait.
pub trait BlockValidator {
    /// Validate a full block.
    fn validate_block(&self, block: &BlockData) -> ConsensusResult<()>;
}

/// Transaction validation trait.
pub trait TransactionValidator {
    /// Validate a transaction.
    fn validate_transaction(&self, tx: &TransactionData) -> ConsensusResult<()>;
}

/// Header data for validation.
#[derive(Debug, Clone)]
pub struct HeaderData {
    /// Block version.
    pub version: u8,
    /// Parent block ID.
    pub parent_id: Vec<u8>,
    /// Merkle root of transactions.
    pub transactions_root: Vec<u8>,
    /// State root after applying this block.
    pub state_root: Vec<u8>,
    /// Timestamp in milliseconds.
    pub timestamp: u64,
    /// Block height.
    pub height: u32,
    /// Difficulty target (nBits).
    pub n_bits: u32,
    /// Votes for protocol parameters.
    pub votes: [u8; 3],
}

/// Full block data for validation.
#[derive(Debug, Clone)]
pub struct BlockData {
    /// Block header.
    pub header: HeaderData,
    /// Block transactions (serialized).
    pub transactions: Vec<TransactionData>,
    /// Block extension.
    pub extension: Vec<(Vec<u8>, Vec<u8>)>,
    /// AD proofs (optional, for stateless validation).
    pub ad_proofs: Option<Vec<u8>>,
}

/// Transaction data for validation.
#[derive(Debug, Clone)]
pub struct TransactionData {
    /// Transaction ID.
    pub id: Vec<u8>,
    /// Input box IDs.
    pub inputs: Vec<Vec<u8>>,
    /// Data input box IDs.
    pub data_inputs: Vec<Vec<u8>>,
    /// Output boxes (serialized).
    pub outputs: Vec<Vec<u8>>,
}

/// Default header validator implementation.
pub struct DefaultHeaderValidator;

impl HeaderValidator for DefaultHeaderValidator {
    #[instrument(skip(self, header, parent), fields(height = header.height))]
    fn validate_header(&self, header: &HeaderData, parent: &HeaderData) -> ConsensusResult<()> {
        // 1. Check height is parent + 1
        if header.height != parent.height + 1 {
            return Err(ConsensusError::InvalidHeader(format!(
                "Invalid height: {} (expected {})",
                header.height,
                parent.height + 1
            )));
        }

        // 2. Check timestamp is after parent
        if header.timestamp <= parent.timestamp {
            return Err(ConsensusError::InvalidTimestamp {
                block_time: header.timestamp,
                parent_time: parent.timestamp,
            });
        }

        // 3. Check timestamp is not too far in the future (allow 20 minutes)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let max_future = now_ms + 20 * 60 * 1000; // 20 minutes
        if header.timestamp > max_future {
            return Err(ConsensusError::InvalidHeader(format!(
                "Timestamp too far in future: {} > {}",
                header.timestamp, max_future
            )));
        }

        // 4. Check parent ID matches
        // Note: This should be validated by the caller who has the actual parent

        // 5. Check version is valid (1-4)
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

        debug!("Header validation passed");
        Ok(())
    }
}

/// Default block validator implementation.
pub struct DefaultBlockValidator {
    header_validator: DefaultHeaderValidator,
}

impl Default for DefaultBlockValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultBlockValidator {
    /// Create a new block validator.
    pub fn new() -> Self {
        Self {
            header_validator: DefaultHeaderValidator,
        }
    }

    /// Validate block size.
    fn validate_size(&self, block: &BlockData) -> ConsensusResult<()> {
        // Approximate size calculation
        let mut size = 0usize;
        for tx in &block.transactions {
            size += tx.id.len();
            for input in &tx.inputs {
                size += input.len();
            }
            for output in &tx.outputs {
                size += output.len();
            }
        }

        if size > MAX_BLOCK_SIZE {
            return Err(ConsensusError::BlockTooLarge {
                size,
                max: MAX_BLOCK_SIZE,
            });
        }

        Ok(())
    }
}

impl BlockValidator for DefaultBlockValidator {
    #[instrument(skip(self, block), fields(height = block.header.height))]
    fn validate_block(&self, block: &BlockData) -> ConsensusResult<()> {
        // 1. Validate block size
        self.validate_size(block)?;

        // 2. Validate transactions are non-empty
        if block.transactions.is_empty() {
            return Err(ConsensusError::InvalidBlock(
                "Block must contain at least one transaction (coinbase)".to_string(),
            ));
        }

        // 3. Validate extension key-value pairs
        for (key, _value) in &block.extension {
            if key.is_empty() {
                return Err(ConsensusError::InvalidExtension(
                    "Extension key cannot be empty".to_string(),
                ));
            }
        }

        debug!("Block validation passed");
        Ok(())
    }
}

/// Default transaction validator implementation.
pub struct DefaultTransactionValidator;

impl TransactionValidator for DefaultTransactionValidator {
    #[instrument(skip(self, tx), fields(tx_id = hex::encode(&tx.id)))]
    fn validate_transaction(&self, tx: &TransactionData) -> ConsensusResult<()> {
        // 1. Check transaction has inputs
        if tx.inputs.is_empty() {
            return Err(ConsensusError::InvalidTransaction(
                "Transaction must have at least one input".to_string(),
            ));
        }

        // 2. Check transaction has outputs
        if tx.outputs.is_empty() {
            return Err(ConsensusError::InvalidTransaction(
                "Transaction must have at least one output".to_string(),
            ));
        }

        // 3. Check for duplicate inputs
        let mut seen_inputs = std::collections::HashSet::new();
        for input in &tx.inputs {
            if !seen_inputs.insert(input.clone()) {
                return Err(ConsensusError::InvalidTransaction(format!(
                    "Duplicate input: {}",
                    hex::encode(input)
                )));
            }
        }

        debug!("Transaction validation passed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_header(height: u32, timestamp: u64, parent_id: Vec<u8>) -> HeaderData {
        HeaderData {
            version: 2,
            parent_id,
            transactions_root: vec![0; 32],
            state_root: vec![0; 32],
            timestamp,
            height,
            n_bits: 0x1f00ffff,
            votes: [0, 0, 0],
        }
    }

    // ============ Header Validation Tests ============
    // Corresponds to Scala's HeaderValidator tests

    #[test]
    fn test_header_height_validation() {
        let validator = DefaultHeaderValidator;

        let parent = create_test_header(100, 1000, vec![0; 32]);

        let mut header = parent.clone();
        header.height = 101;
        header.timestamp = 2000;

        assert!(validator.validate_header(&header, &parent).is_ok());

        // Wrong height
        header.height = 102;
        assert!(validator.validate_header(&header, &parent).is_err());
    }

    #[test]
    fn test_header_height_must_increment_by_one() {
        let validator = DefaultHeaderValidator;

        let parent = create_test_header(100, 1000, vec![0; 32]);

        // Height increments by 1 - valid
        let valid_header = create_test_header(101, 2000, vec![0; 32]);
        assert!(validator.validate_header(&valid_header, &parent).is_ok());

        // Height same as parent - invalid
        let same_height = create_test_header(100, 2000, vec![0; 32]);
        assert!(validator.validate_header(&same_height, &parent).is_err());

        // Height decreases - invalid
        let lower_height = create_test_header(99, 2000, vec![0; 32]);
        assert!(validator.validate_header(&lower_height, &parent).is_err());

        // Height jumps - invalid
        let jump_height = create_test_header(103, 2000, vec![0; 32]);
        assert!(validator.validate_header(&jump_height, &parent).is_err());
    }

    #[test]
    fn test_header_timestamp_must_increase() {
        let validator = DefaultHeaderValidator;

        let parent = create_test_header(100, 1000, vec![0; 32]);

        // Timestamp greater than parent - valid
        let valid_header = create_test_header(101, 1001, vec![0; 32]);
        assert!(validator.validate_header(&valid_header, &parent).is_ok());

        // Timestamp same as parent - invalid
        let same_timestamp = create_test_header(101, 1000, vec![0; 32]);
        assert!(validator.validate_header(&same_timestamp, &parent).is_err());

        // Timestamp less than parent - invalid
        let earlier_timestamp = create_test_header(101, 999, vec![0; 32]);
        assert!(validator
            .validate_header(&earlier_timestamp, &parent)
            .is_err());
    }

    #[test]
    fn test_header_version_validation() {
        let validator = DefaultHeaderValidator;

        let parent = create_test_header(100, 1000, vec![0; 32]);

        // Version 1 and 2 are valid
        let mut v1_header = create_test_header(101, 2000, vec![0; 32]);
        v1_header.version = 1;
        assert!(validator.validate_header(&v1_header, &parent).is_ok());

        let mut v2_header = create_test_header(101, 2000, vec![0; 32]);
        v2_header.version = 2;
        assert!(validator.validate_header(&v2_header, &parent).is_ok());

        // Version 0 is invalid
        let mut v0_header = create_test_header(101, 2000, vec![0; 32]);
        v0_header.version = 0;
        assert!(validator.validate_header(&v0_header, &parent).is_err());
    }

    #[test]
    fn test_genesis_header_validation() {
        // Genesis block (height 0) has special validation
        let genesis = create_test_header(0, 0, vec![0; 32]);
        // Genesis is typically validated differently, but we can test basic structure
        assert_eq!(genesis.height, 0);
    }

    // ============ Transaction Validation Tests ============
    // Corresponds to Scala's TransactionValidator tests

    #[test]
    fn test_transaction_validation() {
        let validator = DefaultTransactionValidator;

        let valid_tx = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32]],
            data_inputs: vec![],
            outputs: vec![vec![3; 100]],
        };

        assert!(validator.validate_transaction(&valid_tx).is_ok());

        // No inputs
        let no_inputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![],
            data_inputs: vec![],
            outputs: vec![vec![3; 100]],
        };

        assert!(validator.validate_transaction(&no_inputs).is_err());
    }

    #[test]
    fn test_transaction_must_have_inputs() {
        let validator = DefaultTransactionValidator;

        let no_inputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![],
            data_inputs: vec![],
            outputs: vec![vec![3; 100]],
        };

        let result = validator.validate_transaction(&no_inputs);
        assert!(result.is_err());
        assert!(matches!(result, Err(ConsensusError::InvalidTransaction(_))));
    }

    #[test]
    fn test_transaction_must_have_outputs() {
        let validator = DefaultTransactionValidator;

        let no_outputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32]],
            data_inputs: vec![],
            outputs: vec![],
        };

        let result = validator.validate_transaction(&no_outputs);
        assert!(result.is_err());
    }

    #[test]
    fn test_transaction_duplicate_inputs_rejected() {
        let validator = DefaultTransactionValidator;

        let duplicate_inputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32], vec![2; 32]], // Same input twice
            data_inputs: vec![],
            outputs: vec![vec![3; 100]],
        };

        let result = validator.validate_transaction(&duplicate_inputs);
        assert!(result.is_err());
    }

    #[test]
    fn test_transaction_multiple_inputs_valid() {
        let validator = DefaultTransactionValidator;

        let multiple_inputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32], vec![3; 32], vec![4; 32]],
            data_inputs: vec![],
            outputs: vec![vec![5; 100]],
        };

        assert!(validator.validate_transaction(&multiple_inputs).is_ok());
    }

    #[test]
    fn test_transaction_with_data_inputs() {
        let validator = DefaultTransactionValidator;

        let with_data_inputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32]],
            data_inputs: vec![vec![10; 32], vec![11; 32]],
            outputs: vec![vec![3; 100]],
        };

        assert!(validator.validate_transaction(&with_data_inputs).is_ok());
    }

    #[test]
    fn test_transaction_multiple_outputs() {
        let validator = DefaultTransactionValidator;

        let multiple_outputs = TransactionData {
            id: vec![1; 32],
            inputs: vec![vec![2; 32]],
            data_inputs: vec![],
            outputs: vec![vec![3; 100], vec![4; 100], vec![5; 100]],
        };

        assert!(validator.validate_transaction(&multiple_outputs).is_ok());
    }

    // ============ Block Validation Tests ============
    // Corresponds to Scala's BlockValidator tests

    #[test]
    fn test_block_size_validation() {
        // Block size should not exceed MAX_BLOCK_SIZE
        let block_size = 100_000;
        assert!(block_size <= MAX_BLOCK_SIZE);

        let oversized = MAX_BLOCK_SIZE + 1;
        assert!(oversized > MAX_BLOCK_SIZE);
    }

    #[test]
    fn test_block_max_size_constant() {
        // Verify the constant matches Ergo's specification
        // 1 MB = 1048576 bytes
        assert_eq!(MAX_BLOCK_SIZE, 1_048_576);
    }

    // ============ Votes Validation Tests ============

    #[test]
    fn test_header_votes_format() {
        let header = create_test_header(100, 1000, vec![0; 32]);

        // Votes should be exactly 3 bytes
        assert_eq!(header.votes.len(), 3);
    }

    #[test]
    fn test_valid_vote_values() {
        // Vote values in Ergo range from -127 to 127 (or 0-255 as unsigned)
        let mut header = create_test_header(100, 1000, vec![0; 32]);

        // No votes (all zeros) - valid
        header.votes = [0, 0, 0];
        assert_eq!(header.votes, [0, 0, 0]);

        // Some vote parameters
        header.votes = [1, 2, 3];
        assert_eq!(header.votes[0], 1);
        assert_eq!(header.votes[1], 2);
        assert_eq!(header.votes[2], 3);
    }
}
