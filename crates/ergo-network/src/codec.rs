//! Ergo P2P protocol codec for message framing.
//!
//! The Ergo P2P protocol uses the following message format:
//!
//! ```text
//! +----------+----------+----------+----------+
//! |  Magic   |   Type   |  Length  | Checksum |
//! | 4 bytes  | 1 byte   | 4 bytes  | 4 bytes  |
//! +----------+----------+----------+----------+
//! |                 Payload                   |
//! |              (Length bytes)               |
//! +-------------------------------------------+
//! ```
//!
//! - Magic: Network identifier (mainnet/testnet)
//! - Type: Message type ID
//! - Length: Payload length in bytes (big-endian)
//! - Checksum: First 4 bytes of Blake2b256(payload)
//! - Payload: Message-specific data

use crate::{Message, NetworkError, NetworkResult, MAINNET_MAGIC, MAX_MESSAGE_SIZE};
use blake2::{Blake2b, Digest};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Base header size: magic (4) + type (1) + length (4) = 9 bytes
/// Checksum (4 bytes) is only included when payload length > 0
const BASE_HEADER_SIZE: usize = 9;
const CHECKSUM_SIZE: usize = 4;

/// Message codec for Ergo P2P protocol.
pub struct MessageCodec {
    /// Network magic bytes.
    magic: [u8; 4],
    /// Maximum allowed message size.
    max_size: usize,
}

impl MessageCodec {
    /// Create a new codec with mainnet magic.
    pub fn new() -> Self {
        Self {
            magic: MAINNET_MAGIC,
            max_size: MAX_MESSAGE_SIZE,
        }
    }

    /// Create a codec with custom magic bytes.
    pub fn with_magic(magic: [u8; 4]) -> Self {
        Self {
            magic,
            max_size: MAX_MESSAGE_SIZE,
        }
    }

    /// Calculate checksum for payload (first 4 bytes of Blake2b256).
    fn checksum(payload: &[u8]) -> [u8; 4] {
        let hash = Blake2b::<typenum::U32>::digest(payload);
        let mut checksum = [0u8; 4];
        checksum.copy_from_slice(&hash[0..4]);
        checksum
    }

    /// Verify checksum.
    fn verify_checksum(payload: &[u8], expected: &[u8; 4]) -> bool {
        let actual = Self::checksum(payload);
        actual == *expected
    }
}

impl Default for MessageCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = NetworkError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Need at least base header size
        if src.len() < BASE_HEADER_SIZE {
            return Ok(None);
        }

        // Parse header without consuming
        let magic = &src[0..4];
        if magic != self.magic {
            return Err(NetworkError::InvalidMessage(format!(
                "Invalid magic: expected {:?}, got {:?}",
                self.magic, magic
            )));
        }

        let msg_type = src[4];
        let length = u32::from_be_bytes([src[5], src[6], src[7], src[8]]) as usize;

        // Validate length
        if length > self.max_size {
            return Err(NetworkError::MessageTooLarge {
                size: length,
                max: self.max_size,
            });
        }

        // Calculate total size - checksum only present when length > 0
        let total_size = if length > 0 {
            BASE_HEADER_SIZE + CHECKSUM_SIZE + length
        } else {
            BASE_HEADER_SIZE
        };

        // Check if we have the full message
        if src.len() < total_size {
            // Reserve space for the full message
            src.reserve(total_size - src.len());
            return Ok(None);
        }

        // Consume base header
        src.advance(BASE_HEADER_SIZE);

        // Get payload (with checksum verification if length > 0)
        let payload = if length > 0 {
            let checksum: [u8; 4] = [src[0], src[1], src[2], src[3]];
            src.advance(CHECKSUM_SIZE);
            let payload = src.split_to(length).freeze();

            // Verify checksum
            if !Self::verify_checksum(&payload, &checksum) {
                return Err(NetworkError::InvalidMessage(
                    "Checksum mismatch".to_string(),
                ));
            }
            payload
        } else {
            Bytes::new()
        };

        // Decode message from payload
        let mut msg_bytes = BytesMut::new();
        msg_bytes.put_u8(msg_type);
        msg_bytes.extend_from_slice(&payload);

        Message::decode(msg_bytes.freeze()).map(Some)
    }
}

impl Encoder<Message> for MessageCodec {
    type Error = NetworkError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // Encode message body
        let encoded = item.encode()?;

        // The encoded message starts with the type byte
        if encoded.is_empty() {
            return Err(NetworkError::InvalidMessage(
                "Empty encoded message".to_string(),
            ));
        }

        let msg_type = encoded[0];
        let payload = &encoded[1..];
        let length = payload.len();

        // Validate length
        if length > self.max_size {
            return Err(NetworkError::MessageTooLarge {
                size: length,
                max: self.max_size,
            });
        }

        // Reserve space - checksum only when length > 0
        let total_size = if length > 0 {
            BASE_HEADER_SIZE + CHECKSUM_SIZE + length
        } else {
            BASE_HEADER_SIZE
        };
        dst.reserve(total_size);

        // Write base header
        dst.put_slice(&self.magic);
        dst.put_u8(msg_type);
        dst.put_u32(length as u32);

        // Write checksum and payload only if length > 0
        if length > 0 {
            let checksum = Self::checksum(payload);
            dst.put_slice(&checksum);
            dst.put_slice(payload);
        }

        Ok(())
    }
}

/// Feature flags for handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PeerFeature {
    /// Full node with UTXO state.
    FullNode = 0x01,
    /// Node supports state snapshots.
    StateSnapshot = 0x02,
    /// Node supports NiPoPoW proofs.
    NiPoPoW = 0x04,
    /// Node is an archive node (stores all blocks).
    Archive = 0x08,
}

impl PeerFeature {
    /// Check if a feature set contains this feature.
    pub fn is_set(&self, features: u8) -> bool {
        (features & (*self as u8)) != 0
    }

    /// Combine multiple features.
    pub fn combine(features: &[PeerFeature]) -> u8 {
        features.iter().fold(0u8, |acc, f| acc | (*f as u8))
    }
}

/// Network address with timestamp.
#[derive(Debug, Clone)]
pub struct PeerAddress {
    /// IP address and port as string.
    pub address: String,
    /// Last seen timestamp.
    pub last_seen: u64,
    /// Connection failures count.
    pub failures: u32,
}

impl PeerAddress {
    /// Create a new peer address.
    pub fn new(address: String) -> Self {
        Self {
            address,
            last_seen: 0,
            failures: 0,
        }
    }

    /// Serialize to bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        let addr_bytes = self.address.as_bytes();
        bytes.extend_from_slice(&(addr_bytes.len() as u16).to_be_bytes());
        bytes.extend_from_slice(addr_bytes);
        bytes.extend_from_slice(&self.last_seen.to_be_bytes());
        bytes.extend_from_slice(&self.failures.to_be_bytes());
        bytes
    }

    /// Deserialize from bytes.
    pub fn deserialize(bytes: &mut Bytes) -> NetworkResult<Self> {
        if bytes.remaining() < 2 {
            return Err(NetworkError::InvalidMessage(
                "PeerAddress too short".to_string(),
            ));
        }
        let addr_len = bytes.get_u16() as usize;
        if bytes.remaining() < addr_len + 12 {
            return Err(NetworkError::InvalidMessage(
                "PeerAddress truncated".to_string(),
            ));
        }
        let address = String::from_utf8_lossy(&bytes.copy_to_bytes(addr_len)).to_string();
        let last_seen = bytes.get_u64();
        let failures = bytes.get_u32();
        Ok(Self {
            address,
            last_seen,
            failures,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Handshake;

    #[test]
    fn test_checksum() {
        let payload = b"hello world";
        let checksum = MessageCodec::checksum(payload);
        assert_eq!(checksum.len(), 4);
        assert!(MessageCodec::verify_checksum(payload, &checksum));
    }

    #[test]
    fn test_codec_roundtrip() {
        let mut codec = MessageCodec::new();
        let handshake = Handshake::new(
            "ergo-rust-node".to_string(),
            (5, 0, 0),
            "test-node".to_string(),
        );
        let msg = Message::Handshake(handshake);

        // Encode
        let mut buf = BytesMut::new();
        codec.encode(msg, &mut buf).unwrap();

        // Decode
        let decoded = codec.decode(&mut buf).unwrap().unwrap();

        if let Message::Handshake(h) = decoded {
            assert_eq!(h.agent_name, "ergo-rust-node");
            assert_eq!(h.version, (5, 0, 0));
        } else {
            panic!("Wrong message type");
        }
    }

    #[test]
    fn test_peer_features() {
        let features = PeerFeature::combine(&[PeerFeature::FullNode, PeerFeature::Archive]);
        assert!(PeerFeature::FullNode.is_set(features));
        assert!(PeerFeature::Archive.is_set(features));
        assert!(!PeerFeature::NiPoPoW.is_set(features));
    }

    #[test]
    fn test_peer_address_roundtrip() {
        let addr = PeerAddress {
            address: "192.168.1.1:9030".to_string(),
            last_seen: 1699999999,
            failures: 2,
        };

        let serialized = addr.serialize();
        let mut bytes = Bytes::from(serialized);
        let deserialized = PeerAddress::deserialize(&mut bytes).unwrap();

        assert_eq!(deserialized.address, addr.address);
        assert_eq!(deserialized.last_seen, addr.last_seen);
        assert_eq!(deserialized.failures, addr.failures);
    }
}
