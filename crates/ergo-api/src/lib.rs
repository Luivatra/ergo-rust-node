//! # ergo-api
//!
//! REST API for the Ergo blockchain node.
//!
//! This crate provides HTTP endpoints compatible with the Scala node API:
//! - `/info` - Node information
//! - `/blocks` - Block queries
//! - `/transactions` - Transaction submission and queries
//! - `/utxo` - UTXO queries
//! - `/peers` - Peer management
//! - `/mining` - Mining interface
//! - `/wallet` - Wallet operations

mod error;
mod handlers;
mod routes;
mod state;

pub use error::{ApiError, ApiResult};
pub use routes::create_router;
pub use state::AppState;

use axum::Router;

/// Default API port.
pub const DEFAULT_API_PORT: u16 = 9053;

/// API version (matches Scala node API compatibility).
pub const API_VERSION: &str = "5.0.0";

/// Node version (our Rust implementation version).
pub const NODE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Create the API router with all routes.
pub fn build_api(state: AppState) -> Router {
    create_router(state)
}
