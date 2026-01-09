//! Node info handler.

use crate::{ApiResult, AppState, NODE_VERSION};
use axum::{extract::State, Json};
use serde::Serialize;

/// Node info response.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    /// Node name.
    pub name: String,
    /// App version.
    pub app_version: String,
    /// Full height.
    pub full_height: u32,
    /// Headers height.
    pub headers_height: u32,
    /// Best header ID.
    pub best_header_id: String,
    /// Best full block ID.
    pub best_full_block_id: String,
    /// State root.
    pub state_root: String,
    /// Is mining.
    pub is_mining: bool,
    /// Peer count.
    pub peer_count: usize,
    /// Unconfirmed count.
    pub unconfirmed_count: usize,
    /// State type.
    pub state_type: String,
    /// Is synchronized.
    pub is_synced: bool,
}

/// GET /info
pub async fn get_info(State(state): State<AppState>) -> ApiResult<Json<NodeInfo>> {
    let (utxo_height, header_height) = state.state.heights();

    let best_header_id = state
        .state
        .history
        .best_header_id()
        .map(|id| hex::encode(id.0.as_ref()))
        .unwrap_or_default();
    let state_root = state.state.utxo.state_root();
    let mempool_stats = state.mempool.stats();
    let peer_count = state.peers.connected_count();

    let info = NodeInfo {
        name: state.node_name.clone(),
        app_version: NODE_VERSION.to_string(),
        full_height: utxo_height,
        headers_height: header_height,
        best_header_id: best_header_id.clone(),
        best_full_block_id: best_header_id, // Same for now
        state_root: hex::encode(&state_root),
        is_mining: state.mining_enabled,
        peer_count,
        unconfirmed_count: mempool_stats.tx_count,
        state_type: "utxo".to_string(),
        is_synced: state.state.is_synced(),
    };

    Ok(Json(info))
}
