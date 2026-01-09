//! P2P protocol messages.

use crate::handshake::PeerSpec;
use crate::{NetworkError, NetworkResult};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// VLQ decode an unsigned integer.
fn vlq_decode(data: &[u8], mut pos: usize) -> NetworkResult<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;

    loop {
        if pos >= data.len() {
            return Err(NetworkError::InvalidMessage("Truncated VLQ".into()));
        }
        let byte = data[pos];
        pos += 1;

        result |= ((byte & 0x7F) as u64) << shift;

        if (byte & 0x80) == 0 {
            break;
        }
        shift += 7;

        if shift > 63 {
            return Err(NetworkError::InvalidMessage("VLQ overflow".into()));
        }
    }

    Ok((result, pos))
}

/// VLQ encode an unsigned integer.
fn vlq_encode(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Calculate VLQ byte length for a value.
fn vlq_byte_len(value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    let mut v = value;
    let mut len = 0;
    while v > 0 {
        len += 1;
        v >>= 7;
    }
    len
}

/// Message type identifiers.
/// These must match the Scala node's message codes in BasicMessagesRepo.scala
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Get peers request (GetPeersSpec.messageCode = 1).
    GetPeers = 1,
    /// Peers response (PeersSpec.messageCode = 2).
    Peers = 2,
    /// Handshake message (handled separately, but we include it for completeness).
    Handshake = 75,
    /// Sync info.
    SyncInfo = 65,
    /// Inventory announcement.
    Inv = 55,
    /// Request modifiers.
    RequestModifier = 22,
    /// Modifier data.
    Modifier = 33,
}

impl TryFrom<u8> for MessageType {
    type Error = NetworkError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(MessageType::GetPeers),
            2 => Ok(MessageType::Peers),
            75 => Ok(MessageType::Handshake),
            65 => Ok(MessageType::SyncInfo),
            55 => Ok(MessageType::Inv),
            22 => Ok(MessageType::RequestModifier),
            33 => Ok(MessageType::Modifier),
            _ => Err(NetworkError::InvalidMessage(format!(
                "Unknown message type: {}",
                value
            ))),
        }
    }
}

/// Handshake message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    /// Agent name.
    pub agent_name: String,
    /// Protocol version.
    pub version: (u8, u8, u8),
    /// Node name.
    pub node_name: String,
    /// Declared address (optional).
    pub declared_addr: Option<String>,
    /// Peer features.
    pub features: Vec<u8>,
}

impl Handshake {
    /// Create a new handshake.
    pub fn new(agent_name: String, version: (u8, u8, u8), node_name: String) -> Self {
        Self {
            agent_name,
            version,
            node_name,
            declared_addr: None,
            features: Vec::new(),
        }
    }
}

/// Sync info message.
///
/// V1 format: 2-byte count + list of 32-byte header IDs
/// V2 format: 0x0000 + 0xFF marker + 1-byte count + headers with length prefix
#[derive(Debug, Clone)]
pub struct SyncInfo {
    /// Last header IDs (from best to older) - V1 format.
    pub last_header_ids: Vec<Vec<u8>>,
    /// Last headers (serialized) - V2 format. If non-empty, V2 format is used.
    pub last_headers: Vec<Vec<u8>>,
}

impl SyncInfo {
    /// Create an empty SyncInfo (for initial sync).
    pub fn empty() -> Self {
        Self {
            last_header_ids: Vec::new(),
            last_headers: Vec::new(),
        }
    }

    /// Create a V1 SyncInfo with header IDs.
    pub fn v1(header_ids: Vec<Vec<u8>>) -> Self {
        Self {
            last_header_ids: header_ids,
            last_headers: Vec::new(),
        }
    }

    /// Create a V2 SyncInfo with serialized headers.
    pub fn v2(headers: Vec<Vec<u8>>) -> Self {
        Self {
            last_header_ids: Vec::new(),
            last_headers: headers,
        }
    }

    /// Check if this is a V2 format message.
    pub fn is_v2(&self) -> bool {
        !self.last_headers.is_empty()
    }

    /// Serialize SyncInfo to bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        if self.is_v2() {
            // V2 format: VLQ(0) + 0xFF + UByte count + headers with VLQ length prefix
            vlq_encode(&mut buf, 0); // Zero count marker (VLQ encoded)
            buf.push(0xFF); // V2 marker byte (-1 as signed byte)
            buf.push(self.last_headers.len() as u8);
            for header in &self.last_headers {
                vlq_encode(&mut buf, header.len() as u64);
                buf.extend_from_slice(header);
            }
        } else {
            // V1 format: VLQ-encoded count + 32-byte header IDs
            // The count is encoded using VLQ (variable-length quantity), not fixed 2-byte
            vlq_encode(&mut buf, self.last_header_ids.len() as u64);
            for id in &self.last_header_ids {
                buf.extend_from_slice(id);
            }
        }

        buf
    }

    /// Parse SyncInfo from bytes.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.is_empty() {
            return Ok(Self::empty());
        }

        // Count is VLQ encoded
        let (count, mut pos) =
            vlq_decode(data, 0).map_err(|e| format!("Failed to decode count: {:?}", e))?;

        if count == 0 && pos < data.len() && data[pos] == 0xFF {
            // V2 format
            pos += 1; // Skip 0xFF marker
            if pos >= data.len() {
                return Ok(Self::empty());
            }
            let header_count = data[pos] as usize;
            pos += 1;
            let mut headers = Vec::with_capacity(header_count);

            for _ in 0..header_count {
                if pos >= data.len() {
                    break;
                }
                let (len, vlq_len) = vlq_decode(data, pos)
                    .map_err(|e| format!("Failed to decode header length: {:?}", e))?;
                pos = vlq_len;
                let len = len as usize;
                if pos + len > data.len() {
                    break;
                }
                headers.push(data[pos..pos + len].to_vec());
                pos += len;
            }

            Ok(Self::v2(headers))
        } else {
            // V1 format
            let count = count as usize;
            let mut header_ids = Vec::with_capacity(count);

            for _ in 0..count {
                if pos + 32 > data.len() {
                    break;
                }
                header_ids.push(data[pos..pos + 32].to_vec());
                pos += 32;
            }

            Ok(Self::v1(header_ids))
        }
    }
}

/// Inventory item.
#[derive(Debug, Clone)]
pub struct InvData {
    /// Modifier type.
    pub type_id: u8,
    /// Modifier IDs.
    pub ids: Vec<Vec<u8>>,
}

/// Modifier request.
#[derive(Debug, Clone)]
pub struct ModifierRequest {
    /// Modifier type.
    pub type_id: u8,
    /// Modifier IDs to request.
    pub ids: Vec<Vec<u8>>,
}

/// Single modifier item.
#[derive(Debug, Clone)]
pub struct ModifierItem {
    /// Modifier ID.
    pub id: Vec<u8>,
    /// Modifier bytes.
    pub data: Vec<u8>,
}

/// Modifier data (contains multiple modifiers of the same type).
#[derive(Debug, Clone)]
pub struct ModifierData {
    /// Modifier type.
    pub type_id: u8,
    /// List of modifiers.
    pub modifiers: Vec<ModifierItem>,
}

/// P2P message.
#[derive(Debug, Clone)]
pub enum Message {
    /// Handshake.
    Handshake(Handshake),
    /// Get peers request.
    GetPeers,
    /// Peers response with peer specifications.
    Peers(Vec<PeerSpec>),
    /// Sync info.
    SyncInfo(SyncInfo),
    /// Inventory.
    Inv(InvData),
    /// Request modifier.
    RequestModifier(ModifierRequest),
    /// Modifier data.
    Modifier(ModifierData),
}

impl Message {
    /// Get the message type.
    pub fn message_type(&self) -> MessageType {
        match self {
            Message::Handshake(_) => MessageType::Handshake,
            Message::GetPeers => MessageType::GetPeers,
            Message::Peers(_) => MessageType::Peers,
            Message::SyncInfo(_) => MessageType::SyncInfo,
            Message::Inv(_) => MessageType::Inv,
            Message::RequestModifier(_) => MessageType::RequestModifier,
            Message::Modifier(_) => MessageType::Modifier,
        }
    }

    /// Encode the message to bytes.
    pub fn encode(&self) -> NetworkResult<Bytes> {
        let mut buf = BytesMut::new();

        // Message type
        buf.put_u8(self.message_type() as u8);

        // Message body (simplified - real impl needs proper serialization)
        match self {
            Message::Handshake(h) => {
                let json = serde_json::to_vec(h)
                    .map_err(|e| NetworkError::Serialization(e.to_string()))?;
                buf.put_u32(json.len() as u32);
                buf.extend_from_slice(&json);
            }
            Message::GetPeers => {
                // Empty body - just the message type byte (already added above)
            }
            Message::Peers(peers) => {
                // Serialize peers using VLQ count + serialized PeerSpec for each
                let mut body = Vec::new();
                vlq_encode(&mut body, peers.len() as u64);
                for peer in peers {
                    let peer_bytes = peer.serialize();
                    body.extend_from_slice(&peer_bytes);
                }
                buf.extend_from_slice(&body);
            }
            Message::SyncInfo(info) => {
                let serialized = info.serialize();
                buf.extend_from_slice(&serialized);
            }
            Message::Inv(inv) => {
                buf.put_u8(inv.type_id);
                let mut count_buf = Vec::new();
                vlq_encode(&mut count_buf, inv.ids.len() as u64);
                buf.extend_from_slice(&count_buf);
                for id in &inv.ids {
                    buf.extend_from_slice(id);
                }
            }
            Message::RequestModifier(req) => {
                buf.put_u8(req.type_id);
                let mut count_buf = Vec::new();
                vlq_encode(&mut count_buf, req.ids.len() as u64);
                buf.extend_from_slice(&count_buf);
                for id in &req.ids {
                    buf.extend_from_slice(id);
                }
            }
            Message::Modifier(m) => {
                buf.put_u8(m.type_id);
                // Count of modifiers (VLQ)
                let mut count_buf = Vec::new();
                vlq_encode(&mut count_buf, m.modifiers.len() as u64);
                buf.extend_from_slice(&count_buf);
                // Each modifier: id (32 bytes) + data_len (VLQ) + data
                for item in &m.modifiers {
                    buf.extend_from_slice(&item.id);
                    let mut len_buf = Vec::new();
                    vlq_encode(&mut len_buf, item.data.len() as u64);
                    buf.extend_from_slice(&len_buf);
                    buf.extend_from_slice(&item.data);
                }
            }
        }

        Ok(buf.freeze())
    }

    /// Decode a message from bytes.
    pub fn decode(mut bytes: Bytes) -> NetworkResult<Self> {
        if bytes.is_empty() {
            return Err(NetworkError::InvalidMessage("Empty message".to_string()));
        }

        let msg_type = MessageType::try_from(bytes.get_u8())?;

        match msg_type {
            MessageType::Handshake => {
                let len = bytes.get_u32() as usize;
                if bytes.remaining() < len {
                    return Err(NetworkError::InvalidMessage(
                        "Truncated handshake".to_string(),
                    ));
                }
                let json = bytes.copy_to_bytes(len);
                let handshake: Handshake = serde_json::from_slice(&json)
                    .map_err(|e| NetworkError::Serialization(e.to_string()))?;
                Ok(Message::Handshake(handshake))
            }
            MessageType::GetPeers => Ok(Message::GetPeers),
            MessageType::Peers => {
                // Parse VLQ count + PeerSpec for each peer
                let data = bytes.to_vec();
                let (count, mut pos) = vlq_decode(&data, 0)?;
                let mut peers = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (peer_spec, new_pos) = PeerSpec::parse(&data, pos)?;
                    peers.push(peer_spec);
                    pos = new_pos;
                }
                Ok(Message::Peers(peers))
            }
            MessageType::SyncInfo => {
                let sync_info =
                    SyncInfo::parse(&bytes[..]).map_err(|e| NetworkError::InvalidMessage(e))?;
                Ok(Message::SyncInfo(sync_info))
            }
            MessageType::Inv => {
                let type_id = bytes.get_u8();
                let (count, _) = vlq_decode(&bytes[..], 0)?;
                // Skip the VLQ bytes
                let vlq_len = vlq_byte_len(count);
                bytes.advance(vlq_len);
                let mut ids = Vec::with_capacity(count as usize);
                // Read as many IDs as we have data for (handle partial messages)
                while bytes.remaining() >= 32 && ids.len() < count as usize {
                    ids.push(bytes.copy_to_bytes(32).to_vec());
                }
                Ok(Message::Inv(InvData { type_id, ids }))
            }
            MessageType::RequestModifier => {
                let type_id = bytes.get_u8();
                let (count, _) = vlq_decode(&bytes[..], 0)?;
                let vlq_len = vlq_byte_len(count);
                bytes.advance(vlq_len);
                let mut ids = Vec::with_capacity(count as usize);
                while bytes.remaining() >= 32 && ids.len() < count as usize {
                    ids.push(bytes.copy_to_bytes(32).to_vec());
                }
                Ok(Message::RequestModifier(ModifierRequest { type_id, ids }))
            }
            MessageType::Modifier => {
                let type_id = bytes.get_u8();
                // Count of modifiers (VLQ)
                let (count, vlq_len) = vlq_decode(&bytes[..], 0)?;
                bytes.advance(vlq_len);

                let mut modifiers = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    if bytes.remaining() < 32 {
                        return Err(NetworkError::InvalidMessage("Modifier ID truncated".into()));
                    }
                    let id = bytes.copy_to_bytes(32).to_vec();

                    // Data length is VLQ encoded
                    let (len, vlq_len) = vlq_decode(&bytes[..], 0)?;
                    bytes.advance(vlq_len);

                    if bytes.remaining() < len as usize {
                        return Err(NetworkError::InvalidMessage(format!(
                            "Modifier data truncated: expected {}, got {}",
                            len,
                            bytes.remaining()
                        )));
                    }
                    let data = bytes.copy_to_bytes(len as usize).to_vec();
                    modifiers.push(ModifierItem { id, data });
                }
                Ok(Message::Modifier(ModifierData { type_id, modifiers }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handshake_roundtrip() {
        let handshake = Handshake::new(
            "ergo-rust-node".to_string(),
            (5, 0, 0),
            "test-node".to_string(),
        );

        let msg = Message::Handshake(handshake);
        let encoded = msg.encode().unwrap();
        let decoded = Message::decode(encoded).unwrap();

        if let Message::Handshake(h) = decoded {
            assert_eq!(h.agent_name, "ergo-rust-node");
            assert_eq!(h.version, (5, 0, 0));
        } else {
            panic!("Wrong message type");
        }
    }

    #[test]
    fn test_get_peers_roundtrip() {
        let msg = Message::GetPeers;
        let encoded = msg.encode().unwrap();
        let decoded = Message::decode(encoded).unwrap();

        assert!(matches!(decoded, Message::GetPeers));
    }

    #[test]
    fn test_syncinfo_v1_roundtrip() {
        // Test SyncInfo V1 serialization roundtrip
        // Scala expects [oldest, ..., newest] order (last = best header)
        let ids = vec![
            vec![1u8; 32], // oldest
            vec![2u8; 32], // middle
            vec![3u8; 32], // newest (best)
        ];
        let sync_info = SyncInfo::v1(ids.clone());

        let serialized = sync_info.serialize();

        // VLQ encoding: count 3 = 0x03 (1 byte) + 3 * 32-byte IDs = 97 bytes
        assert_eq!(serialized.len(), 1 + 3 * 32);

        // Verify VLQ count encoding (3 fits in 1 byte)
        assert_eq!(serialized[0], 3);

        // Verify roundtrip
        let parsed = SyncInfo::parse(&serialized).unwrap();
        assert_eq!(parsed.last_header_ids.len(), 3);
        assert_eq!(parsed.last_header_ids[0], vec![1u8; 32]); // oldest first
        assert_eq!(parsed.last_header_ids[2], vec![3u8; 32]); // newest last
    }

    #[test]
    fn test_syncinfo_v1_header_order_convention() {
        // This test documents the expected header order convention:
        // - get_header_locator() returns [newest, ..., oldest]
        // - SyncInfo should contain [oldest, ..., newest] (last = best)
        // - So we REVERSE the locator when building SyncInfo

        // Simulate a locator from get_header_locator() (newest first)
        let locator = vec![
            vec![3u8; 32], // newest (height 3)
            vec![2u8; 32], // height 2
            vec![1u8; 32], // oldest (height 1)
        ];

        // Reverse for SyncInfo (Scala expects oldest first, newest last)
        let sync_headers: Vec<Vec<u8>> = locator.iter().rev().cloned().collect();

        // Verify order: oldest first, newest last
        assert_eq!(sync_headers[0], vec![1u8; 32]); // oldest
        assert_eq!(sync_headers[2], vec![3u8; 32]); // newest (best) - last position

        // Create SyncInfo and verify
        let sync_info = SyncInfo::v1(sync_headers.clone());
        assert!(!sync_info.is_v2());
        assert_eq!(sync_info.last_header_ids.len(), 3);

        // The LAST element should be the best/newest header
        // This is what Scala checks: si.lastHeaderIds.last shouldEqual chain.last.header.id
        assert_eq!(
            sync_info.last_header_ids.last().unwrap(),
            &vec![3u8; 32],
            "Last element should be the best/newest header"
        );
    }

    #[test]
    fn test_syncinfo_v1_vlq_encoding() {
        // Test that count is VLQ encoded (matching Scala's putUShort which uses VLQ)
        // VLQ encoding: values < 128 use 1 byte, >= 128 use 2 bytes

        // Test small count (1 byte VLQ)
        let small_ids: Vec<Vec<u8>> = (0..10).map(|i| vec![i; 32]).collect();
        let small_sync = SyncInfo::v1(small_ids);
        let small_serialized = small_sync.serialize();
        assert_eq!(small_serialized[0], 10); // count = 10, fits in 1 byte
        assert_eq!(small_serialized.len(), 1 + 10 * 32);

        // Test larger count that needs 2-byte VLQ (128 = 0x80 0x01 in VLQ)
        // For 128 IDs: VLQ = [0x80, 0x01] + 128 * 32 bytes
        // But we'll test with 127 (still 1 byte) and 128 (2 bytes)
        let ids_127: Vec<Vec<u8>> = (0..127).map(|i| vec![i as u8; 32]).collect();
        let sync_127 = SyncInfo::v1(ids_127);
        let serialized_127 = sync_127.serialize();
        assert_eq!(serialized_127[0], 127); // count = 127, still 1 byte
        assert_eq!(serialized_127.len(), 1 + 127 * 32);
    }
}
