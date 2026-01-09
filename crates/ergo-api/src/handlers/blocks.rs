//! Block handlers.

use crate::{ApiError, ApiResult, AppState};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use ergo_chain_types::{BlockId, Digest32};
use serde::{Deserialize, Serialize};

/// Block summary.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockSummary {
    pub id: String,
    pub height: u32,
    pub timestamp: u64,
    pub transactions_count: usize,
}

/// Block header response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockHeader {
    pub id: String,
    pub parent_id: String,
    pub height: u32,
    pub timestamp: u64,
    pub n_bits: u32,
    pub difficulty: String,
}

/// Pagination query.
#[derive(Deserialize, Default)]
pub struct Pagination {
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

/// GET /blocks
pub async fn get_blocks(
    State(state): State<AppState>,
    Query(pagination): Query<Pagination>,
) -> ApiResult<Json<Vec<BlockSummary>>> {
    let headers = state
        .state
        .history
        .headers
        .get_range(pagination.offset, pagination.limit)?;

    let blocks: Vec<BlockSummary> = headers
        .into_iter()
        .map(|h| BlockSummary {
            id: hex::encode(h.id.0.as_ref()),
            height: h.height,
            timestamp: h.timestamp,
            transactions_count: 0, // Would need to count from block data
        })
        .collect();

    Ok(Json(blocks))
}

/// GET /blocks/:id
pub async fn get_block_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<BlockSummary>> {
    let block_id = parse_block_id(&id)?;

    let header = state
        .state
        .history
        .headers
        .get(&block_id)?
        .ok_or_else(|| ApiError::NotFound(format!("Block {} not found", id)))?;

    Ok(Json(BlockSummary {
        id: hex::encode(header.id.0.as_ref()),
        height: header.height,
        timestamp: header.timestamp,
        transactions_count: 0,
    }))
}

/// GET /blocks/:id/header
pub async fn get_block_header(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<BlockHeader>> {
    let block_id = parse_block_id(&id)?;

    let header = state
        .state
        .history
        .headers
        .get(&block_id)?
        .ok_or_else(|| ApiError::NotFound(format!("Block {} not found", id)))?;

    Ok(Json(BlockHeader {
        id: hex::encode(header.id.0.as_ref()),
        parent_id: hex::encode(header.parent_id.0.as_ref()),
        height: header.height,
        timestamp: header.timestamp,
        n_bits: header.n_bits,
        difficulty: header.n_bits.to_string(), // Simplified
    }))
}

/// GET /blocks/:id/transactions
pub async fn get_block_transactions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<String>>> {
    let block_id = parse_block_id(&id)?;

    let txs = state
        .state
        .history
        .blocks
        .get_transactions(&block_id)?
        .ok_or_else(|| ApiError::NotFound(format!("Block {} not found", id)))?;

    // Return transaction IDs
    let tx_ids: Vec<String> = txs
        .txs
        .iter()
        .map(|tx| hex::encode(tx.id().0.as_ref()))
        .collect();

    Ok(Json(tx_ids))
}

/// GET /blocks/at/:height
pub async fn get_block_at_height(
    State(state): State<AppState>,
    Path(height): Path<u32>,
) -> ApiResult<Json<BlockSummary>> {
    let header = state
        .state
        .history
        .headers
        .get_by_height(height)?
        .ok_or_else(|| ApiError::NotFound(format!("Block at height {} not found", height)))?;

    Ok(Json(BlockSummary {
        id: hex::encode(header.id.0.as_ref()),
        height: header.height,
        timestamp: header.timestamp,
        transactions_count: 0,
    }))
}

/// Parse a hex block ID string into a BlockId.
fn parse_block_id(id: &str) -> Result<BlockId, ApiError> {
    let id_bytes =
        hex::decode(id).map_err(|_| ApiError::BadRequest("Invalid block ID".to_string()))?;

    if id_bytes.len() != 32 {
        return Err(ApiError::BadRequest(
            "Block ID must be 32 bytes".to_string(),
        ));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&id_bytes);
    Ok(BlockId(Digest32::from(arr)))
}
