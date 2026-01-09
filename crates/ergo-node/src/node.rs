//! Node implementation.

use crate::config::NodeConfig;
use anyhow::Result;
use ergo_api::AppState;
use ergo_consensus::block::{
    genesis_parent_header, BlockId, BlockTransactions, BoxId, Digest32, ErgoBox, Extension,
    FullBlock, Header,
};
use ergo_consensus::FullBlockValidator;
use ergo_mempool::Mempool;
use ergo_mining::{CandidateGenerator, Miner, MinerConfig};
use ergo_network::{
    DeclaredAddress, Handshake, Message, NetworkCommand, NetworkConfig, NetworkEvent,
    NetworkService, PeerId, PeerManager, PeerSpec, MAINNET_MAGIC, TESTNET_MAGIC,
};
use ergo_state::{BoxEntry, StateChange, StateManager};
use ergo_storage::Database;
use ergo_sync::{SyncCommand, SyncConfig, SyncEvent, SyncProtocol};
use ergo_wallet::{Wallet, WalletConfig as WalletCfg};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Tracks a peer that we want to reconnect to.
#[derive(Debug, Clone)]
struct ReconnectInfo {
    /// Socket address to reconnect to.
    addr: SocketAddr,
    /// Number of consecutive failed connection attempts.
    attempts: u32,
    /// Last attempt time.
    last_attempt: Instant,
    /// Next allowed attempt time (with exponential backoff).
    next_attempt: Instant,
}

impl ReconnectInfo {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            attempts: 0,
            last_attempt: Instant::now(),
            next_attempt: Instant::now(),
        }
    }

    /// Calculate backoff duration based on attempt count.
    /// Uses exponential backoff: 5s, 10s, 20s, 40s, 80s, capped at 5 minutes.
    fn backoff_duration(&self) -> Duration {
        let base_secs = 5u64;
        let max_secs = 300u64; // 5 minutes
        let secs = base_secs.saturating_mul(1 << self.attempts.min(6));
        Duration::from_secs(secs.min(max_secs))
    }

    /// Mark a connection attempt as failed.
    fn mark_failed(&mut self) {
        self.attempts += 1;
        self.last_attempt = Instant::now();
        self.next_attempt = Instant::now() + self.backoff_duration();
    }

    /// Check if we can attempt reconnection now.
    fn can_attempt(&self) -> bool {
        Instant::now() >= self.next_attempt
    }

    /// Reset on successful connection.
    fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Maximum number of reconnection attempts before giving up on a peer.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// The main node struct coordinating all components.
pub struct Node {
    /// Node configuration.
    config: NodeConfig,
    /// Storage database.
    storage: Arc<Database>,
    /// State manager.
    state: Arc<StateManager>,
    /// Transaction mempool.
    mempool: Arc<Mempool>,
    /// Peer manager.
    peers: Arc<PeerManager>,
    /// Miner (optional).
    miner: Option<Arc<Miner>>,
    /// Wallet (optional).
    wallet: Option<Arc<Wallet>>,
    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// API server handle.
    api_handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// Network command sender.
    network_cmd_tx: Option<mpsc::Sender<NetworkCommand>>,
    /// Sync command sender.
    sync_cmd_tx: Option<mpsc::Sender<SyncCommand>>,
}

impl Node {
    /// Create a new node.
    pub async fn new(config: NodeConfig) -> Result<Arc<Self>> {
        // Create data directory
        std::fs::create_dir_all(&config.data_dir)?;

        // Open database
        let db_path = config.data_dir.join("db");
        info!("Opening database at {:?}", db_path);
        let storage = Arc::new(Database::open(&db_path)?);

        // Initialize state manager
        let state = Arc::new(StateManager::init_from_storage(
            Arc::clone(&storage) as Arc<dyn ergo_storage::Storage>
        )?);

        // Initialize mempool
        let mempool = Arc::new(Mempool::with_defaults());

        // Initialize peer manager
        let peers = Arc::new(PeerManager::default());

        // Add known peers
        for peer_addr in &config.network_config.known_peers {
            if let Ok(addr) = peer_addr.parse::<SocketAddr>() {
                let info = ergo_network::PeerInfo::new(addr, true);
                peers.add_peer(info);
            }
        }

        // Initialize miner if enabled
        let miner = if config.mining.enabled {
            let candidate_gen = Arc::new(CandidateGenerator::new(
                Arc::clone(&state),
                Arc::clone(&mempool),
            ));

            let miner_config = MinerConfig {
                internal_mining: false,
                external_mining: config.mining.external,
                reward_address: config.mining.reward_address.clone().unwrap_or_default(),
                threads: 1,
            };

            let miner = Arc::new(Miner::new(miner_config, candidate_gen));
            miner.start();
            Some(miner)
        } else {
            None
        };

        // Initialize wallet if enabled
        let wallet = if config.wallet.enabled {
            let wallet_cfg = WalletCfg {
                data_dir: config.data_dir.join(&config.wallet.data_dir),
                secret_file: config.wallet.secret_file.clone(),
                ..Default::default()
            };

            let wallet = Arc::new(Wallet::new(wallet_cfg, Arc::clone(&state)));

            // Try to load existing wallet
            if wallet.load().is_ok() {
                info!("Loaded existing wallet");
            }

            Some(wallet)
        } else {
            None
        };

        let node = Arc::new(Self {
            config,
            storage,
            state,
            mempool,
            peers,
            miner,
            wallet,
            shutdown: Arc::new(AtomicBool::new(false)),
            api_handle: RwLock::new(None),
            network_cmd_tx: None,
            sync_cmd_tx: None,
        });

        Ok(node)
    }

    /// Run the node.
    pub async fn run(self: &Arc<Self>) -> Result<()> {
        info!("Starting node services...");

        // Start API server
        self.start_api().await?;

        // Start P2P networking
        self.start_networking().await?;

        // Start synchronization
        self.start_sync().await?;

        // Main loop
        while !self.shutdown.load(Ordering::SeqCst) {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            // Periodic tasks
            self.tick().await;
        }

        info!("Node main loop stopped");
        Ok(())
    }

    /// Start the API server.
    async fn start_api(&self) -> Result<()> {
        let bind_addr: SocketAddr = self.config.api.bind_address.parse()?;

        let app_state = AppState::new(
            Arc::clone(&self.state),
            Arc::clone(&self.mempool),
            Arc::clone(&self.peers),
            self.config.node_name.clone(),
        )
        .with_mining(
            self.config.mining.enabled,
            self.config.mining.reward_address.clone(),
        );

        let app_state = if let Some(ref key) = self.config.api.api_key {
            app_state.with_api_key(key.clone())
        } else {
            app_state
        };

        let router = ergo_api::build_api(app_state);

        info!("Starting API server on {}", bind_addr);

        let listener = tokio::net::TcpListener::bind(bind_addr).await?;

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                warn!("API server error: {}", e);
            }
        });

        *self.api_handle.write().await = Some(handle);

        Ok(())
    }

    /// Start P2P networking and synchronization.
    async fn start_networking(&self) -> Result<()> {
        let bind_addr: SocketAddr = self.config.network_config.bind_address.parse()?;

        let magic = if self.config.network == "testnet" {
            TESTNET_MAGIC
        } else {
            MAINNET_MAGIC
        };

        let network_config = NetworkConfig {
            listen_addr: bind_addr,
            magic,
            agent_name: "ergo-rust-node".to_string(),
            node_name: self.config.node_name.clone(),
            max_connections: self.config.network_config.max_connections,
            ..Default::default()
        };

        info!("Starting P2P networking on {}", bind_addr);

        // Create network service
        let (network_service, mut network_event_rx, network_cmd_tx) =
            NetworkService::new(network_config, Arc::clone(&self.peers));

        // Create sync protocol
        let (sync_cmd_tx, mut sync_cmd_rx) = mpsc::channel::<SyncCommand>(500);
        // Large buffer for sync events to avoid blocking during header sync
        // Headers come in batches of 400 from multiple peers, and disk I/O can be slow
        // Need enough capacity to buffer while headers are being written to storage
        let (sync_event_tx, mut sync_event_rx) = mpsc::channel::<SyncEvent>(10000);
        let sync_protocol = SyncProtocol::new(SyncConfig::default(), sync_cmd_tx.clone());

        // Initialize sync protocol with stored headers from database
        let (_, header_height) = self.state.heights();
        info!("Initializing sync with header_height={}", header_height);
        if header_height > 0 {
            // Set the synchronizer's height to our stored header height
            sync_protocol.set_height(header_height);

            // Load ALL stored header IDs so sync protocol knows what we already have
            match self.state.get_headers(1, header_height) {
                Ok(headers) if !headers.is_empty() => {
                    info!(
                        "Loading {} stored headers into sync protocol",
                        headers.len()
                    );
                    let all_header_ids: Vec<Vec<u8>> =
                        headers.iter().map(|h| h.id.0.as_ref().to_vec()).collect();

                    // Get exponentially spaced locator for SyncInfo messages
                    let locator_ids = match self.state.get_header_locator() {
                        Ok(locator) => {
                            info!("Got {} locator headers for SyncInfo", locator.len());
                            locator.iter().map(|id| id.0.as_ref().to_vec()).collect()
                        }
                        Err(e) => {
                            warn!("Failed to get locator, using last headers: {}", e);
                            // Fallback: use last N headers
                            all_header_ids.iter().rev().take(21).cloned().collect()
                        }
                    };

                    // Get headers at V2 SyncInfo offsets: [0, 16, 128, 512] from best height
                    // These are sent in newest-first order (offset 0 = best header first)
                    // The Scala node uses these specific offsets for commonPoint detection
                    const V2_SYNC_OFFSETS: [u32; 4] = [0, 16, 128, 512];
                    let v2_headers: Vec<_> = V2_SYNC_OFFSETS
                        .iter()
                        .filter_map(|offset| {
                            let target_height = header_height.saturating_sub(*offset);
                            if target_height > 0 {
                                // Find header at this height in our loaded headers
                                headers.iter().find(|h| h.height == target_height).cloned()
                            } else {
                                None
                            }
                        })
                        .collect();
                    info!(
                        "Got {} headers for V2 SyncInfo at offsets {:?}, heights: {:?}",
                        v2_headers.len(),
                        V2_SYNC_OFFSETS,
                        v2_headers.iter().map(|h| h.height).collect::<Vec<_>>()
                    );

                    sync_protocol.init_from_stored_headers(all_header_ids, locator_ids, v2_headers);
                }
                Ok(_) => {
                    warn!(
                        "No headers found in storage despite header_height={}",
                        header_height
                    );
                }
                Err(e) => {
                    warn!("Failed to get headers from storage: {}", e);
                }
            }
        }

        let network_cmd_tx_clone = network_cmd_tx.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let mempool = Arc::clone(&self.mempool);

        // Spawn network service
        tokio::spawn(async move {
            if let Err(e) = network_service.run().await {
                error!("Network service error: {}", e);
            }
        });

        // Spawn sync protocol event handler
        let sync_protocol = Arc::new(sync_protocol);
        let sync_protocol_clone = Arc::clone(&sync_protocol);
        let shutdown_for_sync = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            while !shutdown_for_sync.load(Ordering::SeqCst) {
                match sync_event_rx.recv().await {
                    Some(event) => {
                        if let Err(e) = sync_protocol_clone.handle_event(event).await {
                            warn!("Sync event error: {}", e);
                        }
                    }
                    None => break,
                }
            }
            info!("Sync protocol stopped");
        });

        // Connect to known peers
        for peer_addr in &self.config.network_config.known_peers {
            if let Ok(addr) = peer_addr.parse::<SocketAddr>() {
                let _ = network_cmd_tx.send(NetworkCommand::Connect { addr }).await;
            }
        }

        // Spawn event router - bridges network events to sync protocol
        let network_cmd_tx_for_router = network_cmd_tx_clone.clone();
        let shutdown_for_router = shutdown.clone();
        let sync_event_tx_clone = sync_event_tx.clone();
        let state_for_router = Arc::clone(&self.state);

        // Track peer addresses for reconnection
        let mut peer_addresses: HashMap<PeerId, SocketAddr> = HashMap::new();
        // Track peer handshakes for GetPeers response
        let mut peer_handshakes: HashMap<PeerId, Handshake> = HashMap::new();
        // Track peers we want to reconnect to
        let mut reconnect_queue: HashMap<SocketAddr, ReconnectInfo> = HashMap::new();
        // Track discovered peers from Peers messages (for future connections)
        let mut discovered_peers: std::collections::HashSet<SocketAddr> =
            std::collections::HashSet::new();
        // Desired minimum number of connections
        let min_connections: usize = 3;
        // Maximum number of connections to maintain (increased from 10 for faster sync)
        let max_connections: usize = 25;

        // Buffer for blocks received out of order, indexed by height
        let mut pending_blocks: std::collections::BTreeMap<u32, (Vec<u8>, Vec<u8>)> =
            std::collections::BTreeMap::new();

        tokio::spawn(async move {
            // Tick interval for sync protocol housekeeping (every 1 second)
            // This needs to be fast enough to request pending headers before peer disconnects
            let mut tick_interval = tokio::time::interval(std::time::Duration::from_secs(1));
            tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Reconnection check interval (every 10 seconds)
            let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_secs(10));
            reconnect_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Block sync check interval (every 5 seconds)
            let mut block_sync_interval = tokio::time::interval(std::time::Duration::from_secs(5));
            block_sync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            while !shutdown_for_router.load(Ordering::SeqCst) {
                tokio::select! {
                    // Periodic tick for sync protocol
                    _ = tick_interval.tick() => {
                        // Use try_send to avoid blocking if channel is full
                        let _ = sync_event_tx_clone.try_send(SyncEvent::Tick);
                    }
                    // Periodic block sync check - triggers block downloads when headers are ahead
                    _ = block_sync_interval.tick() => {
                        let (utxo_height, header_height) = state_for_router.heights();
                        // Only trigger if we have headers ahead of our UTXO state
                        // and there's a meaningful gap (at least 1 block behind)
                        if header_height > utxo_height && header_height > 0 {
                            // Request next batch of blocks starting from current UTXO height + 1
                            let batch_size = 16.min(header_height - utxo_height) as u32;
                            match state_for_router.get_headers(utxo_height + 1, batch_size) {
                                Ok(headers) if !headers.is_empty() => {
                                    info!(
                                        utxo_height,
                                        header_height,
                                        batch_size = headers.len(),
                                        first_block = headers.first().map(|h| h.height),
                                        "Triggering block download for missing blocks"
                                    );
                                    let _ = sync_event_tx_clone.send(SyncEvent::RequestBlocks {
                                        headers,
                                    }).await;
                                }
                                Ok(_) => {
                                    debug!(utxo_height, header_height, "No headers found for block download");
                                }
                                Err(e) => {
                                    warn!(error = %e, "Failed to get headers for block sync check");
                                }
                            }
                        }
                    }
                    // Periodic reconnection attempts
                    _ = reconnect_interval.tick() => {
                        let connected_count = peer_addresses.len();

                        // Try to reconnect to peers if we're below minimum
                        if connected_count < min_connections {
                            let mut to_remove = Vec::new();

                            for (addr, info) in reconnect_queue.iter_mut() {
                                if info.attempts >= MAX_RECONNECT_ATTEMPTS {
                                    info!(addr = %addr, attempts = info.attempts, "Giving up on peer after max attempts");
                                    to_remove.push(*addr);
                                    continue;
                                }

                                if info.can_attempt() {
                                    info!(
                                        addr = %addr,
                                        attempt = info.attempts + 1,
                                        backoff_secs = info.backoff_duration().as_secs(),
                                        "Attempting to reconnect to peer"
                                    );
                                    let _ = network_cmd_tx_for_router.send(NetworkCommand::Connect { addr: *addr }).await;
                                    // Don't mark as failed yet - wait for ConnectionFailed event
                                }
                            }

                            // Remove peers we've given up on
                            for addr in to_remove {
                                reconnect_queue.remove(&addr);
                            }
                        }

                        // Try connecting to discovered peers if we're below max connections
                        if connected_count < max_connections && !discovered_peers.is_empty() {
                            // Take up to 3 discovered peers to try
                            let peers_to_try: Vec<SocketAddr> = discovered_peers
                                .iter()
                                .filter(|addr| !peer_addresses.values().any(|a| a == *addr))
                                .filter(|addr| !reconnect_queue.contains_key(*addr))
                                .take(3)
                                .cloned()
                                .collect();

                            for addr in peers_to_try {
                                info!(addr = %addr, "Attempting to connect to discovered peer");
                                // Remove from discovered set - if connection fails, we won't retry
                                // (to avoid spamming unreachable peers)
                                discovered_peers.remove(&addr);
                                let _ = network_cmd_tx_for_router.send(NetworkCommand::Connect { addr }).await;
                            }
                        }
                    }
                    // Handle network events - forward to sync protocol
                    Some(event) = network_event_rx.recv() => {
                        match event {
                            NetworkEvent::PeerConnected { peer_id, addr, handshake } => {
                                info!(peer = %peer_id, addr = %addr, agent = %handshake.agent_name, "Peer connected");

                                // Track the peer's address for potential reconnection later
                                peer_addresses.insert(peer_id.clone(), addr);
                                // Store handshake for GetPeers response
                                peer_handshakes.insert(peer_id.clone(), handshake);

                                // Remove from reconnect queue on successful connection
                                if let Some(info) = reconnect_queue.get_mut(&addr) {
                                    info.reset();
                                }
                                reconnect_queue.remove(&addr);
                                // Also remove from discovered peers (we're now connected)
                                discovered_peers.remove(&addr);

                                // Send GetPeers to discover more peers
                                info!(peer = %peer_id, "Sending GetPeers request to discover more peers");
                                let _ = network_cmd_tx_for_router
                                    .send(NetworkCommand::SendMessage {
                                        peer_id: peer_id.clone(),
                                        message: Message::GetPeers,
                                    })
                                    .await;

                                let _ = sync_event_tx_clone.send(SyncEvent::PeerConnected {
                                    peer: peer_id,
                                }).await;
                            }
                            NetworkEvent::PeerDisconnected { peer_id } => {
                                info!(peer = %peer_id, "Peer disconnected");

                                // Get the address before removing
                                if let Some(addr) = peer_addresses.remove(&peer_id) {
                                    // Add to reconnect queue if not already there
                                    if !reconnect_queue.contains_key(&addr) {
                                        info!(addr = %addr, "Adding peer to reconnection queue");
                                        reconnect_queue.insert(addr, ReconnectInfo::new(addr));
                                    }
                                }
                                // Remove handshake data
                                peer_handshakes.remove(&peer_id);

                                let _ = sync_event_tx_clone.send(SyncEvent::PeerDisconnected {
                                    peer: peer_id,
                                }).await;
                            }
                            NetworkEvent::MessageReceived { peer_id, message } => {
                                debug!(peer = %peer_id, msg = ?message.message_type(), "Message received");
                                Self::handle_message(
                                    &peer_id,
                                    message,
                                    &mempool,
                                    &sync_event_tx_clone,
                                    &peer_addresses,
                                    &peer_handshakes,
                                    &network_cmd_tx_for_router,
                                    &mut discovered_peers,
                                ).await;
                            }
                            NetworkEvent::ConnectionFailed { addr, error } => {
                                warn!(addr = %addr, error = %error, "Connection failed");

                                // Update reconnect info with failed attempt
                                if let Some(info) = reconnect_queue.get_mut(&addr) {
                                    info.mark_failed();
                                    info!(
                                        addr = %addr,
                                        attempts = info.attempts,
                                        next_attempt_secs = info.backoff_duration().as_secs(),
                                        "Will retry connection with backoff"
                                    );
                                } else {
                                    // Add to reconnect queue for first-time failures (e.g., initial connection)
                                    let mut new_info = ReconnectInfo::new(addr);
                                    new_info.mark_failed();
                                    info!(
                                        addr = %addr,
                                        next_attempt_secs = new_info.backoff_duration().as_secs(),
                                        "Adding failed peer to reconnection queue"
                                    );
                                    reconnect_queue.insert(addr, new_info);
                                }
                            }
                        }
                    }
                    // Handle sync commands that need network actions or state updates
                    Some(cmd) = sync_cmd_rx.recv() => {
                        match cmd {
                            SyncCommand::SendToPeer { peer, message } => {
                                let _ = network_cmd_tx_for_router.send(NetworkCommand::SendMessage {
                                    peer_id: peer,
                                    message,
                                }).await;
                            }
                            SyncCommand::Broadcast { message } => {
                                let _ = network_cmd_tx_for_router.send(NetworkCommand::Broadcast {
                                    message,
                                }).await;
                            }
                            SyncCommand::StoreHeader { header, raw_bytes, response_tx } => {
                                let height = header.height;
                                let result = match state_for_router.apply_header_with_bytes(header, &raw_bytes) {
                                    Ok(_) => {
                                        debug!(height, "Header stored in state");
                                        Ok(())
                                    }
                                    Err(e) => {
                                        warn!(height, error = %e, "Failed to store header");
                                        Err(e.to_string())
                                    }
                                };
                                let _ = response_tx.send(result);
                            }
                            SyncCommand::ApplyBlock { block_id, block_data } => {
                                debug!(block_id = %hex::encode(&block_id), size = block_data.len(), "Received block for application");

                                // Get the header to determine block height
                                let header_id = BlockId(
                                    Digest32::from(
                                        <[u8; 32]>::try_from(block_id.as_slice()).unwrap_or([0u8; 32])
                                    )
                                );

                                let header = match state_for_router.get_header(&header_id) {
                                    Ok(Some(h)) => h,
                                    Ok(None) => {
                                        warn!(block_id = %hex::encode(&block_id), "Header not found for block");
                                        continue;
                                    }
                                    Err(e) => {
                                        warn!(block_id = %hex::encode(&block_id), error = %e, "Failed to get header");
                                        continue;
                                    }
                                };

                                let block_height = header.height;
                                let (current_utxo_height, _) = state_for_router.heights();

                                // Check if this is the next block we need
                                if block_height == current_utxo_height + 1 {
                                    // This is the next block - apply it directly
                                    debug!(height = block_height, "Block is next in sequence, applying directly");
                                } else if block_height > current_utxo_height + 1 {
                                    // Block arrived out of order - buffer it
                                    debug!(
                                        block_height,
                                        current_utxo_height,
                                        buffered = pending_blocks.len(),
                                        "Block arrived out of order, buffering"
                                    );
                                    pending_blocks.insert(block_height, (block_id.clone(), block_data.clone()));
                                    continue;
                                } else {
                                    // Block is at or below current height - already applied or stale
                                    debug!(block_height, current_utxo_height, "Block already applied or stale, skipping");
                                    continue;
                                }

                                // Collect blocks to process: current one + any buffered sequential blocks
                                let mut blocks_to_process = vec![(block_id.clone(), block_data.clone())];

                                // Collect buffered blocks that form a contiguous sequence
                                let (current_utxo, _) = state_for_router.heights();
                                let mut next_height = current_utxo + 2; // +1 is the current block, +2 is next
                                while let Some((next_bid, next_bdata)) = pending_blocks.remove(&next_height) {
                                    blocks_to_process.push((next_bid, next_bdata));
                                    next_height += 1;
                                }

                                // Batch size for writing to disk (tune for performance)
                                const BATCH_WRITE_SIZE: usize = 32;

                                // Validate blocks and collect them for batched application
                                let mut validated_blocks: Vec<(ergo_consensus::FullBlock, StateChange, Vec<u8>)> = Vec::new();
                                let mut validation_failed = false;

                                for (bid, bdata) in blocks_to_process {
                                    if validation_failed {
                                        // Re-buffer blocks after a failure
                                        let hdr_id = BlockId(Digest32::from(<[u8; 32]>::try_from(bid.as_slice()).unwrap_or([0u8; 32])));
                                        if let Ok(Some(hdr)) = state_for_router.get_header(&hdr_id) {
                                            pending_blocks.insert(hdr.height, (bid, bdata));
                                        }
                                        continue;
                                    }

                                    // Parse BlockTransactions from raw data
                                    let block_txs = match BlockTransactions::parse(&bdata) {
                                        Ok(txs) => txs,
                                        Err(e) => {
                                            warn!(block_id = %hex::encode(&bid), error = ?e, "Failed to parse BlockTransactions");
                                            let _ = sync_event_tx_clone.send(SyncEvent::BlockFailed {
                                                block_id: bid,
                                                error: format!("Parse error: {:?}", e),
                                            }).await;
                                            validation_failed = true;
                                            continue;
                                        }
                                    };

                                    // Get the header from state manager
                                    let hdr_id = BlockId(
                                        Digest32::from(
                                            <[u8; 32]>::try_from(bid.as_slice()).unwrap_or([0u8; 32])
                                        )
                                    );

                                    let hdr = match state_for_router.get_header(&hdr_id) {
                                        Ok(Some(h)) => h,
                                        Ok(None) => {
                                            warn!(block_id = %hex::encode(&bid), "Header not found for block");
                                            validation_failed = true;
                                            continue;
                                        }
                                        Err(e) => {
                                            warn!(block_id = %hex::encode(&bid), error = %e, "Failed to get header");
                                            validation_failed = true;
                                            continue;
                                        }
                                    };

                                    let height = hdr.height;

                                    // Get parent header for validation
                                    let parent_header = if height > 1 {
                                        match state_for_router.get_header(&hdr.parent_id) {
                                            Ok(Some(h)) => h,
                                            Ok(None) => {
                                                warn!(height, "Parent header not found");
                                                validation_failed = true;
                                                continue;
                                            }
                                            Err(e) => {
                                                warn!(height, error = %e, "Failed to get parent header");
                                                validation_failed = true;
                                                continue;
                                            }
                                        }
                                    } else {
                                        genesis_parent_header()
                                    };

                                    // Create FullBlock
                                    let extension = Extension::empty(hdr_id.clone());
                                    let full_block = FullBlock::new(hdr.clone(), block_txs, extension, None);

                                    // Get last 10 headers for ErgoScript context
                                    let last_headers: [Header; 10] = {
                                        let mut headers = Vec::with_capacity(10);
                                        let start_height = if height > 10 { height - 10 } else { 1 };
                                        for h in start_height..height {
                                            if let Ok(Some(hdr)) = state_for_router.history.headers.get_by_height(h) {
                                                headers.push(hdr);
                                            }
                                        }
                                        while headers.len() < 10 {
                                            headers.insert(0, genesis_parent_header());
                                        }
                                        headers.try_into().unwrap_or_else(|_| std::array::from_fn(|_| genesis_parent_header()))
                                    };

                                    // Validate block
                                    let validator = FullBlockValidator::new();
                                    let utxo_state = &state_for_router.utxo;
                                    let utxo_lookup = |box_id: &[u8]| -> Option<ErgoBox> {
                                        utxo_state.get_box_by_bytes(box_id).ok().flatten().map(|entry| entry.ergo_box)
                                    };

                                    let validation_result = validator.validate_block(
                                        &full_block,
                                        &parent_header,
                                        utxo_lookup,
                                        last_headers,
                                    );

                                    if !validation_result.valid {
                                        warn!(
                                            height,
                                            block_id = %hex::encode(&bid),
                                            error = ?validation_result.error,
                                            "Block validation failed"
                                        );
                                        let _ = sync_event_tx_clone.send(SyncEvent::BlockFailed {
                                            block_id: bid,
                                            error: validation_result.error.unwrap_or_else(|| "Unknown validation error".to_string()),
                                        }).await;
                                        validation_failed = true;
                                        continue;
                                    }

                                    // Convert validated state change
                                    let validated_change = validation_result.state_change.expect("Valid block must have state change");
                                    let state_change = StateChange {
                                        spent: validated_change.spent.iter().filter_map(|s| {
                                            if s.box_id.len() == 32 {
                                                let mut arr = [0u8; 32];
                                                arr.copy_from_slice(&s.box_id);
                                                Some(BoxId::from(Digest32::from(arr)))
                                            } else {
                                                None
                                            }
                                        }).collect(),
                                        created: validated_change.created.iter().map(|c| {
                                            BoxEntry::new(
                                                c.ergo_box.clone(),
                                                height,
                                                c.tx_id.clone(),
                                                c.output_index,
                                            )
                                        }).collect(),
                                    };

                                    debug!(
                                        height,
                                        total_cost = validation_result.total_cost,
                                        spent = state_change.spent.len(),
                                        created = state_change.created.len(),
                                        "Block validated"
                                    );

                                    validated_blocks.push((full_block, state_change, bid));

                                    // Apply batch if we've reached the batch size
                                    if validated_blocks.len() >= BATCH_WRITE_SIZE {
                                        let batch_count = validated_blocks.len();
                                        let first_h = validated_blocks[0].0.height();
                                        let last_h = validated_blocks[batch_count - 1].0.height();

                                        // Convert to the format expected by apply_blocks_batched
                                        let blocks_for_batch: Vec<_> = validated_blocks
                                            .drain(..)
                                            .map(|(fb, sc, bid)| ((fb, sc), bid))
                                            .collect();

                                        let block_ids: Vec<_> = blocks_for_batch.iter().map(|(_, bid)| bid.clone()).collect();
                                        let blocks: Vec<_> = blocks_for_batch.into_iter().map(|((fb, sc), _)| (fb, sc)).collect();

                                        match state_for_router.apply_blocks_batched(blocks) {
                                            Ok(_) => {
                                                info!(first_h, last_h, batch_count, "Batch of blocks applied successfully");
                                                // Notify sync protocol for each block
                                                for (i, bid) in block_ids.into_iter().enumerate() {
                                                    let h = first_h + i as u32;
                                                    let _ = sync_event_tx_clone.send(SyncEvent::BlockApplied {
                                                        block_id: bid,
                                                        height: h,
                                                    }).await;
                                                }
                                            }
                                            Err(e) => {
                                                warn!(first_h, last_h, error = %e, "Failed to apply block batch");
                                                for bid in block_ids {
                                                    let _ = sync_event_tx_clone.send(SyncEvent::BlockFailed {
                                                        block_id: bid,
                                                        error: e.to_string(),
                                                    }).await;
                                                }
                                                validation_failed = true;
                                            }
                                        }
                                    }
                                }

                                // Apply remaining validated blocks
                                if !validated_blocks.is_empty() && !validation_failed {
                                    let batch_count = validated_blocks.len();
                                    let first_h = validated_blocks[0].0.height();
                                    let last_h = validated_blocks[batch_count - 1].0.height();

                                    let blocks_for_batch: Vec<_> = validated_blocks
                                        .drain(..)
                                        .map(|(fb, sc, bid)| ((fb, sc), bid))
                                        .collect();

                                    let block_ids: Vec<_> = blocks_for_batch.iter().map(|(_, bid)| bid.clone()).collect();
                                    let blocks: Vec<_> = blocks_for_batch.into_iter().map(|((fb, sc), _)| (fb, sc)).collect();

                                    match state_for_router.apply_blocks_batched(blocks) {
                                        Ok(_) => {
                                            info!(first_h, last_h, batch_count, "Final batch of blocks applied successfully");
                                            for (i, bid) in block_ids.into_iter().enumerate() {
                                                let h = first_h + i as u32;
                                                let _ = sync_event_tx_clone.send(SyncEvent::BlockApplied {
                                                    block_id: bid,
                                                    height: h,
                                                }).await;
                                            }
                                        }
                                        Err(e) => {
                                            warn!(first_h, last_h, error = %e, "Failed to apply final block batch");
                                            for bid in block_ids {
                                                let _ = sync_event_tx_clone.send(SyncEvent::BlockFailed {
                                                    block_id: bid,
                                                    error: e.to_string(),
                                                }).await;
                                            }
                                        }
                                    }
                                }

                                // After processing, request more blocks if we're still behind
                                let (utxo_height, header_height) = state_for_router.heights();
                                if utxo_height < header_height {
                                    // Get next batch of headers to download blocks for
                                    let batch_size = 16.min(header_height - utxo_height) as u32;
                                    match state_for_router.get_headers(utxo_height + 1, batch_size) {
                                        Ok(headers) if !headers.is_empty() => {
                                            debug!(
                                                utxo_height,
                                                header_height,
                                                batch_size = headers.len(),
                                                "Requesting next batch of blocks"
                                            );
                                            let _ = sync_event_tx_clone.send(SyncEvent::RequestBlocks {
                                                headers,
                                            }).await;
                                        }
                                        Ok(_) => {}
                                        Err(e) => {
                                            warn!(error = %e, "Failed to get headers for block download");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    else => break,
                }
            }
            info!("Event router stopped");
        });

        Ok(())
    }

    /// Handle an incoming P2P message.
    async fn handle_message(
        peer_id: &ergo_network::PeerId,
        message: Message,
        _mempool: &Arc<Mempool>,
        sync_event_tx: &mpsc::Sender<SyncEvent>,
        peer_addresses: &HashMap<PeerId, SocketAddr>,
        peer_handshakes: &HashMap<PeerId, Handshake>,
        network_cmd_tx: &mpsc::Sender<NetworkCommand>,
        discovered_peers: &mut std::collections::HashSet<SocketAddr>,
    ) {
        match message {
            Message::SyncInfo(sync_info) => {
                debug!(peer = %peer_id, height = sync_info.last_headers.len(), "SyncInfo received");
                // Use try_send to avoid deadlock - if channel is full, drop the event
                if sync_event_tx
                    .try_send(SyncEvent::SyncInfoReceived {
                        peer: peer_id.clone(),
                        info: sync_info,
                    })
                    .is_err()
                {
                    warn!("Sync event channel full, dropping SyncInfo");
                }
            }
            Message::Inv(inv) => {
                debug!(peer = %peer_id, count = inv.ids.len(), "Inv received");
                if sync_event_tx
                    .try_send(SyncEvent::InvReceived {
                        peer: peer_id.clone(),
                        inv,
                    })
                    .is_err()
                {
                    warn!("Sync event channel full, dropping Inv");
                }
            }
            Message::Modifier(modifier) => {
                debug!(peer = %peer_id, type_id = modifier.type_id, count = modifier.modifiers.len(), "Modifier received");
                // Send an event for each modifier in the batch
                // Use try_send to avoid deadlock
                for item in modifier.modifiers {
                    if sync_event_tx
                        .try_send(SyncEvent::ModifierReceived {
                            peer: peer_id.clone(),
                            type_id: modifier.type_id,
                            id: item.id,
                            data: item.data,
                        })
                        .is_err()
                    {
                        warn!("Sync event channel full, dropping Modifier");
                        break; // Stop trying if channel is full
                    }
                }
            }
            Message::RequestModifier(request) => {
                debug!(peer = %peer_id, type_id = request.type_id, count = request.ids.len(), "RequestModifier received");
                // We would respond with requested modifiers if we have them
            }
            Message::GetPeers => {
                debug!(peer = %peer_id, "GetPeers request - sending known peers");

                // Build PeerSpec list from connected peers (excluding the requesting peer)
                let peer_specs: Vec<PeerSpec> = peer_addresses
                    .iter()
                    .filter(|(id, _)| *id != peer_id)
                    .filter_map(|(id, addr)| {
                        peer_handshakes.get(id).map(|handshake| {
                            // Convert Handshake to PeerSpec with declared address from socket
                            let declared_addr = Some(DeclaredAddress {
                                ip: addr.ip(),
                                port: addr.port(),
                            });
                            PeerSpec::new(
                                handshake.agent_name.clone(),
                                handshake.version,
                                handshake.node_name.clone(),
                                declared_addr,
                            )
                        })
                    })
                    .take(20) // Limit to 20 peers
                    .collect();

                if !peer_specs.is_empty() {
                    info!(peer = %peer_id, count = peer_specs.len(), "Responding with peers");
                    let response = Message::Peers(peer_specs);
                    let _ = network_cmd_tx
                        .send(NetworkCommand::SendMessage {
                            peer_id: peer_id.clone(),
                            message: response,
                        })
                        .await;
                } else {
                    debug!(peer = %peer_id, "No peers to share");
                }
            }
            Message::Peers(peers) => {
                info!(peer = %peer_id, count = peers.len(), "Peers received");
                // Store discovered peers for future connection attempts
                let mut added_count = 0;
                for peer_spec in &peers {
                    if let Some(declared_addr) = &peer_spec.declared_addr {
                        let addr = SocketAddr::new(declared_addr.ip, declared_addr.port);
                        // Don't add if we're already connected to this peer
                        if !peer_addresses.values().any(|a| *a == addr) {
                            // Don't add if already in discovered set
                            if discovered_peers.insert(addr) {
                                added_count += 1;
                                debug!(addr = %addr, "Added discovered peer");
                            }
                        }
                    }
                }
                if added_count > 0 {
                    info!(
                        added = added_count,
                        total_discovered = discovered_peers.len(),
                        "Added new discovered peers"
                    );
                }
            }
            Message::Handshake(_) => {
                // Handshake already handled by NetworkService
            }
        }
    }

    /// Start synchronization.
    async fn start_sync(&self) -> Result<()> {
        let (utxo_height, header_height) = self.state.heights();
        info!(
            "Current state: UTXO height={}, header height={}",
            utxo_height, header_height
        );

        // Sync is started as part of start_networking
        // The SyncProtocol will handle header and block synchronization

        Ok(())
    }

    /// Periodic tick.
    async fn tick(&self) {
        // Log stats periodically
        static COUNTER: AtomicBool = AtomicBool::new(false);

        if !COUNTER.swap(true, Ordering::SeqCst) {
            let (utxo_height, header_height) = self.state.heights();
            let mempool_stats = self.mempool.stats();
            let peer_count = self.peers.connected_count();

            info!(
                utxo_height,
                header_height,
                mempool_txs = mempool_stats.tx_count,
                peers = peer_count,
                "Node status"
            );
        }
    }

    /// Shutdown the node.
    pub async fn shutdown(&self) {
        info!("Shutting down node...");
        self.shutdown.store(true, Ordering::SeqCst);

        // Stop miner
        if let Some(ref miner) = self.miner {
            miner.stop();
        }

        // Cancel API server
        if let Some(handle) = self.api_handle.write().await.take() {
            handle.abort();
        }

        // Flush database
        if let Err(e) = self.storage.flush() {
            warn!("Error flushing database: {}", e);
        }

        info!("Node shutdown complete");
    }
}

impl Clone for Node {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            storage: Arc::clone(&self.storage),
            state: Arc::clone(&self.state),
            mempool: Arc::clone(&self.mempool),
            peers: Arc::clone(&self.peers),
            miner: self.miner.clone(),
            wallet: self.wallet.clone(),
            shutdown: Arc::clone(&self.shutdown),
            api_handle: RwLock::new(None), // Don't clone the handle
            network_cmd_tx: self.network_cmd_tx.clone(),
            sync_cmd_tx: self.sync_cmd_tx.clone(),
        }
    }
}
