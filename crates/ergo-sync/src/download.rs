//! Block download management.

use crate::{SyncError, SyncResult, PARALLEL_DOWNLOADS};
use ergo_network::PeerId;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// A download task.
#[derive(Debug, Clone)]
pub struct DownloadTask {
    /// Block/modifier ID.
    pub id: Vec<u8>,
    /// Modifier type.
    pub type_id: u8,
    /// Assigned peer.
    pub peer: Option<PeerId>,
    /// Request time.
    pub requested_at: Option<Instant>,
    /// Retry count.
    pub retries: u32,
    /// Peers that failed to deliver this block (for blacklisting).
    pub failed_peers: HashSet<PeerId>,
}

impl DownloadTask {
    /// Create a new download task.
    pub fn new(id: Vec<u8>, type_id: u8) -> Self {
        Self {
            id,
            type_id,
            peer: None,
            requested_at: None,
            retries: 0,
            failed_peers: HashSet::new(),
        }
    }

    /// Check if a peer has already failed to deliver this block.
    pub fn peer_failed(&self, peer: &PeerId) -> bool {
        self.failed_peers.contains(peer)
    }

    /// Check if task has timed out.
    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        self.requested_at
            .map(|t| t.elapsed() > timeout)
            .unwrap_or(false)
    }
}

/// Download configuration.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Maximum parallel downloads.
    pub max_parallel: usize,
    /// Request timeout.
    pub timeout: Duration,
    /// Maximum retries.
    pub max_retries: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_parallel: PARALLEL_DOWNLOADS,
            // Reduced from 30s to 10s for faster detection of stuck downloads
            timeout: Duration::from_secs(10),
            max_retries: 5,
        }
    }
}

/// Block download manager.
pub struct BlockDownloader {
    /// Configuration.
    config: DownloadConfig,
    /// Pending tasks (id -> task).
    pending: RwLock<HashMap<Vec<u8>, DownloadTask>>,
    /// In-flight requests (id -> peer).
    in_flight: RwLock<HashMap<Vec<u8>, PeerId>>,
    /// Completed IDs.
    completed: RwLock<HashSet<Vec<u8>>>,
    /// Failed IDs.
    failed: RwLock<HashSet<Vec<u8>>>,
}

impl BlockDownloader {
    /// Create a new block downloader.
    pub fn new(config: DownloadConfig) -> Self {
        Self {
            config,
            pending: RwLock::new(HashMap::new()),
            in_flight: RwLock::new(HashMap::new()),
            completed: RwLock::new(HashSet::new()),
            failed: RwLock::new(HashSet::new()),
        }
    }

    /// Add tasks to download queue.
    /// Skips tasks that are already completed, pending, or in-flight.
    pub fn queue(&self, tasks: Vec<DownloadTask>) {
        let mut pending = self.pending.write();
        let in_flight = self.in_flight.read();
        let completed = self.completed.read();
        let mut added = 0;
        for task in tasks {
            // Skip if already completed, pending, or in-flight
            if completed.contains(&task.id) {
                continue;
            }
            if in_flight.contains_key(&task.id) {
                continue;
            }
            if pending.contains_key(&task.id) {
                continue;
            }
            pending.insert(task.id.clone(), task);
            added += 1;
        }
        if added > 0 {
            debug!(
                added,
                total_pending = pending.len(),
                "Tasks queued for download"
            );
        }
    }

    /// Get tasks ready to dispatch.
    pub fn get_ready_tasks(&self, count: usize) -> Vec<DownloadTask> {
        let in_flight_count = self.in_flight.read().len();
        let available = self.config.max_parallel.saturating_sub(in_flight_count);
        let take = count.min(available);

        let pending = self.pending.read();
        pending
            .values()
            .filter(|t| t.peer.is_none())
            .take(take)
            .cloned()
            .collect()
    }

    /// Get tasks ready to dispatch to a specific peer (excluding failed peers).
    pub fn get_ready_tasks_for_peer(&self, count: usize, peer: &PeerId) -> Vec<DownloadTask> {
        let in_flight_count = self.in_flight.read().len();
        let available = self.config.max_parallel.saturating_sub(in_flight_count);
        let take = count.min(available);

        let pending = self.pending.read();
        pending
            .values()
            .filter(|t| t.peer.is_none() && !t.peer_failed(peer))
            .take(take)
            .cloned()
            .collect()
    }

    /// Check if there are any tasks that can't be dispatched to any available peer.
    /// Returns IDs of tasks that have failed with all provided peers.
    pub fn get_stuck_tasks(&self, available_peers: &[PeerId]) -> Vec<Vec<u8>> {
        if available_peers.is_empty() {
            return Vec::new();
        }

        let pending = self.pending.read();
        pending
            .values()
            .filter(|t| t.peer.is_none() && available_peers.iter().all(|p| t.peer_failed(p)))
            .map(|t| t.id.clone())
            .collect()
    }

    /// Clear failed peers for a task, allowing it to be retried with any peer.
    /// Used when a task is stuck because all available peers have failed.
    pub fn clear_failed_peers(&self, id: &[u8]) {
        if let Some(task) = self.pending.write().get_mut(id) {
            debug!(
                id = hex::encode(id),
                was_failed_peers = task.failed_peers.len(),
                "Clearing failed peers for stuck task"
            );
            task.failed_peers.clear();
        }
    }

    /// Mark a task as dispatched to a peer.
    pub fn dispatch(&self, id: &[u8], peer: PeerId) {
        if let Some(mut task) = self.pending.write().get_mut(id) {
            task.peer = Some(peer.clone());
            task.requested_at = Some(Instant::now());
        }
        self.in_flight.write().insert(id.to_vec(), peer);
    }

    /// Mark a task as completed.
    pub fn complete(&self, id: &[u8]) {
        self.pending.write().remove(id);
        self.in_flight.write().remove(id);
        self.completed.write().insert(id.to_vec());
        debug!(id = hex::encode(id), "Download completed");
    }

    /// Handle a failed download.
    pub fn fail(&self, id: &[u8], peer: &PeerId) {
        self.in_flight.write().remove(id);

        let mut pending = self.pending.write();
        if let Some(task) = pending.get_mut(id) {
            task.retries += 1;
            task.peer = None;
            task.requested_at = None;
            // Track this peer as having failed for this block
            task.failed_peers.insert(peer.clone());

            if task.retries >= self.config.max_retries {
                warn!(
                    id = hex::encode(id),
                    retries = task.retries,
                    failed_peers = task.failed_peers.len(),
                    "Download failed permanently"
                );
                pending.remove(id);
                drop(pending);
                self.failed.write().insert(id.to_vec());
            } else {
                debug!(
                    id = hex::encode(id),
                    retries = task.retries,
                    failed_peers = task.failed_peers.len(),
                    "Download will retry with different peer if available"
                );
            }
        }
    }

    /// Check for timed out requests.
    pub fn check_timeouts(&self) -> Vec<(Vec<u8>, PeerId)> {
        let mut timed_out = Vec::new();

        let pending = self.pending.read();
        for (id, task) in pending.iter() {
            if task.is_timed_out(self.config.timeout) {
                if let Some(peer) = &task.peer {
                    timed_out.push((id.clone(), peer.clone()));
                    debug!(
                        id = hex::encode(id),
                        elapsed_secs = task
                            .requested_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0),
                        retries = task.retries,
                        "Block download timed out"
                    );
                }
            }
        }

        if !timed_out.is_empty() {
            warn!(
                count = timed_out.len(),
                pending = pending.len(),
                "Found timed out block downloads"
            );
        }

        timed_out
    }

    /// Get download statistics.
    pub fn stats(&self) -> DownloadStats {
        DownloadStats {
            pending: self.pending.read().len(),
            in_flight: self.in_flight.read().len(),
            completed: self.completed.read().len(),
            failed: self.failed.read().len(),
        }
    }

    /// Check if all downloads are complete.
    pub fn is_complete(&self) -> bool {
        self.pending.read().is_empty() && self.in_flight.read().is_empty()
    }

    /// Clear completed set (to free memory).
    pub fn clear_completed(&self) {
        self.completed.write().clear();
    }
}

/// Download statistics.
#[derive(Debug, Clone, Default)]
pub struct DownloadStats {
    /// Pending downloads.
    pub pending: usize,
    /// In-flight requests.
    pub in_flight: usize,
    /// Completed downloads.
    pub completed: usize,
    /// Failed downloads.
    pub failed: usize,
}

impl Default for BlockDownloader {
    fn default() -> Self {
        Self::new(DownloadConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_queue() {
        let downloader = BlockDownloader::default();

        let tasks = vec![
            DownloadTask::new(vec![1; 32], 1),
            DownloadTask::new(vec![2; 32], 1),
            DownloadTask::new(vec![3; 32], 1),
        ];

        downloader.queue(tasks);

        let stats = downloader.stats();
        assert_eq!(stats.pending, 3);
        assert_eq!(stats.in_flight, 0);
    }

    #[test]
    fn test_dispatch_and_complete() {
        let downloader = BlockDownloader::default();

        let task = DownloadTask::new(vec![1; 32], 1);
        downloader.queue(vec![task]);

        let peer = PeerId::from_bytes(vec![9, 9, 9]);
        downloader.dispatch(&[1; 32], peer);

        assert_eq!(downloader.stats().in_flight, 1);

        downloader.complete(&[1; 32]);

        assert_eq!(downloader.stats().in_flight, 0);
        assert_eq!(downloader.stats().completed, 1);
    }
}
