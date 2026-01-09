//! Utility API handlers.
//!
//! Provides utility endpoints for:
//! - Random seed generation
//! - Blake2b hashing
//! - Address encoding/decoding
//! - Address validation

use crate::{ApiError, ApiResult};
use axum::{extract::Path, Json};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Default seed size in bytes.
const DEFAULT_SEED_SIZE: usize = 32;

/// Maximum seed size in bytes.
const MAX_SEED_SIZE: usize = 64;

// ==================== Request/Response Types ====================

/// Hash request body.
#[derive(Debug, Deserialize)]
pub struct HashRequest {
    /// Data to hash (hex encoded).
    pub data: String,
}

/// Hash response.
#[derive(Debug, Serialize)]
pub struct HashResponse {
    /// Blake2b-256 hash (hex encoded).
    pub hash: String,
}

/// Seed response.
#[derive(Debug, Serialize)]
pub struct SeedResponse {
    /// Random seed (hex encoded).
    pub seed: String,
}

/// Address validation response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressValidationResponse {
    /// The address that was validated.
    pub address: String,
    /// Whether the address is valid.
    pub is_valid: bool,
    /// Error message if invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Raw address response.
#[derive(Debug, Serialize)]
pub struct RawAddressResponse {
    /// Raw address bytes (hex encoded).
    pub raw: String,
}

/// Address from raw response.
#[derive(Debug, Serialize)]
pub struct AddressFromRawResponse {
    /// Encoded address.
    pub address: String,
}

/// ErgoTree to address response.
#[derive(Debug, Serialize)]
pub struct ErgoTreeToAddressResponse {
    /// Encoded address.
    pub address: String,
}

/// ErgoTree request body.
#[derive(Debug, Deserialize)]
pub struct ErgoTreeRequest {
    /// ErgoTree bytes (hex encoded).
    #[serde(rename = "ergoTree")]
    pub ergo_tree: String,
}

// ==================== Handlers ====================

/// GET /utils/seed
/// Generate a random seed of default size (32 bytes).
pub async fn get_seed() -> ApiResult<Json<SeedResponse>> {
    let seed = generate_seed(DEFAULT_SEED_SIZE);
    Ok(Json(SeedResponse {
        seed: hex::encode(seed),
    }))
}

/// GET /utils/seed/:length
/// Generate a random seed of specified size.
pub async fn get_seed_with_length(Path(length): Path<usize>) -> ApiResult<Json<SeedResponse>> {
    if length == 0 {
        return Err(ApiError::BadRequest(
            "Seed length must be positive".to_string(),
        ));
    }
    if length > MAX_SEED_SIZE {
        return Err(ApiError::BadRequest(format!(
            "Seed length must be at most {} bytes",
            MAX_SEED_SIZE
        )));
    }

    let seed = generate_seed(length);
    Ok(Json(SeedResponse {
        seed: hex::encode(seed),
    }))
}

/// POST /utils/hash/blake2b
/// Hash data using Blake2b-256.
pub async fn hash_blake2b(Json(request): Json<HashRequest>) -> ApiResult<Json<HashResponse>> {
    let data = hex::decode(&request.data)
        .map_err(|e| ApiError::BadRequest(format!("Invalid hex data: {}", e)))?;

    let hash = blake2b_256(&data);
    Ok(Json(HashResponse {
        hash: hex::encode(hash),
    }))
}

/// GET /utils/address/:address
/// Validate an Ergo address.
pub async fn validate_address(
    Path(address): Path<String>,
) -> ApiResult<Json<AddressValidationResponse>> {
    let validation = validate_ergo_address(&address);
    Ok(Json(validation))
}

/// POST /utils/address
/// Validate an Ergo address (POST version).
pub async fn validate_address_post(
    Json(address): Json<String>,
) -> ApiResult<Json<AddressValidationResponse>> {
    let validation = validate_ergo_address(&address);
    Ok(Json(validation))
}

/// GET /utils/addressToRaw/:address
/// Convert an address to raw bytes.
pub async fn address_to_raw(Path(address): Path<String>) -> ApiResult<Json<RawAddressResponse>> {
    // Decode the address to get raw bytes
    let raw_bytes = decode_address_to_raw(&address)
        .map_err(|e| ApiError::BadRequest(format!("Invalid address: {}", e)))?;

    Ok(Json(RawAddressResponse {
        raw: hex::encode(raw_bytes),
    }))
}

/// GET /utils/rawToAddress/:raw
/// Convert raw public key bytes to an address.
pub async fn raw_to_address(Path(raw): Path<String>) -> ApiResult<Json<AddressFromRawResponse>> {
    let raw_bytes =
        hex::decode(&raw).map_err(|e| ApiError::BadRequest(format!("Invalid hex: {}", e)))?;

    let address = encode_raw_to_address(&raw_bytes)
        .map_err(|e| ApiError::BadRequest(format!("Invalid raw data: {}", e)))?;

    Ok(Json(AddressFromRawResponse { address }))
}

/// GET /utils/ergoTreeToAddress/:ergoTree
/// Convert an ErgoTree to an address.
pub async fn ergo_tree_to_address(
    Path(ergo_tree_hex): Path<String>,
) -> ApiResult<Json<ErgoTreeToAddressResponse>> {
    let address = convert_ergo_tree_to_address(&ergo_tree_hex)
        .map_err(|e| ApiError::BadRequest(format!("Invalid ErgoTree: {}", e)))?;

    Ok(Json(ErgoTreeToAddressResponse { address }))
}

/// POST /utils/ergoTreeToAddress
/// Convert an ErgoTree to an address (POST version).
pub async fn ergo_tree_to_address_post(
    Json(request): Json<ErgoTreeRequest>,
) -> ApiResult<Json<ErgoTreeToAddressResponse>> {
    let address = convert_ergo_tree_to_address(&request.ergo_tree)
        .map_err(|e| ApiError::BadRequest(format!("Invalid ErgoTree: {}", e)))?;

    Ok(Json(ErgoTreeToAddressResponse { address }))
}

// ==================== Helper Functions ====================

/// Generate a random seed of the specified size.
fn generate_seed(size: usize) -> Vec<u8> {
    let mut seed = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut seed);
    seed
}

/// Compute Blake2b-256 hash.
fn blake2b_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&result);
    arr
}

/// Validate an Ergo address and return validation result.
fn validate_ergo_address(address: &str) -> AddressValidationResponse {
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};

    // Try mainnet first, then testnet
    let mainnet_result =
        AddressEncoder::new(NetworkPrefix::Mainnet).parse_address_from_str(address);
    let testnet_result =
        AddressEncoder::new(NetworkPrefix::Testnet).parse_address_from_str(address);

    match mainnet_result.or(testnet_result) {
        Ok(_) => AddressValidationResponse {
            address: address.to_string(),
            is_valid: true,
            error: None,
        },
        Err(e) => AddressValidationResponse {
            address: address.to_string(),
            is_valid: false,
            error: Some(e.to_string()),
        },
    }
}

/// Decode an address to raw content bytes.
fn decode_address_to_raw(address: &str) -> Result<Vec<u8>, String> {
    use ergo_lib::ergotree_ir::chain::address::{AddressEncoder, NetworkPrefix};

    // Try mainnet first, then testnet
    let addr = AddressEncoder::new(NetworkPrefix::Mainnet)
        .parse_address_from_str(address)
        .or_else(|_| AddressEncoder::new(NetworkPrefix::Testnet).parse_address_from_str(address))
        .map_err(|e| e.to_string())?;

    Ok(addr.content_bytes())
}

/// Encode raw public key bytes to a P2PK address.
fn encode_raw_to_address(raw: &[u8]) -> Result<String, String> {
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
    use ergo_lib::ergotree_ir::sigma_protocol::sigma_boolean::ProveDlog;

    // Parse as a group element (public key point)
    if raw.len() != 33 {
        return Err(format!(
            "Expected 33 bytes for compressed public key, got {}",
            raw.len()
        ));
    }

    // Try to parse as ProveDlog
    let prove_dlog = ProveDlog::sigma_parse_bytes(raw)
        .map_err(|e| format!("Failed to parse public key: {}", e))?;

    let address = Address::P2Pk(prove_dlog);
    let encoder = AddressEncoder::new(NetworkPrefix::Mainnet);
    Ok(encoder.address_to_str(&address))
}

/// Convert ErgoTree hex to address.
fn convert_ergo_tree_to_address(ergo_tree_hex: &str) -> Result<String, String> {
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    let tree_bytes = hex::decode(ergo_tree_hex).map_err(|e| format!("Invalid hex: {}", e))?;

    let ergo_tree = ErgoTree::sigma_parse_bytes(&tree_bytes)
        .map_err(|e| format!("Failed to parse ErgoTree: {}", e))?;

    let address = Address::recreate_from_ergo_tree(&ergo_tree)
        .map_err(|e| format!("Failed to create address from ErgoTree: {}", e))?;

    let encoder = AddressEncoder::new(NetworkPrefix::Mainnet);
    Ok(encoder.address_to_str(&address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_seed() {
        let seed1 = generate_seed(32);
        let seed2 = generate_seed(32);

        assert_eq!(seed1.len(), 32);
        assert_eq!(seed2.len(), 32);
        // Seeds should be different (with overwhelming probability)
        assert_ne!(seed1, seed2);
    }

    #[test]
    fn test_blake2b_256() {
        let data = b"hello world";
        let hash = blake2b_256(data);

        assert_eq!(hash.len(), 32);
        // Known hash for "hello world"
        let expected = "256c83b297114d201b30179f3f0ef0cace9783622da5974326b436178aeef610";
        assert_eq!(hex::encode(hash), expected);
    }

    #[test]
    fn test_validate_ergo_address_valid() {
        // Valid mainnet P2PK address
        let address = "9fRAWhdxEsTcdb8PhGNrZfwqa65zfkuYHAMmkQLcic1gdLSV5vA";
        let result = validate_ergo_address(address);

        assert!(result.is_valid);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_validate_ergo_address_invalid() {
        let address = "invalid_address";
        let result = validate_ergo_address(address);

        assert!(!result.is_valid);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_decode_address_to_raw() {
        let address = "9fRAWhdxEsTcdb8PhGNrZfwqa65zfkuYHAMmkQLcic1gdLSV5vA";
        let result = decode_address_to_raw(address);

        assert!(result.is_ok());
        let raw = result.unwrap();
        assert!(!raw.is_empty());
    }
}
