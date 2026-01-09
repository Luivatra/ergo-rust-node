//! Network service for managing P2P connections.
//!
//! This module provides:
//! - TCP listener for incoming connections
//! - Connection pool management
//! - Message routing between peers and application
//! - Handshake protocol implementation

use crate::{
    ConnectionConfig, Handshake, HandshakeCodec, HandshakeData, Message, MessageCodec,
    NetworkError, NetworkResult, PeerId, PeerInfo, PeerManager, PeerState, MAINNET_MAGIC,
    PROTOCOL_VERSION,
};
use futures::stream::StreamExt;
use futures::SinkExt;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tracing::{debug, error, info, warn};

/// Network service configuration.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Listen address.
    pub listen_addr: SocketAddr,
    /// Network magic bytes.
    pub magic: [u8; 4],
    /// Agent name for handshake.
    pub agent_name: String,
    /// Node name for handshake.
    pub node_name: String,
    /// Maximum number of connections.
    pub max_connections: usize,
    /// Connection configuration.
    pub connection: ConnectionConfig,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9030".parse().unwrap(),
            magic: MAINNET_MAGIC,
            agent_name: "ergo-rust-node".to_string(),
            node_name: "ergo-rust".to_string(),
            max_connections: 50,
            connection: ConnectionConfig::default(),
        }
    }
}

/// Events emitted by the network service.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// A new peer connected and completed handshake.
    PeerConnected {
        peer_id: PeerId,
        addr: SocketAddr,
        handshake: Handshake,
    },
    /// A peer disconnected.
    PeerDisconnected { peer_id: PeerId },
    /// Received a message from a peer.
    MessageReceived { peer_id: PeerId, message: Message },
    /// Failed to connect to a peer.
    ConnectionFailed { addr: SocketAddr, error: String },
}

/// Commands that can be sent to the network service.
#[derive(Debug)]
pub enum NetworkCommand {
    /// Connect to a peer.
    Connect { addr: SocketAddr },
    /// Disconnect a peer.
    Disconnect { peer_id: PeerId },
    /// Send a message to a specific peer.
    SendMessage { peer_id: PeerId, message: Message },
    /// Broadcast a message to all connected peers.
    Broadcast { message: Message },
    /// Shutdown the service.
    Shutdown,
}

/// Handle to a peer connection for sending messages.
pub struct PeerHandle {
    /// Message sender to the peer's task.
    tx: mpsc::Sender<Message>,
    /// Peer info.
    info: PeerInfo,
}

/// Network service managing all P2P connections.
pub struct NetworkService {
    /// Configuration.
    config: NetworkConfig,
    /// Peer manager for tracking peers.
    peer_manager: Arc<PeerManager>,
    /// Active peer connections.
    peers: Arc<RwLock<HashMap<PeerId, PeerHandle>>>,
    /// Event sender.
    event_tx: mpsc::Sender<NetworkEvent>,
    /// Command receiver.
    command_rx: Option<mpsc::Receiver<NetworkCommand>>,
    /// Command sender (for cloning).
    command_tx: mpsc::Sender<NetworkCommand>,
}

impl NetworkService {
    /// Create a new network service.
    pub fn new(
        config: NetworkConfig,
        peer_manager: Arc<PeerManager>,
    ) -> (
        Self,
        mpsc::Receiver<NetworkEvent>,
        mpsc::Sender<NetworkCommand>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(1000);
        let (command_tx, command_rx) = mpsc::channel(100);

        let service = Self {
            config,
            peer_manager,
            peers: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            command_rx: Some(command_rx),
            command_tx: command_tx.clone(),
        };

        (service, event_rx, command_tx)
    }

    /// Run the network service.
    pub async fn run(mut self) -> NetworkResult<()> {
        // Bind listener
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        info!(addr = %self.config.listen_addr, "Network service listening");

        let mut command_rx = self.command_rx.take().unwrap();

        loop {
            tokio::select! {
                // Accept incoming connections
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            self.handle_incoming(stream, addr).await;
                        }
                        Err(e) => {
                            error!("Accept error: {}", e);
                        }
                    }
                }

                // Handle commands
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        NetworkCommand::Connect { addr } => {
                            self.connect_to_peer(addr).await;
                        }
                        NetworkCommand::Disconnect { peer_id } => {
                            self.disconnect_peer(&peer_id).await;
                        }
                        NetworkCommand::SendMessage { peer_id, message } => {
                            self.send_to_peer(&peer_id, message).await;
                        }
                        NetworkCommand::Broadcast { message } => {
                            self.broadcast(message).await;
                        }
                        NetworkCommand::Shutdown => {
                            info!("Network service shutting down");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle an incoming connection.
    async fn handle_incoming(&self, stream: TcpStream, addr: SocketAddr) {
        let peer_count = self.peers.read().len();
        if peer_count >= self.config.max_connections {
            warn!(addr = %addr, "Max connections reached, rejecting");
            return;
        }

        info!(addr = %addr, "Incoming connection");

        let config = self.config.clone();
        let peers = self.peers.clone();
        let event_tx = self.event_tx.clone();
        let peer_manager = self.peer_manager.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_peer_connection(stream, addr, config, peers, event_tx, peer_manager, false)
                    .await
            {
                warn!(addr = %addr, error = %e, "Connection failed");
            }
        });
    }

    /// Connect to a peer.
    async fn connect_to_peer(&self, addr: SocketAddr) {
        let peer_count = self.peers.read().len();
        if peer_count >= self.config.max_connections {
            warn!("Max connections reached, not connecting to {}", addr);
            return;
        }

        info!(addr = %addr, "Connecting to peer");

        let config = self.config.clone();
        let peers = self.peers.clone();
        let event_tx = self.event_tx.clone();
        let peer_manager = self.peer_manager.clone();

        tokio::spawn(async move {
            match TcpStream::connect(addr).await {
                Ok(stream) => {
                    if let Err(e) = handle_peer_connection(
                        stream,
                        addr,
                        config,
                        peers,
                        event_tx.clone(),
                        peer_manager,
                        true,
                    )
                    .await
                    {
                        warn!(addr = %addr, error = %e, "Outgoing connection failed");
                        // Send ConnectionFailed event so reconnection logic can track it
                        let _ = event_tx
                            .send(NetworkEvent::ConnectionFailed {
                                addr,
                                error: e.to_string(),
                            })
                            .await;
                    }
                }
                Err(e) => {
                    let _ = event_tx
                        .send(NetworkEvent::ConnectionFailed {
                            addr,
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    /// Disconnect a peer.
    async fn disconnect_peer(&self, peer_id: &PeerId) {
        if let Some(handle) = self.peers.write().remove(peer_id) {
            drop(handle.tx); // Dropping the sender will cause the peer task to exit
            info!(peer = %peer_id, "Disconnected peer");
        }
    }

    /// Send a message to a specific peer.
    async fn send_to_peer(&self, peer_id: &PeerId, message: Message) {
        // Clone the sender before releasing the lock to avoid holding it across await
        let tx = self.peers.read().get(peer_id).map(|h| h.tx.clone());
        if let Some(tx) = tx {
            debug!(peer = %peer_id, msg = ?message.message_type(), "Sending message to peer");
            if let Err(e) = tx.send(message).await {
                warn!(peer = %peer_id, error = %e, "Failed to send message");
            }
        } else {
            warn!(peer = %peer_id, "Cannot send message - peer not found");
        }
    }

    /// Broadcast a message to all connected peers.
    async fn broadcast(&self, message: Message) {
        let peers: Vec<_> = self.peers.read().keys().cloned().collect();
        for peer_id in peers {
            self.send_to_peer(&peer_id, message.clone()).await;
        }
    }

    /// Get connected peer count.
    pub fn peer_count(&self) -> usize {
        self.peers.read().len()
    }

    /// Get a command sender.
    pub fn command_sender(&self) -> mpsc::Sender<NetworkCommand> {
        self.command_tx.clone()
    }
}

/// Handle a peer connection (both incoming and outgoing).
async fn handle_peer_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    config: NetworkConfig,
    peers: Arc<RwLock<HashMap<PeerId, PeerHandle>>>,
    event_tx: mpsc::Sender<NetworkEvent>,
    peer_manager: Arc<PeerManager>,
    is_outgoing: bool,
) -> NetworkResult<()> {
    // Perform raw handshake (no framing)
    let session_id: u64 = rand::random();
    let our_handshake = HandshakeData::new(
        config.agent_name.clone(),
        PROTOCOL_VERSION,
        config.node_name.clone(),
    )
    .with_mainnet_features(session_id);

    let their_handshake = if is_outgoing {
        // Outgoing: we send first, then receive
        let handshake_bytes = our_handshake.serialize();
        debug!(
            addr = %addr,
            bytes = handshake_bytes.len(),
            hex = %hex::encode(&handshake_bytes),
            "Sending handshake"
        );
        stream
            .write_all(&handshake_bytes)
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::new(e.kind(), e.to_string())))?;
        stream
            .flush()
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::new(e.kind(), e.to_string())))?;

        // Read their handshake
        read_handshake(&mut stream).await?
    } else {
        // Incoming: we receive first, then send
        let their = read_handshake(&mut stream).await?;

        let handshake_bytes = our_handshake.serialize();
        stream
            .write_all(&handshake_bytes)
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::new(e.kind(), e.to_string())))?;
        stream
            .flush()
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::new(e.kind(), e.to_string())))?;

        their
    };

    info!(
        addr = %addr,
        agent = %their_handshake.agent_name,
        version = ?their_handshake.version,
        peer_name = %their_handshake.peer_name,
        "Handshake complete"
    );

    // Now switch to framed protocol for subsequent messages
    let codec = MessageCodec::with_magic(config.magic);
    let framed = Framed::new(stream, codec);

    // Convert HandshakeData to the Handshake type used in events
    let handshake_for_event = Handshake::new(
        their_handshake.agent_name.clone(),
        their_handshake.version,
        their_handshake.peer_name.clone(),
    );

    // Create peer ID and handle
    let peer_id = PeerId::from_addr(&addr);
    let (tx, mut rx) = mpsc::channel::<Message>(100);

    let peer_info = PeerInfo::new(addr, is_outgoing);
    peers.write().insert(
        peer_id.clone(),
        PeerHandle {
            tx,
            info: peer_info.clone(),
        },
    );

    // Update peer manager - add peer and mark as connected
    peer_manager.add_peer(peer_info);
    peer_manager.set_state(&peer_id, PeerState::Connected);

    // Emit connected event
    let _ = event_tx
        .send(NetworkEvent::PeerConnected {
            peer_id: peer_id.clone(),
            addr,
            handshake: handshake_for_event,
        })
        .await;

    // Split framed stream
    let (mut sink, mut stream) = framed.split();

    // Handle messages
    loop {
        tokio::select! {
            // Incoming messages from peer
            result = stream.next() => {
                match result {
                    Some(Ok(message)) => {
                        debug!(peer = %peer_id, msg = ?message.message_type(), "Received message");
                        let _ = event_tx.send(NetworkEvent::MessageReceived {
                            peer_id: peer_id.clone(),
                            message,
                        }).await;
                    }
                    Some(Err(e)) => {
                        warn!(peer = %peer_id, error = %e, "Receive error");
                        break;
                    }
                    None => {
                        debug!(peer = %peer_id, "Connection closed by peer");
                        break;
                    }
                }
            }

            // Outgoing messages to peer
            Some(message) = rx.recv() => {
                if let Err(e) = sink.send(message).await {
                    warn!(peer = %peer_id, error = %e, "Send error");
                    break;
                }
            }
        }
    }

    // Clean up
    peers.write().remove(&peer_id);

    // Update peer manager - mark as disconnected
    peer_manager.set_state(&peer_id, PeerState::Disconnected);

    let _ = event_tx
        .send(NetworkEvent::PeerDisconnected {
            peer_id: peer_id.clone(),
        })
        .await;

    Ok(())
}

/// Read a raw handshake from a stream.
async fn read_handshake(stream: &mut TcpStream) -> NetworkResult<HandshakeData> {
    let mut buffer = Vec::with_capacity(1024);
    let mut temp = [0u8; 256];

    // Read data incrementally until we can parse a complete handshake
    loop {
        // Check size limit
        if buffer.len() > 8096 {
            return Err(NetworkError::InvalidMessage("Handshake too large".into()));
        }

        // Try to parse what we have so far
        if buffer.len() > 10 {
            // Need at least some bytes for timestamp + agent
            match HandshakeData::parse(&buffer) {
                Ok(handshake) => {
                    return Ok(handshake);
                }
                Err(_) => {
                    // Need more data, continue reading
                }
            }
        }

        // Set a read timeout
        let read_result =
            tokio::time::timeout(std::time::Duration::from_secs(30), stream.read(&mut temp)).await;

        match read_result {
            Ok(Ok(0)) => {
                return Err(NetworkError::ConnectionClosed);
            }
            Ok(Ok(n)) => {
                buffer.extend_from_slice(&temp[..n]);
            }
            Ok(Err(e)) => {
                return Err(NetworkError::Io(std::io::Error::new(
                    e.kind(),
                    e.to_string(),
                )));
            }
            Err(_) => {
                return Err(NetworkError::HandshakeFailed("Handshake timeout".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_config_default() {
        let config = NetworkConfig::default();
        assert_eq!(config.max_connections, 50);
        assert_eq!(config.magic, MAINNET_MAGIC);
    }
}
