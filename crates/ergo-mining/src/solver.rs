//! Autolykos v2 PoW solver for CPU mining.
//!
//! This module implements the Autolykos v2 mining algorithm, which is the inverse
//! of the verification algorithm. For each nonce attempt, it:
//!
//! 1. Computes msg = H(header_bytes || nonce)
//! 2. Computes seed = H(msg || pk)
//! 3. Generates k=32 indices from the seed
//! 4. Calculates elements f(i) = H(i || msg || pk) for each index
//! 5. Sums the elements and checks if H(sum) <= target
//!
//! Note: CPU mining is computationally expensive (~67 Blake2b operations per nonce).
//! This is intended for testnet, development, and small-scale mining.

use blake2::{Blake2b, Digest};
use ergo_chain_types::AutolykosSolution;
use ergo_consensus::{nbits_to_target, AutolykosV2};
use ergo_lib::ergo_chain_types::EcPoint;
use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
use num_bigint::BigUint;
use rand::Rng;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{debug, trace};

/// Size of Blake2b256 output.
const HASH_SIZE: usize = 32;

/// Default batch size for mining attempts.
const DEFAULT_BATCH_SIZE: u64 = 10_000;

/// Autolykos v2 PoW solver for CPU mining.
#[derive(Debug, Clone)]
pub struct AutolykosSolver {
    /// Number of elements to sum (k=32).
    k: u32,
    /// Verifier for validating solutions.
    verifier: AutolykosV2,
}

impl Default for AutolykosSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl AutolykosSolver {
    /// Create a new solver with default parameters.
    pub fn new() -> Self {
        Self {
            k: 32,
            verifier: AutolykosV2::new(),
        }
    }

    /// Try to find a valid PoW solution.
    ///
    /// # Arguments
    /// * `header_bytes` - Serialized block header (without PoW solution)
    /// * `n_bits` - Compact difficulty target
    /// * `height` - Block height (determines N parameter)
    /// * `miner_pk` - Miner's public key
    /// * `max_attempts` - Maximum nonce attempts before giving up
    /// * `cancel` - Atomic flag to cancel mining
    /// * `hash_counter` - Counter for hash rate tracking
    ///
    /// # Returns
    /// * `Some(AutolykosSolution)` if a valid solution is found
    /// * `None` if cancelled or max attempts reached
    pub fn try_solve(
        &self,
        header_bytes: &[u8],
        n_bits: u32,
        height: u32,
        miner_pk: &EcPoint,
        max_attempts: u64,
        cancel: &AtomicBool,
        hash_counter: &AtomicU64,
    ) -> Option<AutolykosSolution> {
        let target = nbits_to_target(n_bits);
        let n = AutolykosV2::n_for_height(height);

        // Serialize miner public key once
        let pk_bytes = match miner_pk.sigma_serialize_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!("Failed to serialize miner pk: {}", e);
                return None;
            }
        };

        // Generate random starting nonce
        let start_nonce: u64 = rand::thread_rng().gen();

        for attempt in 0..max_attempts {
            // Check for cancellation periodically
            if attempt % 1000 == 0 && cancel.load(Ordering::Relaxed) {
                trace!("Mining cancelled after {} attempts", attempt);
                return None;
            }

            let nonce = start_nonce.wrapping_add(attempt);
            let nonce_bytes = nonce.to_be_bytes();

            // Try this nonce
            if let Some(solution) =
                self.try_nonce(header_bytes, &nonce_bytes, &target, n, &pk_bytes, miner_pk)
            {
                hash_counter.fetch_add(attempt + 1, Ordering::Relaxed);
                debug!(
                    "Found valid solution after {} attempts, nonce={}",
                    attempt + 1,
                    nonce
                );
                return Some(solution);
            }
        }

        hash_counter.fetch_add(max_attempts, Ordering::Relaxed);
        None
    }

    /// Try to find a solution in batches, yielding between batches.
    ///
    /// This is useful for async contexts where we want to periodically
    /// check for new work or cancellation.
    pub fn try_solve_batch(
        &self,
        header_bytes: &[u8],
        n_bits: u32,
        height: u32,
        miner_pk: &EcPoint,
        start_nonce: u64,
        batch_size: u64,
        hash_counter: &AtomicU64,
    ) -> Option<AutolykosSolution> {
        let target = nbits_to_target(n_bits);
        let n = AutolykosV2::n_for_height(height);

        // Serialize miner public key once
        let pk_bytes = match miner_pk.sigma_serialize_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!("Failed to serialize miner pk: {}", e);
                return None;
            }
        };

        for offset in 0..batch_size {
            let nonce = start_nonce.wrapping_add(offset);
            let nonce_bytes = nonce.to_be_bytes();

            if let Some(solution) =
                self.try_nonce(header_bytes, &nonce_bytes, &target, n, &pk_bytes, miner_pk)
            {
                hash_counter.fetch_add(offset + 1, Ordering::Relaxed);
                return Some(solution);
            }
        }

        hash_counter.fetch_add(batch_size, Ordering::Relaxed);
        None
    }

    /// Try a single nonce value.
    ///
    /// Returns `Some(AutolykosSolution)` if the nonce produces a valid PoW.
    fn try_nonce(
        &self,
        header_bytes: &[u8],
        nonce: &[u8; 8],
        target: &BigUint,
        n: u32,
        pk_bytes: &[u8],
        miner_pk: &EcPoint,
    ) -> Option<AutolykosSolution> {
        // Step 1: Calculate message = H(header_bytes || nonce)
        let msg = self.calculate_msg(header_bytes, nonce);

        // Step 2: Calculate seed = H(msg || pk)
        let seed = self.calculate_seed(&msg, pk_bytes);

        // Step 3: Generate k indices
        let indices = self.generate_indices(&seed, n);

        // Step 4: Calculate sum of f values
        let f_sum = self.calculate_f_sum(&indices, &msg, pk_bytes);

        // Step 5: Calculate hit = H(f_sum)
        let hit = self.calculate_hit(&f_sum);
        let hit_value = BigUint::from_bytes_be(&hit);

        // Step 6: Check if hit <= target
        if hit_value <= *target {
            Some(AutolykosSolution {
                miner_pk: Box::new(miner_pk.clone()),
                pow_onetime_pk: None,
                nonce: nonce.to_vec(),
                pow_distance: None,
            })
        } else {
            None
        }
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
    fn generate_indices(&self, seed: &[u8], n: u32) -> Vec<u32> {
        let mut indices = Vec::with_capacity(self.k as usize);

        for i in 0..self.k {
            let mut hasher = Blake2b::<typenum::U32>::new();
            Digest::update(&mut hasher, seed);
            Digest::update(&mut hasher, &i.to_be_bytes());
            let hash = hasher.finalize();

            let idx_bytes: [u8; 4] = hash[0..4].try_into().unwrap();
            let idx = u32::from_be_bytes(idx_bytes) % n;
            indices.push(idx);
        }

        indices
    }

    /// Calculate the sum of f values for the given indices.
    ///
    /// Optimized to minimize BigUint allocations by reusing the hasher
    /// and accumulating directly into the sum.
    fn calculate_f_sum(&self, indices: &[u32], msg: &[u8], pk: &[u8]) -> Vec<u8> {
        // Pre-allocate sum with capacity for k * 256-bit values
        // Maximum sum is 32 * 2^256, which fits in ~261 bits (33 bytes)
        let mut sum = BigUint::from(0u32);

        // Reuse a single hash buffer to avoid repeated allocations
        let mut hash_buf = [0u8; HASH_SIZE];

        for &idx in indices {
            let mut hasher = Blake2b::<typenum::U32>::new();
            Digest::update(&mut hasher, &idx.to_be_bytes());
            Digest::update(&mut hasher, msg);
            Digest::update(&mut hasher, pk);
            hash_buf.copy_from_slice(&hasher.finalize());

            // Add directly to sum - BigUint handles the arithmetic efficiently
            sum += BigUint::from_bytes_be(&hash_buf);
        }

        sum.to_bytes_be()
    }

    /// Calculate the hit value: H(f_sum)
    fn calculate_hit(&self, f_sum: &[u8]) -> [u8; HASH_SIZE] {
        let mut hasher = Blake2b::<typenum::U32>::new();
        Digest::update(&mut hasher, f_sum);
        hasher.finalize().into()
    }

    /// Verify a solution using the internal verifier.
    pub fn verify(
        &self,
        header_bytes: &[u8],
        solution: &AutolykosSolution,
        n_bits: u32,
        height: u32,
    ) -> bool {
        let target = nbits_to_target(n_bits);
        self.verifier
            .verify_solution(header_bytes, solution, &target, height)
            .unwrap_or(false)
    }

    /// Get the default batch size for mining.
    pub fn default_batch_size() -> u64 {
        DEFAULT_BATCH_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_miner_pk() -> EcPoint {
        // Use identity point for testing
        ergo_chain_types::ec_point::identity()
    }

    fn create_test_header_bytes() -> Vec<u8> {
        // Create deterministic test header bytes
        vec![0u8; 204]
    }

    #[test]
    fn test_solver_creation() {
        let solver = AutolykosSolver::new();
        assert_eq!(solver.k, 32);
    }

    #[test]
    fn test_hash_functions() {
        let solver = AutolykosSolver::new();

        let msg = solver.calculate_msg(&[1, 2, 3], &[4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(msg.len(), 32);

        let seed = solver.calculate_seed(&msg, &[1, 2, 3]);
        assert_eq!(seed.len(), 32);

        let indices = solver.generate_indices(&seed, 1 << 26);
        assert_eq!(indices.len(), 32);
        for &idx in &indices {
            assert!(idx < (1 << 26));
        }
    }

    #[test]
    fn test_solver_with_easy_target() {
        let solver = AutolykosSolver::new();
        let header_bytes = create_test_header_bytes();
        let miner_pk = create_test_miner_pk();

        // Use a very easy difficulty (high target) so we find a solution quickly
        // 0x2100ffff gives a very large target
        let easy_nbits = 0x2100ffff_u32;
        let height = 100_000;

        let cancel = AtomicBool::new(false);
        let hash_counter = AtomicU64::new(0);

        let result = solver.try_solve(
            &header_bytes,
            easy_nbits,
            height,
            &miner_pk,
            100_000, // Should find solution quickly with easy target
            &cancel,
            &hash_counter,
        );

        assert!(result.is_some(), "Should find solution with easy target");

        let solution = result.unwrap();
        assert_eq!(solution.nonce.len(), 8);

        // Verify the solution
        assert!(
            solver.verify(&header_bytes, &solution, easy_nbits, height),
            "Solution should verify"
        );
    }

    #[test]
    fn test_solver_cancellation() {
        let solver = AutolykosSolver::new();
        let header_bytes = create_test_header_bytes();
        let miner_pk = create_test_miner_pk();

        // Use a hard difficulty so we don't find solution immediately
        let hard_nbits = 0x17034d4b_u32; // Very hard
        let height = 100_000;

        // Set cancel immediately
        let cancel = AtomicBool::new(true);
        let hash_counter = AtomicU64::new(0);

        let result = solver.try_solve(
            &header_bytes,
            hard_nbits,
            height,
            &miner_pk,
            1_000_000,
            &cancel,
            &hash_counter,
        );

        // Should return None due to cancellation
        assert!(result.is_none());
    }

    #[test]
    fn test_batch_mining() {
        let solver = AutolykosSolver::new();
        let header_bytes = create_test_header_bytes();
        let miner_pk = create_test_miner_pk();

        // Easy target
        let easy_nbits = 0x2100ffff_u32;
        let height = 100_000;
        let hash_counter = AtomicU64::new(0);

        // Try multiple batches
        let mut found = false;
        for batch in 0..100 {
            let start_nonce = batch * 10_000;
            if let Some(solution) = solver.try_solve_batch(
                &header_bytes,
                easy_nbits,
                height,
                &miner_pk,
                start_nonce,
                10_000,
                &hash_counter,
            ) {
                assert!(solver.verify(&header_bytes, &solution, easy_nbits, height));
                found = true;
                break;
            }
        }

        assert!(
            found,
            "Should find solution within 100 batches with easy target"
        );
        assert!(hash_counter.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_n_parameter_for_height() {
        // Before hardfork (height < 614,400)
        assert_eq!(AutolykosV2::n_for_height(0), 1 << 25);
        assert_eq!(AutolykosV2::n_for_height(614_399), 1 << 25);

        // After hardfork (height >= 614,400)
        assert_eq!(AutolykosV2::n_for_height(614_400), 1 << 26);
        assert_eq!(AutolykosV2::n_for_height(1_000_000), 1 << 26);
    }
}
