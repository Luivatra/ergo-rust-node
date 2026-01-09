//! Transaction handlers.

use crate::{ApiError, ApiResult, AppState};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

/// Transaction submission request.
#[derive(Deserialize)]
pub struct SubmitTx {
    /// Serialized transaction (hex or base64).
    pub bytes: String,
}

/// Transaction response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxResponse {
    pub id: String,
}

/// Unconfirmed transaction info.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnconfirmedTx {
    pub id: String,
    pub fee: u64,
    pub size: usize,
}

/// POST /transactions
pub async fn submit_transaction(
    State(state): State<AppState>,
    Json(request): Json<SubmitTx>,
) -> ApiResult<Json<TxResponse>> {
    let bytes = hex::decode(&request.bytes)
        .map_err(|_| ApiError::BadRequest("Invalid transaction bytes".to_string()))?;

    // Parse and validate transaction
    // For now, just generate an ID
    let id = blake2b_hash(&bytes);

    let pooled = ergo_mempool::PooledTransaction {
        id: id.clone(),
        bytes,
        fee: 0,              // Would extract from transaction
        inputs: Vec::new(),  // Would extract from transaction
        outputs: Vec::new(), // Would extract from transaction
        arrival_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    };

    state.mempool.add(pooled)?;

    Ok(Json(TxResponse {
        id: hex::encode(&id),
    }))
}

/// GET /transactions/:id
pub async fn get_transaction(
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Would look up in chain + mempool
    Err(ApiError::NotFound(format!("Transaction {} not found", id)))
}

/// GET /transactions/unconfirmed
pub async fn get_unconfirmed(State(state): State<AppState>) -> ApiResult<Json<Vec<UnconfirmedTx>>> {
    let txs: Vec<UnconfirmedTx> = state
        .mempool
        .get_all_ids()
        .into_iter()
        .filter_map(|id| {
            state.mempool.get(&id).map(|tx| UnconfirmedTx {
                id: hex::encode(&tx.id),
                fee: tx.fee,
                size: tx.bytes.len(),
            })
        })
        .collect();

    Ok(Json(txs))
}

/// GET /transactions/unconfirmed/:id
pub async fn get_unconfirmed_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<UnconfirmedTx>> {
    let id_bytes =
        hex::decode(&id).map_err(|_| ApiError::BadRequest("Invalid transaction ID".to_string()))?;

    let tx = state
        .mempool
        .get(&id_bytes)
        .ok_or_else(|| ApiError::NotFound(format!("Unconfirmed tx {} not found", id)))?;

    Ok(Json(UnconfirmedTx {
        id: hex::encode(&tx.id),
        fee: tx.fee,
        size: tx.bytes.len(),
    }))
}

/// Simple BLAKE2b hash for generating IDs.
fn blake2b_hash(data: &[u8]) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Simplified - would use actual blake2b
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    let hash = hasher.finish();
    hash.to_be_bytes().to_vec()
}
