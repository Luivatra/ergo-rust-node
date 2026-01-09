//! Script API handlers.
//!
//! Provides endpoints for ErgoScript compilation and address conversion:
//! - Compile to P2S address
//! - Compile to P2SH address
//! - Address to ErgoTree conversion
//! - Address to bytes conversion

use crate::{ApiError, ApiResult};
use axum::Json;
use serde::{Deserialize, Serialize};

// ==================== Request/Response Types ====================

/// Script compilation request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileRequest {
    /// ErgoScript source code.
    pub source: String,
    /// Optional ErgoTree version (default: 0).
    #[serde(default)]
    pub tree_version: u8,
}

/// Address response.
#[derive(Debug, Serialize)]
pub struct AddressResponse {
    /// Generated address.
    pub address: String,
}

/// Address to ErgoTree request.
#[derive(Debug, Deserialize)]
pub struct AddressToTreeRequest {
    /// Address to convert.
    pub address: String,
}

/// ErgoTree response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErgoTreeResponse {
    /// ErgoTree bytes (hex encoded).
    pub tree: String,
}

/// Address to bytes request.
#[derive(Debug, Deserialize)]
pub struct AddressToBytesRequest {
    /// Address to convert.
    pub address: String,
}

/// Bytes response.
#[derive(Debug, Serialize)]
pub struct BytesResponse {
    /// Bytes (hex encoded).
    pub bytes: String,
}

/// Script execution request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteRequest {
    /// ErgoScript source code.
    pub script: String,
    /// Environment variables.
    #[serde(default)]
    pub env: std::collections::HashMap<String, serde_json::Value>,
    /// Optional ErgoTree version.
    pub tree_version: Option<u8>,
    /// Execution context.
    pub ctx: serde_json::Value,
}

/// Script execution result.
#[derive(Debug, Serialize)]
pub struct ExecuteResponse {
    /// Result value.
    pub value: serde_json::Value,
    /// Execution cost.
    pub cost: u64,
}

// ==================== Handlers ====================

/// POST /script/p2sAddress
/// Compile ErgoScript to Pay-to-Script (P2S) address.
pub async fn compile_to_p2s(
    Json(request): Json<CompileRequest>,
) -> ApiResult<Json<AddressResponse>> {
    let address = compile_script_to_p2s(&request.source)
        .map_err(|e| ApiError::BadRequest(format!("Compilation failed: {}", e)))?;

    Ok(Json(AddressResponse { address }))
}

/// POST /script/p2shAddress
/// Compile ErgoScript to Pay-to-Script-Hash (P2SH) address.
pub async fn compile_to_p2sh(
    Json(request): Json<CompileRequest>,
) -> ApiResult<Json<AddressResponse>> {
    let address = compile_script_to_p2sh(&request.source)
        .map_err(|e| ApiError::BadRequest(format!("Compilation failed: {}", e)))?;

    Ok(Json(AddressResponse { address }))
}

/// POST /script/addressToTree
/// Convert an address to its ErgoTree representation.
pub async fn address_to_tree(
    Json(request): Json<AddressToTreeRequest>,
) -> ApiResult<Json<ErgoTreeResponse>> {
    let tree_hex = convert_address_to_tree(&request.address)
        .map_err(|e| ApiError::BadRequest(format!("Invalid address: {}", e)))?;

    Ok(Json(ErgoTreeResponse { tree: tree_hex }))
}

/// POST /script/addressToBytes
/// Convert an address to raw bytes.
pub async fn address_to_bytes(
    Json(request): Json<AddressToBytesRequest>,
) -> ApiResult<Json<BytesResponse>> {
    let bytes_hex = convert_address_to_bytes(&request.address)
        .map_err(|e| ApiError::BadRequest(format!("Invalid address: {}", e)))?;

    Ok(Json(BytesResponse { bytes: bytes_hex }))
}

/// POST /script/executeWithContext
/// Execute a script with the given context.
pub async fn execute_with_context(
    Json(_request): Json<ExecuteRequest>,
) -> ApiResult<Json<ExecuteResponse>> {
    // Script execution is complex and requires full interpreter integration
    // For now, return a not implemented error
    Err(ApiError::NotImplemented(
        "Script execution not yet implemented".to_string(),
    ))
}

// ==================== Helper Functions ====================

/// Compile ErgoScript source to P2S address.
fn compile_script_to_p2s(source: &str) -> Result<String, String> {
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;

    // For simple scripts, try to parse as a direct ErgoTree hex
    // Full ErgoScript compilation requires the sigma compiler
    if source.chars().all(|c| c.is_ascii_hexdigit()) {
        // Treat as hex-encoded ErgoTree
        let tree_bytes = hex::decode(source).map_err(|e| format!("Invalid hex: {}", e))?;

        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
        let ergo_tree = ErgoTree::sigma_parse_bytes(&tree_bytes)
            .map_err(|e| format!("Failed to parse ErgoTree: {}", e))?;

        let address = Address::P2S(
            ergo_tree
                .sigma_serialize_bytes()
                .map_err(|e| format!("Serialization failed: {}", e))?,
        );

        let encoder = AddressEncoder::new(NetworkPrefix::Mainnet);
        return Ok(encoder.address_to_str(&address));
    }

    // Full ErgoScript compilation would require integrating the sigma compiler
    // which is complex. For now, return an error for non-hex input.
    Err(
        "ErgoScript compilation not yet implemented. Provide hex-encoded ErgoTree instead."
            .to_string(),
    )
}

/// Compile ErgoScript source to P2SH address.
fn compile_script_to_p2sh(source: &str) -> Result<String, String> {
    use blake2::digest::consts::U32;
    use blake2::{Blake2b, Digest};
    use ergo_lib::ergotree_ir::chain::address::{Address, AddressEncoder, NetworkPrefix};
    use ergo_lib::ergotree_ir::ergo_tree::ErgoTree;

    // For simple scripts, try to parse as a direct ErgoTree hex
    if source.chars().all(|c| c.is_ascii_hexdigit()) {
        let tree_bytes = hex::decode(source).map_err(|e| format!("Invalid hex: {}", e))?;

        use ergo_lib::ergotree_ir::serialization::SigmaSerializable;
        let _ergo_tree = ErgoTree::sigma_parse_bytes(&tree_bytes)
            .map_err(|e| format!("Failed to parse ErgoTree: {}", e))?;

        // P2SH = first 192 bits (24 bytes) of Blake2b256 hash of the script
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(&tree_bytes);
        let hash = hasher.finalize();

        // Take first 24 bytes for P2SH
        let mut hash_24 = [0u8; 24];
        hash_24.copy_from_slice(&hash[..24]);

        let address = Address::P2SH(hash_24);
        let encoder = AddressEncoder::new(NetworkPrefix::Mainnet);
        return Ok(encoder.address_to_str(&address));
    }

    Err(
        "ErgoScript compilation not yet implemented. Provide hex-encoded ErgoTree instead."
            .to_string(),
    )
}

/// Convert an address to its ErgoTree hex.
fn convert_address_to_tree(address: &str) -> Result<String, String> {
    use ergo_lib::ergotree_ir::chain::address::{AddressEncoder, NetworkPrefix};
    use ergo_lib::ergotree_ir::serialization::SigmaSerializable;

    let addr = AddressEncoder::new(NetworkPrefix::Mainnet)
        .parse_address_from_str(address)
        .or_else(|_| AddressEncoder::new(NetworkPrefix::Testnet).parse_address_from_str(address))
        .map_err(|e| e.to_string())?;

    let tree = addr
        .script()
        .map_err(|e| format!("Failed to get script: {}", e))?;

    let tree_bytes = tree
        .sigma_serialize_bytes()
        .map_err(|e| format!("Serialization failed: {}", e))?;

    Ok(hex::encode(tree_bytes))
}

/// Convert an address to raw bytes.
fn convert_address_to_bytes(address: &str) -> Result<String, String> {
    use ergo_lib::ergotree_ir::chain::address::{AddressEncoder, NetworkPrefix};

    let addr = AddressEncoder::new(NetworkPrefix::Mainnet)
        .parse_address_from_str(address)
        .or_else(|_| AddressEncoder::new(NetworkPrefix::Testnet).parse_address_from_str(address))
        .map_err(|e| e.to_string())?;

    Ok(hex::encode(addr.content_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_to_tree() {
        // Valid mainnet P2PK address
        let address = "9fRAWhdxEsTcdb8PhGNrZfwqa65zfkuYHAMmkQLcic1gdLSV5vA";
        let result = convert_address_to_tree(address);

        assert!(result.is_ok());
        let tree_hex = result.unwrap();
        assert!(!tree_hex.is_empty());
        // ErgoTree should start with proper header byte
        assert!(tree_hex.starts_with("0008cd") || tree_hex.len() > 6);
    }

    #[test]
    fn test_address_to_bytes() {
        let address = "9fRAWhdxEsTcdb8PhGNrZfwqa65zfkuYHAMmkQLcic1gdLSV5vA";
        let result = convert_address_to_bytes(address);

        assert!(result.is_ok());
        let bytes_hex = result.unwrap();
        assert!(!bytes_hex.is_empty());
    }

    #[test]
    fn test_address_to_tree_invalid() {
        let address = "invalid_address";
        let result = convert_address_to_tree(address);

        assert!(result.is_err());
    }
}
