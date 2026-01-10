//! Miner implementation.
//!
//! This module provides the `Miner` which coordinates both internal CPU mining
//! and external miner support. For internal mining, it manages a pool of worker
//! threads that search for valid PoW solutions.

use crate::worker::{FoundSolution, MiningTask, WorkerPool};
use crate::{BlockCandidate, CandidateGenerator, MiningError, MiningResult};
use ergo_consensus::{nbits_to_target, AutolykosSolution, AutolykosV2};
use ergo_lib::ergo_chain_types::EcPoint;
use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Candidate refresh interval in seconds.
const CANDIDATE_REFRESH_SECS: u64 = 30;

/// Miner configuration.
#[derive(Debug, Clone)]
pub struct MinerConfig {
    /// Enable internal CPU mining.
    pub internal_mining: bool,
    /// Use external miner (stratum protocol).
    pub external_mining: bool,
    /// Reward address.
    pub reward_address: String,
    /// Number of mining threads (0 = auto-detect based on CPU cores).
    pub threads: usize,
    /// Network prefix for address parsing.
    pub network: NetworkPrefix,
}

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            internal_mining: false,
            external_mining: true,
            reward_address: String::new(),
            threads: 0, // Auto-detect
            network: NetworkPrefix::Mainnet,
        }
    }
}

impl MinerConfig {
    /// Get the effective number of mining threads.
    pub fn effective_threads(&self) -> usize {
        if self.threads == 0 {
            // Auto-detect: use number of CPUs, but at least 1
            num_cpus::get().max(1)
        } else {
            self.threads
        }
    }
}

/// Mining statistics.
#[derive(Debug, Clone, Default)]
pub struct MiningStats {
    /// Candidates generated.
    pub candidates_generated: u64,
    /// Solutions received.
    pub solutions_received: u64,
    /// Valid solutions.
    pub valid_solutions: u64,
    /// Invalid solutions.
    pub invalid_solutions: u64,
    /// Blocks mined.
    pub blocks_mined: u64,
    /// Hash rate (hashes per second).
    pub hash_rate: f64,
    /// Total hashes computed.
    pub total_hashes: u64,
}

/// Block miner.
pub struct Miner {
    /// Configuration.
    config: MinerConfig,
    /// Candidate generator.
    candidate_gen: Arc<CandidateGenerator>,
    /// Autolykos verifier.
    autolykos: AutolykosV2,
    /// Mining enabled flag.
    enabled: AtomicBool,
    /// Internal mining running flag.
    internal_running: AtomicBool,
    /// Current candidate.
    current_candidate: RwLock<Option<BlockCandidate>>,
    /// Mining statistics.
    stats: RwLock<MiningStats>,
    /// Total hash count for rate calculation.
    hash_count: AtomicU64,
    /// Last hash rate calculation time.
    last_rate_calc: RwLock<Instant>,
}

impl Miner {
    /// Create a new miner.
    pub fn new(config: MinerConfig, candidate_gen: Arc<CandidateGenerator>) -> Self {
        if !config.reward_address.is_empty() {
            candidate_gen.set_reward_address(config.reward_address.clone());
        }

        Self {
            config,
            candidate_gen,
            autolykos: AutolykosV2::new(),
            enabled: AtomicBool::new(false),
            internal_running: AtomicBool::new(false),
            current_candidate: RwLock::new(None),
            stats: RwLock::new(MiningStats::default()),
            hash_count: AtomicU64::new(0),
            last_rate_calc: RwLock::new(Instant::now()),
        }
    }

    /// Enable mining.
    pub fn start(&self) {
        self.enabled.store(true, Ordering::SeqCst);
        info!("Mining enabled");
    }

    /// Disable mining.
    pub fn stop(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.internal_running.store(false, Ordering::SeqCst);
        info!("Mining disabled");
    }

    /// Check if mining is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Check if internal mining is running.
    pub fn is_internal_running(&self) -> bool {
        self.internal_running.load(Ordering::SeqCst)
    }

    /// Get the miner configuration.
    pub fn config(&self) -> &MinerConfig {
        &self.config
    }

    /// Get current mining candidate.
    pub fn get_candidate(&self) -> MiningResult<BlockCandidate> {
        if !self.is_enabled() {
            return Err(MiningError::CandidateFailed(
                "Mining not enabled".to_string(),
            ));
        }

        let candidate = self.candidate_gen.get_or_generate()?;
        *self.current_candidate.write() = Some(candidate.clone());

        self.stats.write().candidates_generated += 1;

        Ok(candidate)
    }

    /// Submit a mining solution.
    pub fn submit_solution(&self, solution: AutolykosSolution) -> MiningResult<bool> {
        if !self.is_enabled() {
            return Err(MiningError::CandidateFailed(
                "Mining not enabled".to_string(),
            ));
        }

        self.stats.write().solutions_received += 1;

        let candidate = self
            .current_candidate
            .read()
            .clone()
            .ok_or_else(|| MiningError::InvalidSolution("No current candidate".to_string()))?;

        // Get difficulty target
        let target = nbits_to_target(candidate.n_bits);

        // Verify solution
        let valid = self.autolykos.verify_solution(
            &candidate.header_bytes,
            &solution,
            &target,
            candidate.height,
        )?;

        if valid {
            info!(height = candidate.height, "Valid solution found!");
            self.stats.write().valid_solutions += 1;

            // Would broadcast block here
            self.stats.write().blocks_mined += 1;

            // Invalidate candidate
            self.candidate_gen.invalidate();
            *self.current_candidate.write() = None;

            Ok(true)
        } else {
            debug!("Invalid solution submitted");
            self.stats.write().invalid_solutions += 1;
            Ok(false)
        }
    }

    /// Get mining statistics.
    pub fn stats(&self) -> MiningStats {
        self.stats.read().clone()
    }

    /// Get reward address.
    pub fn reward_address(&self) -> Option<String> {
        self.candidate_gen.reward_address()
    }

    /// Set reward address.
    pub fn set_reward_address(&self, address: String) {
        self.candidate_gen.set_reward_address(address);
    }

    /// Notify of new block (invalidates current work).
    ///
    /// This clears the current candidate, causing the mining loop to
    /// regenerate a new candidate on the next iteration.
    pub fn on_new_block(&self, height: u32) {
        self.candidate_gen.invalidate();
        *self.current_candidate.write() = None;
        debug!(height = height, "Mining work invalidated due to new block");
    }

    /// Update hash rate statistics.
    ///
    /// This tracks total hashes and calculates a rolling hash rate.
    /// The `new_hashes` parameter is the number of hashes computed since the last call.
    fn update_hash_rate(&self, new_hashes: u64) {
        // Update total hash count
        let total = self.hash_count.fetch_add(new_hashes, Ordering::Relaxed) + new_hashes;

        // Calculate rate based on elapsed time since last calculation
        let mut last_calc = self.last_rate_calc.write();
        let elapsed = last_calc.elapsed();

        // Update stats atomically
        let mut stats = self.stats.write();
        stats.total_hashes = total;

        // Only update rate if enough time has passed (avoid division by tiny numbers)
        if elapsed >= Duration::from_millis(100) {
            // Calculate rate: hashes in this period / time elapsed
            let rate = new_hashes as f64 / elapsed.as_secs_f64();
            stats.hash_rate = rate;
            *last_calc = Instant::now();
        }
    }

    /// Get the miner's public key from the reward address.
    fn get_miner_pk(&self) -> MiningResult<EcPoint> {
        let address_str = self
            .reward_address()
            .ok_or_else(|| MiningError::NoRewardAddress)?;

        let encoder = AddressEncoder::new(self.config.network);
        let address = encoder
            .parse_address_from_str(&address_str)
            .map_err(|e| MiningError::CandidateFailed(format!("Invalid reward address: {}", e)))?;

        // Extract public key from address
        match address {
            Address::P2Pk(pk) => Ok((*pk.h).clone()),
            _ => Err(MiningError::CandidateFailed(
                "Reward address must be a P2PK address for mining".to_string(),
            )),
        }
    }

    /// Start internal CPU mining.
    ///
    /// This spawns worker threads that continuously search for valid solutions.
    /// Call `stop()` to stop mining.
    pub async fn start_internal_mining(self: Arc<Self>) -> MiningResult<()> {
        if !self.config.internal_mining {
            return Err(MiningError::CandidateFailed(
                "Internal mining not enabled in config".to_string(),
            ));
        }

        if self.internal_running.swap(true, Ordering::SeqCst) {
            return Err(MiningError::CandidateFailed(
                "Internal mining already running".to_string(),
            ));
        }

        // Ensure we have a reward address
        let miner_pk = self.get_miner_pk()?;

        let num_threads = self.config.effective_threads();
        info!(
            threads = num_threads,
            address = ?self.reward_address(),
            "Starting internal CPU mining"
        );

        // Create worker pool
        let mut pool = WorkerPool::new(num_threads);

        // Mining loop
        while self.is_enabled() && self.internal_running.load(Ordering::SeqCst) {
            // Generate a new candidate
            let candidate = match self.get_candidate() {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to generate candidate: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            info!(
                height = candidate.height,
                n_bits = format!("{:#x}", candidate.n_bits),
                "Mining on new candidate"
            );

            // Create mining task
            let task = MiningTask {
                header_bytes: candidate.header_bytes.clone(),
                n_bits: candidate.n_bits,
                height: candidate.height,
                miner_pk: Box::new(miner_pk.clone()),
                created_at: candidate.timestamp,
            };

            // Broadcast to workers
            pool.broadcast_task(task);

            // Wait for solution, new block, or timeout
            let candidate_timeout = Duration::from_secs(CANDIDATE_REFRESH_SECS);
            let deadline = Instant::now() + candidate_timeout;

            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    debug!("Candidate expired, regenerating");
                    break;
                }

                tokio::select! {
                    // Check for solutions
                    solution = pool.recv_solution() => {
                        if let Some(found) = solution {
                            // Update statistics
                            self.update_hash_rate(pool.reset_hash_counts());

                            // Verify and submit
                            if self.handle_found_solution(found, &candidate).await {
                                // Solution accepted, generate new candidate
                                break;
                            }
                        }
                    }

                    // Periodic hash rate update and check for stale work
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        let hashes = pool.reset_hash_counts();
                        self.update_hash_rate(hashes);
                        debug!(
                            hash_rate = self.stats().hash_rate,
                            total_hashes = self.stats().total_hashes,
                            "Mining progress"
                        );

                        // Check if our candidate is still valid (new block may have arrived)
                        if self.current_candidate.read().is_none() {
                            debug!("Candidate invalidated by new block, regenerating");
                            break;
                        }
                    }
                }

                // Check if we should stop
                if !self.is_enabled() || !self.internal_running.load(Ordering::SeqCst) {
                    break;
                }
            }
        }

        // Cleanup
        pool.shutdown();
        self.internal_running.store(false, Ordering::SeqCst);
        info!("Internal mining stopped");

        Ok(())
    }

    /// Handle a found solution from a worker.
    async fn handle_found_solution(
        &self,
        found: FoundSolution,
        candidate: &BlockCandidate,
    ) -> bool {
        info!(
            worker_id = found.worker_id,
            height = found.task.height,
            hashes = found.hashes_computed,
            "Worker found potential solution"
        );

        // Verify the solution matches the current candidate
        if found.task.height != candidate.height {
            debug!(
                found_height = found.task.height,
                expected_height = candidate.height,
                "Solution is for stale candidate"
            );
            return false;
        }

        // Submit the solution
        match self.submit_solution(found.solution) {
            Ok(valid) => {
                if valid {
                    info!(height = candidate.height, "Block mined successfully!");
                    true
                } else {
                    warn!("Solution did not meet difficulty target");
                    false
                }
            }
            Err(e) => {
                error!("Failed to submit solution: {}", e);
                false
            }
        }
    }

    /// Stop internal mining.
    pub fn stop_internal_mining(&self) {
        self.internal_running.store(false, Ordering::SeqCst);
        debug!("Internal mining stop requested");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ergo_mempool::Mempool;
    use ergo_state::StateManager;
    use ergo_storage::Database;
    use tempfile::TempDir;

    fn create_test_miner() -> (Miner, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        let state = Arc::new(StateManager::new(Arc::new(db)));
        let mempool = Arc::new(Mempool::with_defaults());
        let candidate_gen = Arc::new(CandidateGenerator::new(state, mempool));

        let config = MinerConfig {
            reward_address: "test_address".to_string(),
            ..Default::default()
        };

        let miner = Miner::new(config, candidate_gen);
        (miner, tmp)
    }

    #[test]
    fn test_miner_enable_disable() {
        let (miner, _tmp) = create_test_miner();

        assert!(!miner.is_enabled());

        miner.start();
        assert!(miner.is_enabled());

        miner.stop();
        assert!(!miner.is_enabled());
    }

    #[test]
    fn test_miner_config_threads() {
        let config = MinerConfig {
            threads: 0,
            ..Default::default()
        };

        let effective = config.effective_threads();
        assert!(effective >= 1, "Should have at least 1 thread");

        let config2 = MinerConfig {
            threads: 4,
            ..Default::default()
        };
        assert_eq!(config2.effective_threads(), 4);
    }

    #[test]
    fn test_miner_stats() {
        let (miner, _tmp) = create_test_miner();

        let stats = miner.stats();
        assert_eq!(stats.candidates_generated, 0);
        assert_eq!(stats.blocks_mined, 0);
    }

    #[test]
    fn test_miner_on_new_block() {
        let (miner, _tmp) = create_test_miner();
        miner.start();

        // Simulate receiving a new block
        miner.on_new_block(100);

        // Candidate should be invalidated
        assert!(miner.current_candidate.read().is_none());
    }
}
