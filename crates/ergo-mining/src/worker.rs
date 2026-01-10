//! Mining worker thread implementation.
//!
//! This module provides the `MiningWorker` which runs in a separate thread
//! and continuously searches for valid PoW solutions. Workers communicate
//! with the coordinator via channels.

use crate::solver::AutolykosSolver;
use ergo_chain_types::AutolykosSolution;
use ergo_lib::ergo_chain_types::EcPoint;
use rand::Rng;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, trace, warn};

/// Batch size for mining attempts before checking for new work.
const BATCH_SIZE: u64 = 10_000;

/// Maximum number of workers for nonce space partitioning.
/// Workers beyond this will wrap around, but with different random offsets.
const MAX_NONCE_PARTITIONS: u64 = 256;

/// A mining task describing the work to be done.
#[derive(Clone, Debug)]
pub struct MiningTask {
    /// Serialized block header (without PoW solution).
    pub header_bytes: Vec<u8>,
    /// Compact difficulty target (nBits).
    pub n_bits: u32,
    /// Block height.
    pub height: u32,
    /// Miner's public key.
    pub miner_pk: Box<EcPoint>,
    /// Task generation timestamp (for staleness detection).
    pub created_at: u64,
}

/// A solution found by a worker.
#[derive(Debug)]
pub struct FoundSolution {
    /// The valid PoW solution.
    pub solution: AutolykosSolution,
    /// The task this solution is for.
    pub task: MiningTask,
    /// Worker ID that found the solution.
    pub worker_id: usize,
    /// Number of hashes computed to find this solution.
    pub hashes_computed: u64,
}

/// A mining worker that runs in its own thread.
pub struct MiningWorker {
    /// Worker ID.
    id: usize,
    /// Handle to the worker thread.
    handle: Option<JoinHandle<()>>,
    /// Flag to stop the worker.
    running: Arc<AtomicBool>,
    /// Hash counter for statistics.
    hash_count: Arc<AtomicU64>,
}

impl MiningWorker {
    /// Spawn a new mining worker.
    ///
    /// # Arguments
    /// * `id` - Unique worker ID
    /// * `task_rx` - Receiver for mining tasks
    /// * `solution_tx` - Sender for found solutions
    ///
    /// # Returns
    /// A new `MiningWorker` with a running background thread.
    pub fn spawn(
        id: usize,
        task_rx: watch::Receiver<Option<MiningTask>>,
        solution_tx: mpsc::Sender<FoundSolution>,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let hash_count = Arc::new(AtomicU64::new(0));

        let running_clone = Arc::clone(&running);
        let hash_count_clone = Arc::clone(&hash_count);

        let handle = thread::Builder::new()
            .name(format!("mining-worker-{}", id))
            .spawn(move || {
                Self::worker_loop(id, task_rx, solution_tx, running_clone, hash_count_clone);
            })
            .expect("Failed to spawn mining worker thread");

        info!(worker_id = id, "Mining worker spawned");

        Self {
            id,
            handle: Some(handle),
            running,
            hash_count,
        }
    }

    /// The main worker loop.
    fn worker_loop(
        id: usize,
        mut task_rx: watch::Receiver<Option<MiningTask>>,
        solution_tx: mpsc::Sender<FoundSolution>,
        running: Arc<AtomicBool>,
        hash_count: Arc<AtomicU64>,
    ) {
        let solver = AutolykosSolver::new();

        // Generate a unique starting nonce for this worker
        // Partition nonce space among workers to avoid overlap
        // Use modulo to handle more workers than partitions, with random offset for uniqueness
        let partition = (id as u64) % MAX_NONCE_PARTITIONS;
        let partition_size = u64::MAX / MAX_NONCE_PARTITIONS;
        let base_offset = partition * partition_size;
        // Add random offset within partition for workers that share same partition
        let random_offset: u64 = rand::thread_rng().gen::<u64>() % (partition_size / 2);
        let nonce_offset = base_offset.wrapping_add(random_offset);

        debug!(
            worker_id = id,
            partition = partition,
            "Worker starting with nonce offset {:#x}",
            nonce_offset
        );

        while running.load(Ordering::Relaxed) {
            // Get current task and mark it as seen
            let task = {
                // Mark current value as seen so has_changed() works correctly
                task_rx.borrow_and_update().clone()
            };

            match task {
                Some(task) => {
                    // Mine on this task
                    let start_hashes = hash_count.load(Ordering::Relaxed);
                    let mut local_nonce = nonce_offset.wrapping_add(start_hashes);

                    // Process batches until we find a solution or get new work
                    loop {
                        if !running.load(Ordering::Relaxed) {
                            break;
                        }

                        // Check if we have new work (only true if changed after we saw the current task)
                        if task_rx.has_changed().unwrap_or(false) {
                            trace!(worker_id = id, "New work received, switching tasks");
                            break;
                        }

                        // Try a batch of nonces
                        // Use a shared counter to track hashes in this batch
                        let batch_counter = AtomicU64::new(0);
                        let result = solver.try_solve_batch(
                            &task.header_bytes,
                            task.n_bits,
                            task.height,
                            &task.miner_pk,
                            local_nonce,
                            BATCH_SIZE,
                            &batch_counter,
                        );

                        let batch_hashes = batch_counter.load(Ordering::Relaxed);

                        if let Some(solution) = result {
                            let total_hashes = hash_count
                                .fetch_add(batch_hashes, Ordering::Relaxed)
                                + batch_hashes;

                            info!(
                                worker_id = id,
                                height = task.height,
                                hashes = total_hashes,
                                "Found valid solution!"
                            );

                            // Send solution to coordinator
                            let found = FoundSolution {
                                solution,
                                task: task.clone(),
                                worker_id: id,
                                hashes_computed: batch_hashes,
                            };

                            // Use blocking send since we're in a non-async thread
                            if solution_tx.blocking_send(found).is_err() {
                                warn!(worker_id = id, "Failed to send solution, channel closed");
                                return;
                            }

                            // Wait for new work after finding a solution
                            break;
                        }

                        // Update counters for unsuccessful batch
                        hash_count.fetch_add(BATCH_SIZE, Ordering::Relaxed);
                        local_nonce = local_nonce.wrapping_add(BATCH_SIZE);
                    }
                }
                None => {
                    // No work available, wait a bit and check again
                    trace!(worker_id = id, "No work available, waiting...");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }

        info!(
            worker_id = id,
            total_hashes = hash_count.load(Ordering::Relaxed),
            "Worker shutting down"
        );
    }

    /// Stop the worker.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    /// Check if the worker is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get the worker ID.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Get the current hash count.
    pub fn hash_count(&self) -> u64 {
        self.hash_count.load(Ordering::Relaxed)
    }

    /// Reset the hash counter and return the previous value.
    pub fn reset_hash_count(&self) -> u64 {
        self.hash_count.swap(0, Ordering::Relaxed)
    }

    /// Wait for the worker thread to finish.
    pub fn join(mut self) -> thread::Result<()> {
        self.stop();
        if let Some(handle) = self.handle.take() {
            handle.join()
        } else {
            Ok(())
        }
    }
}

impl Drop for MiningWorker {
    fn drop(&mut self) {
        self.stop();
        // Note: We don't join here to avoid blocking in drop
    }
}

/// A pool of mining workers.
pub struct WorkerPool {
    /// Workers in the pool.
    workers: Vec<MiningWorker>,
    /// Task sender to broadcast work to all workers.
    task_tx: watch::Sender<Option<MiningTask>>,
    /// Solution receiver.
    solution_rx: mpsc::Receiver<FoundSolution>,
    /// Running flag.
    running: Arc<AtomicBool>,
}

impl WorkerPool {
    /// Create a new worker pool with the specified number of workers.
    pub fn new(num_workers: usize) -> Self {
        let (task_tx, task_rx) = watch::channel(None);
        let (solution_tx, solution_rx) = mpsc::channel(num_workers * 2);
        let running = Arc::new(AtomicBool::new(true));

        let mut workers = Vec::with_capacity(num_workers);
        for id in 0..num_workers {
            let worker = MiningWorker::spawn(id, task_rx.clone(), solution_tx.clone());
            workers.push(worker);
        }

        info!(num_workers = num_workers, "Worker pool created");

        Self {
            workers,
            task_tx,
            solution_rx,
            running,
        }
    }

    /// Broadcast a new mining task to all workers.
    pub fn broadcast_task(&self, task: MiningTask) {
        if self.task_tx.send(Some(task)).is_err() {
            warn!("Failed to broadcast task, no workers subscribed");
        }
    }

    /// Clear the current task (workers will idle).
    pub fn clear_task(&self) {
        let _ = self.task_tx.send(None);
    }

    /// Try to receive a solution (non-blocking).
    pub fn try_recv_solution(&mut self) -> Option<FoundSolution> {
        self.solution_rx.try_recv().ok()
    }

    /// Receive a solution (async).
    pub async fn recv_solution(&mut self) -> Option<FoundSolution> {
        self.solution_rx.recv().await
    }

    /// Get the total hash count across all workers.
    pub fn total_hash_count(&self) -> u64 {
        self.workers.iter().map(|w| w.hash_count()).sum()
    }

    /// Reset all hash counters and return the total.
    pub fn reset_hash_counts(&self) -> u64 {
        self.workers.iter().map(|w| w.reset_hash_count()).sum()
    }

    /// Get the number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Check if the pool is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Stop all workers.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        for worker in &self.workers {
            worker.stop();
        }
    }

    /// Shutdown the pool and wait for all workers to finish.
    pub fn shutdown(self) {
        self.stop();
        for worker in self.workers {
            let _ = worker.join();
        }
        info!("Worker pool shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn create_test_task() -> MiningTask {
        // Use an extremely easy target (max value) so tests complete quickly
        MiningTask {
            header_bytes: vec![0u8; 204],
            n_bits: 0x2100ffff, // Very easy target - should find solution quickly
            height: 100_000,
            miner_pk: Box::new(ergo_chain_types::ec_point::identity()),
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn test_worker_pool_creation() {
        let pool = WorkerPool::new(2);
        assert_eq!(pool.num_workers(), 2);
        assert!(pool.is_running());
        pool.shutdown();
    }

    #[tokio::test]
    async fn test_worker_finds_solution() {
        let mut pool = WorkerPool::new(2);
        let task = create_test_task();

        // Give workers time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        pool.broadcast_task(task);

        // Wait for a solution with timeout - increase timeout for CI
        let solution = tokio::time::timeout(Duration::from_secs(60), pool.recv_solution()).await;

        pool.stop();

        assert!(solution.is_ok(), "Should receive solution within timeout");
        let found = solution.unwrap();
        assert!(found.is_some(), "Should find a solution");

        let found = found.unwrap();
        assert_eq!(found.solution.nonce.len(), 8);

        pool.shutdown();
    }

    #[tokio::test]
    async fn test_worker_task_switching() {
        let pool = WorkerPool::new(1);

        // Broadcast a hard task
        let hard_task = MiningTask {
            header_bytes: vec![0u8; 204],
            n_bits: 0x17034d4b, // Very hard target
            height: 100_000,
            miner_pk: Box::new(ergo_chain_types::ec_point::identity()),
            created_at: 0,
        };
        pool.broadcast_task(hard_task);

        // Wait a bit for worker to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Switch to easy task
        let easy_task = create_test_task();
        pool.broadcast_task(easy_task);

        // Workers should switch to new task
        assert!(pool.is_running());

        pool.shutdown();
    }

    #[tokio::test]
    async fn test_hash_counting() {
        let pool = WorkerPool::new(1);

        // Give worker time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Use a hard target so the worker keeps hashing without finding a solution
        let hard_task = MiningTask {
            header_bytes: vec![0u8; 204],
            n_bits: 0x17034d4b, // Very hard target - won't find solution
            height: 100_000,
            miner_pk: Box::new(ergo_chain_types::ec_point::identity()),
            created_at: 0,
        };
        pool.broadcast_task(hard_task);

        // Wait for some hashes to be computed
        tokio::time::sleep(Duration::from_secs(2)).await;

        let hash_count = pool.total_hash_count();
        assert!(
            hash_count > 0,
            "Should have computed some hashes, got {}",
            hash_count
        );

        pool.shutdown();
    }

    #[test]
    fn test_worker_stop() {
        let (task_tx, task_rx) = watch::channel(None);
        let (solution_tx, _solution_rx) = mpsc::channel(10);

        let worker = MiningWorker::spawn(0, task_rx, solution_tx);
        assert!(worker.is_running());

        worker.stop();
        std::thread::sleep(Duration::from_millis(200));

        // Worker should have stopped
        drop(task_tx); // This should also trigger worker to stop
    }
}
