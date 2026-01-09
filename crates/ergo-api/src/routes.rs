//! API route definitions.

use crate::{handlers, AppState};
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};

/// Create the API router with all routes.
pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Info endpoints
        .route("/info", get(handlers::info::get_info))
        // Block endpoints
        .route("/blocks", get(handlers::blocks::get_blocks))
        .route("/blocks/:id", get(handlers::blocks::get_block_by_id))
        .route(
            "/blocks/:id/header",
            get(handlers::blocks::get_block_header),
        )
        .route(
            "/blocks/:id/transactions",
            get(handlers::blocks::get_block_transactions),
        )
        .route(
            "/blocks/at/:height",
            get(handlers::blocks::get_block_at_height),
        )
        // Transaction endpoints
        .route(
            "/transactions",
            post(handlers::transactions::submit_transaction),
        )
        .route(
            "/transactions/:id",
            get(handlers::transactions::get_transaction),
        )
        .route(
            "/transactions/unconfirmed",
            get(handlers::transactions::get_unconfirmed),
        )
        .route(
            "/transactions/unconfirmed/:id",
            get(handlers::transactions::get_unconfirmed_by_id),
        )
        // UTXO endpoints
        .route("/utxo/byId/:id", get(handlers::utxo::get_box_by_id))
        .route(
            "/utxo/byAddress/:address",
            get(handlers::utxo::get_boxes_by_address),
        )
        // Peer endpoints
        .route("/peers/all", get(handlers::peers::get_all_peers))
        .route(
            "/peers/connected",
            get(handlers::peers::get_connected_peers),
        )
        .route("/peers/blacklisted", get(handlers::peers::get_blacklisted))
        .route("/peers/connect", post(handlers::peers::connect_peer))
        // Mining endpoints
        .route("/mining/candidate", get(handlers::mining::get_candidate))
        .route(
            "/mining/candidate/extended",
            get(handlers::mining::get_candidate_extended),
        )
        .route("/mining/solution", post(handlers::mining::submit_solution))
        .route(
            "/mining/rewardAddress",
            get(handlers::mining::get_reward_address).post(handlers::mining::set_reward_address),
        )
        // Wallet endpoints (optional)
        .route("/wallet/init", post(handlers::wallet::init_wallet))
        .route("/wallet/unlock", post(handlers::wallet::unlock_wallet))
        .route("/wallet/lock", get(handlers::wallet::lock_wallet))
        .route("/wallet/status", get(handlers::wallet::get_status))
        .route("/wallet/balances", get(handlers::wallet::get_balances))
        .route("/wallet/addresses", get(handlers::wallet::get_addresses))
        .route(
            "/wallet/addresses/derive",
            post(handlers::wallet::derive_address),
        )
        .route(
            "/wallet/boxes/unspent",
            get(handlers::wallet::get_unspent_boxes),
        )
        .route(
            "/wallet/transaction/send",
            post(handlers::wallet::send_transaction),
        )
        // Utils endpoints
        .route("/utils/seed", get(handlers::utils::get_seed))
        .route(
            "/utils/seed/:length",
            get(handlers::utils::get_seed_with_length),
        )
        .route("/utils/hash/blake2b", post(handlers::utils::hash_blake2b))
        .route(
            "/utils/address/:address",
            get(handlers::utils::validate_address),
        )
        .route(
            "/utils/address",
            post(handlers::utils::validate_address_post),
        )
        .route(
            "/utils/addressToRaw/:address",
            get(handlers::utils::address_to_raw),
        )
        .route(
            "/utils/rawToAddress/:raw",
            get(handlers::utils::raw_to_address),
        )
        .route(
            "/utils/ergoTreeToAddress/:ergoTree",
            get(handlers::utils::ergo_tree_to_address),
        )
        .route(
            "/utils/ergoTreeToAddress",
            post(handlers::utils::ergo_tree_to_address_post),
        )
        // Emission endpoints
        .route(
            "/emission/at/:height",
            get(handlers::emission::get_emission_at_height),
        )
        .route(
            "/emission/scripts",
            get(handlers::emission::get_emission_scripts),
        )
        // Script endpoints
        .route("/script/p2sAddress", post(handlers::script::compile_to_p2s))
        .route(
            "/script/p2shAddress",
            post(handlers::script::compile_to_p2sh),
        )
        .route(
            "/script/addressToTree",
            post(handlers::script::address_to_tree),
        )
        .route(
            "/script/addressToBytes",
            post(handlers::script::address_to_bytes),
        )
        .route(
            "/script/executeWithContext",
            post(handlers::script::execute_with_context),
        )
        // Apply middleware
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
