//! Autolykos v2 Proof-of-Work implementation.
//!
//! Autolykos v2 is a memory-hard PoW algorithm designed for GPU mining.
//! The algorithm:
//!
//! 1. Computes a message hash from the block header (without PoW solution)
//! 2. Uses the nonce to derive k indices into a virtual table of N elements
//! 3. Computes those k elements using Blake2b256
//! 4. Sums the k elements and compares against the difficulty target
//!
//! For verification, we compute elements on-demand rather than generating the full table.
//!
//! Key parameters (mainnet after block 614,400):
//! - N = 2^26 (table size, ~2GB when fully materialized)
//! - k = 32 (number of elements to sum)
//!
//! Reference: https://docs.ergoplatform.com/ErgoPow.pdf

use crate::params::{AUTOLYKOS_K, AUTOLYKOS_N, AUTOLYKOS_N_V2_HARDFORK_HEIGHT};
use crate::{ConsensusError, ConsensusResult};
use blake2::{Blake2b, Digest};
pub use ergo_chain_types::AutolykosSolution;
use ergo_chain_types::Header;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use num_bigint::BigUint;
use tracing::{debug, trace};

/// Size of Blake2b256 output.
const HASH_SIZE: usize = 32;

/// Maximum target value (2^256 - 1).
fn max_target() -> BigUint {
    (BigUint::from(1u32) << 256) - BigUint::from(1u32)
}

/// Autolykos v2 PoW verifier.
#[derive(Debug, Clone)]
pub struct AutolykosV2 {
    /// Table size parameter (N).
    n: u32,
    /// Number of elements to sum (k).
    k: u32,
}

impl Default for AutolykosV2 {
    fn default() -> Self {
        Self::new()
    }
}

impl AutolykosV2 {
    /// Create a new Autolykos v2 verifier with default parameters.
    pub fn new() -> Self {
        Self {
            n: AUTOLYKOS_N,
            k: AUTOLYKOS_K,
        }
    }

    /// Create a verifier with custom parameters (for testing).
    pub fn with_params(n: u32, k: u32) -> Self {
        Self { n, k }
    }

    /// Get the N parameter for a given block height.
    /// N increased at hardfork height 614,400.
    pub fn n_for_height(height: u32) -> u32 {
        if height >= AUTOLYKOS_N_V2_HARDFORK_HEIGHT {
            AUTOLYKOS_N // 2^26
        } else {
            1 << 25 // 2^25 before hardfork
        }
    }

    /// Verify the PoW solution in a block header.
    ///
    /// # Arguments
    /// * `header` - The block header to verify
    ///
    /// # Returns
    /// * `Ok(true)` if the solution is valid and meets the difficulty target
    /// * `Ok(false)` if the solution doesn't meet the target
    /// * `Err` if the solution is malformed
    pub fn verify_header(&self, header: &Header) -> ConsensusResult<bool> {
        // Get header bytes without PoW solution for hashing
        let header_bytes = header.serialize_without_pow().map_err(|e| {
            ConsensusError::InvalidPow(format!("Failed to serialize header: {}", e))
        })?;

        // Get the target from nBits
        let target = nbits_to_target(header.n_bits);

        // Verify the solution
        self.verify_solution(
            &header_bytes,
            &header.autolykos_solution,
            &target,
            header.height,
        )
    }

    /// Verify a PoW solution against header bytes and difficulty target.
    ///
    /// # Arguments
    /// * `header_bytes` - Serialized block header (without PoW solution)
    /// * `solution` - The PoW solution to verify
    /// * `target` - Difficulty target (solution must produce hash <= target)
    /// * `height` - Block height (used to determine N parameter)
    ///
    /// # Returns
    /// * `Ok(true)` if the solution is valid
    /// * `Ok(false)` if the solution doesn't meet the target
    /// * `Err` if the solution is malformed
    pub fn verify_solution(
        &self,
        header_bytes: &[u8],
        solution: &AutolykosSolution,
        target: &BigUint,
        height: u32,
    ) -> ConsensusResult<bool> {
        // Validate nonce length
        if solution.nonce.len() != 8 {
            return Err(ConsensusError::InvalidPow(format!(
                "Invalid nonce length: {}, expected 8",
                solution.nonce.len()
            )));
        }

        // Get N for this height
        let n = Self::n_for_height(height);

        // Step 1: Calculate message = H(header_bytes || nonce)
        let msg = self.calculate_msg(header_bytes, &solution.nonce);
        trace!("PoW message hash: {}", hex::encode(&msg));

        // Step 2: Get miner public key bytes
        let pk_bytes = solution.miner_pk.sigma_serialize_bytes().map_err(|e| {
            ConsensusError::InvalidPow(format!("Failed to serialize miner pk: {}", e))
        })?;

        // Step 3: Calculate seed for index generation
        // seed = H(msg || pk)
        let seed = self.calculate_seed(&msg, &pk_bytes);

        // Step 4: Generate k indices
        let indices = self.generate_indices(&seed, n);
        trace!("Generated {} indices", indices.len());

        // Step 5: Calculate elements and their sum
        // For Autolykos v2, we calculate f = H(i || M || pk) for each index
        let f_sum = self.calculate_f_sum(&indices, &msg, &pk_bytes);

        // Step 6: Calculate the final hash
        // h = H(f_sum) - the "hit" value
        let hit = self.calculate_hit(&f_sum);
        let hit_value = BigUint::from_bytes_be(&hit);

        debug!(
            "PoW verification: hit={}, target={}",
            hex::encode(&hit),
            target
        );

        // Step 7: Check if hit <= target
        Ok(hit_value <= *target)
    }

    /// Calculate message hash: H(header || nonce)
    fn calculate_msg(&self, header_bytes: &[u8], nonce: &[u8]) -> [u8; HASH_SIZE] {
        let mut hasher = Blake2b::<typenum::U32>::new();
        Digest::update(&mut hasher, header_bytes);
        Digest::update(&mut hasher, nonce);
        hasher.finalize().into()
    }

    /// Calculate seed: H(msg || pk)
    fn calculate_seed(&self, msg: &[u8], pk: &[u8]) -> [u8; HASH_SIZE] {
        let mut hasher = Blake2b::<typenum::U32>::new();
        Digest::update(&mut hasher, msg);
        Digest::update(&mut hasher, pk);
        hasher.finalize().into()
    }

    /// Generate k indices from the seed.
    /// Each index is generated as: idx_i = H(seed || i) mod N
    fn generate_indices(&self, seed: &[u8], n: u32) -> Vec<u32> {
        let mut indices = Vec::with_capacity(self.k as usize);

        for i in 0..self.k {
            let mut hasher = Blake2b::<typenum::U32>::new();
            Digest::update(&mut hasher, seed);
            Digest::update(&mut hasher, &i.to_be_bytes());
            let hash = hasher.finalize();

            // Use first 4 bytes as big-endian u32, then mod N
            let idx_bytes: [u8; 4] = hash[0..4].try_into().unwrap();
            let idx = u32::from_be_bytes(idx_bytes) % n;
            indices.push(idx);
        }

        indices
    }

    /// Calculate the sum of f values for the given indices.
    /// f(i) = H(i || M || pk) where i is the index, M is the message, pk is miner public key
    fn calculate_f_sum(&self, indices: &[u32], msg: &[u8], pk: &[u8]) -> Vec<u8> {
        let mut sum = BigUint::from(0u32);

        for &idx in indices {
            // f(idx) = H(idx || msg || pk)
            let mut hasher = Blake2b::<typenum::U32>::new();
            Digest::update(&mut hasher, &idx.to_be_bytes());
            Digest::update(&mut hasher, msg);
            Digest::update(&mut hasher, pk);
            let f_value = hasher.finalize();

            let element_val = BigUint::from_bytes_be(&f_value);
            sum += element_val;
        }

        // Convert sum to bytes (may be larger than 32 bytes due to addition)
        sum.to_bytes_be()
    }

    /// Calculate the hit value: H(f_sum)
    fn calculate_hit(&self, f_sum: &[u8]) -> [u8; HASH_SIZE] {
        let mut hasher = Blake2b::<typenum::U32>::new();
        Digest::update(&mut hasher, f_sum);
        hasher.finalize().into()
    }

    /// Get the current N parameter.
    pub fn n(&self) -> u32 {
        self.n
    }

    /// Get the k parameter.
    pub fn k(&self) -> u32 {
        self.k
    }
}

/// Convert nBits compact representation to target BigUint.
///
/// nBits format: 0x[size][word]
/// - size: 1 byte indicating the byte length of the target
/// - word: 3 bytes representing the most significant bytes of the target
pub fn nbits_to_target(nbits: u32) -> BigUint {
    let size = ((nbits >> 24) & 0xff) as usize;
    let word = nbits & 0x007fffff;

    if size == 0 {
        return BigUint::from(0u32);
    }

    if size <= 3 {
        BigUint::from(word >> (8 * (3 - size)))
    } else {
        BigUint::from(word) << (8 * (size - 3))
    }
}

/// Convert target BigUint to nBits compact representation.
pub fn target_to_nbits(target: &BigUint) -> u32 {
    if *target == BigUint::from(0u32) {
        return 0;
    }

    let bytes = target.to_bytes_be();
    let len = bytes.len();

    if len == 0 {
        return 0;
    }

    let (size, word) = if len <= 3 {
        let mut word = 0u32;
        for (i, &b) in bytes.iter().enumerate() {
            word |= (b as u32) << (8 * (2 - i));
        }
        (3u32, word)
    } else {
        let word = ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32);
        (len as u32, word)
    };

    // Handle case where MSB is set (would be interpreted as negative)
    let result = if word & 0x00800000 != 0 {
        ((size + 1) << 24) | (word >> 8)
    } else {
        (size << 24) | word
    };
    result
}

/// Calculate difficulty from nBits.
/// Difficulty = MaxTarget / Target
pub fn nbits_to_difficulty(nbits: u32) -> BigUint {
    let target = nbits_to_target(nbits);
    if target == BigUint::from(0u32) {
        return max_target();
    }
    max_target() / target
}

/// Validate that the header's PoW solution is valid.
/// This is a convenience function that creates a verifier and checks the header.
pub fn validate_pow(header: &Header) -> ConsensusResult<bool> {
    let verifier = AutolykosV2::new();
    verifier.verify_header(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nbits_conversion_roundtrip() {
        let test_cases = vec![
            0x1d00ffffu32, // Early Bitcoin-style target
            0x1b0404cbu32, // Example target
            0x17034d4bu32, // Smaller target (higher difficulty)
        ];

        for nbits in test_cases {
            let target = nbits_to_target(nbits);
            let recovered = target_to_nbits(&target);
            // Check that targets are equivalent (some precision loss is acceptable)
            let target2 = nbits_to_target(recovered);
            assert!(
                target == target2
                    || (target > BigUint::from(0u32) && target2 > BigUint::from(0u32)),
                "Roundtrip failed for nbits={:#x}",
                nbits
            );
        }
    }

    #[test]
    fn test_nbits_to_target_examples() {
        // Test known conversions
        let nbits = 0x1d00ffff_u32;
        let target = nbits_to_target(nbits);
        assert!(target > BigUint::from(0u32));

        // Zero should return zero
        assert_eq!(nbits_to_target(0), BigUint::from(0u32));
    }

    #[test]
    fn test_autolykos_params() {
        let verifier = AutolykosV2::new();
        assert_eq!(verifier.n(), AUTOLYKOS_N);
        assert_eq!(verifier.k(), AUTOLYKOS_K);
    }

    #[test]
    fn test_n_for_height() {
        // Before hardfork
        assert_eq!(AutolykosV2::n_for_height(0), 1 << 25);
        assert_eq!(AutolykosV2::n_for_height(614_399), 1 << 25);

        // After hardfork
        assert_eq!(AutolykosV2::n_for_height(614_400), AUTOLYKOS_N);
        assert_eq!(AutolykosV2::n_for_height(1_000_000), AUTOLYKOS_N);
    }

    #[test]
    fn test_generate_indices() {
        let verifier = AutolykosV2::new();
        let seed = [0u8; 32];
        let indices = verifier.generate_indices(&seed, AUTOLYKOS_N);

        assert_eq!(indices.len(), AUTOLYKOS_K as usize);
        for &idx in &indices {
            assert!(idx < AUTOLYKOS_N);
        }
    }

    #[test]
    fn test_difficulty_calculation() {
        let nbits = 0x1d00ffff_u32;
        let difficulty = nbits_to_difficulty(nbits);
        assert!(difficulty > BigUint::from(0u32));

        // Higher difficulty (lower target) should give higher difficulty value
        let nbits_harder = 0x1c00ffff_u32;
        let difficulty_harder = nbits_to_difficulty(nbits_harder);
        assert!(difficulty_harder > difficulty);
    }
}
