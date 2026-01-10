//! Ergo Node - A Rust implementation of the Ergo blockchain node.
//!
//! This is the main entry point for the ergo-node binary.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod config;
mod node;

use config::NodeConfig;
use node::Node;

/// Ergo blockchain node implementation in Rust.
#[derive(Parser, Debug)]
#[command(name = "ergo-node")]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "ergo-node.toml")]
    config: PathBuf,

    /// Data directory
    #[arg(short, long)]
    data_dir: Option<PathBuf>,

    /// Network to connect to
    #[arg(short, long, default_value = "mainnet")]
    network: String,

    /// Enable mining
    #[arg(long)]
    mining: bool,

    /// Enable internal CPU mining
    #[arg(long)]
    internal_mining: bool,

    /// Number of mining threads (0 = auto-detect)
    #[arg(long, default_value = "0")]
    mining_threads: usize,

    /// Mining reward address
    #[arg(long)]
    mining_address: Option<String>,

    /// API bind address
    #[arg(long, default_value = "127.0.0.1:9053")]
    api_bind: String,

    /// P2P bind address
    #[arg(long, default_value = "0.0.0.0:9030")]
    p2p_bind: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Print version and exit
    #[arg(long)]
    version_info: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.version_info {
        print_version();
        return Ok(());
    }

    // Initialize logging
    let log_level = match args.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting Ergo Rust Node v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config = NodeConfig::load(&args.config, &args)?;

    info!("Network: {}", config.network);
    info!("Data directory: {:?}", config.data_dir);
    info!("API: {}", config.api.bind_address);
    info!("P2P: {}", config.network_config.bind_address);

    // Create and run node
    let node = Node::new(config).await?;

    // Handle shutdown signals
    let node_handle = node.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Shutdown signal received");
        node_handle.shutdown().await;
    });

    // Run the node
    node.run().await?;

    info!("Ergo node stopped");
    Ok(())
}

fn print_version() {
    println!("Ergo Rust Node");
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Protocol: 5.0");
    println!();
    println!("Built with:");
    println!("  sigma-rust for ErgoScript");
    println!("  RocksDB for storage");
    println!("  Tokio for async runtime");
}
