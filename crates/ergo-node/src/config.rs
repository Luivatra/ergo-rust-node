//! Node configuration.

use crate::Args;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Complete node configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Node name.
    pub node_name: String,
    /// Network (mainnet, testnet).
    pub network: String,
    /// Data directory.
    pub data_dir: PathBuf,
    /// State type (utxo, digest).
    pub state_type: StateType,
    /// Number of blocks to keep in history.
    /// -1 means keep all blocks (no pruning).
    /// Positive values enable pruning, keeping only the last N blocks.
    #[serde(default = "default_blocks_to_keep")]
    pub blocks_to_keep: i32,
    /// Network configuration.
    #[serde(default)]
    pub network_config: NetworkConfig,
    /// API configuration.
    #[serde(default)]
    pub api: ApiConfig,
    /// Mining configuration.
    #[serde(default)]
    pub mining: MiningConfig,
    /// Wallet configuration.
    #[serde(default)]
    pub wallet: WalletConfig,
}

/// Default blocks to keep (-1 = keep all).
fn default_blocks_to_keep() -> i32 {
    -1
}

/// State type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    /// Full UTXO state.
    #[default]
    Utxo,
    /// Digest (stateless) mode.
    Digest,
}

/// Network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// P2P bind address.
    pub bind_address: String,
    /// Declared public address.
    pub declared_address: Option<String>,
    /// Known peers.
    pub known_peers: Vec<String>,
    /// Maximum connections.
    pub max_connections: usize,
    /// Maximum outbound connections.
    pub max_outbound: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:9030".to_string(),
            declared_address: None,
            known_peers: default_known_peers(),
            max_connections: 30,
            max_outbound: 10,
        }
    }
}

/// API configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// API bind address.
    pub bind_address: String,
    /// API key (optional).
    pub api_key: Option<String>,
    /// Enable public API.
    pub public: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind_address: "127.0.0.1:9053".to_string(),
            api_key: None,
            public: false,
        }
    }
}

/// Mining configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningConfig {
    /// Enable mining.
    pub enabled: bool,
    /// Use internal CPU mining.
    #[serde(default)]
    pub internal: bool,
    /// Use external miner (API-based).
    #[serde(default = "default_true")]
    pub external: bool,
    /// Reward address.
    pub reward_address: Option<String>,
    /// Number of mining threads (0 = auto-detect based on CPU cores).
    #[serde(default)]
    pub threads: usize,
}

fn default_true() -> bool {
    true
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            internal: false,
            external: true,
            reward_address: None,
            threads: 0, // Auto-detect
        }
    }
}

/// Wallet configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    /// Enable wallet.
    pub enabled: bool,
    /// Wallet data directory.
    pub data_dir: PathBuf,
    /// Secret storage file.
    pub secret_file: String,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            data_dir: PathBuf::from("wallet"),
            secret_file: "secret.json".to_string(),
        }
    }
}

impl NodeConfig {
    /// Load configuration from file and CLI args.
    pub fn load(config_path: &Path, args: &Args) -> Result<Self> {
        let mut config = if config_path.exists() {
            let content =
                std::fs::read_to_string(config_path).context("Failed to read config file")?;
            toml::from_str(&content).context("Failed to parse config file")?
        } else {
            Self::default_for_network(&args.network)
        };

        // Override with CLI args
        if let Some(ref data_dir) = args.data_dir {
            config.data_dir = data_dir.clone();
        }

        config.network = args.network.clone();
        config.network_config.bind_address = args.p2p_bind.clone();
        config.api.bind_address = args.api_bind.clone();
        config.mining.enabled = args.mining;

        // Internal mining implies mining is enabled
        if args.internal_mining {
            config.mining.enabled = true;
            config.mining.internal = true;
        }

        if args.mining_threads > 0 {
            config.mining.threads = args.mining_threads;
        }

        if let Some(ref addr) = args.mining_address {
            config.mining.reward_address = Some(addr.clone());
        }

        Ok(config)
    }

    /// Create default config for a network.
    pub fn default_for_network(network: &str) -> Self {
        let (data_dir, known_peers) = match network {
            "testnet" => (PathBuf::from(".ergo-testnet"), testnet_known_peers()),
            _ => (PathBuf::from(".ergo"), default_known_peers()),
        };

        Self {
            node_name: "ergo-rust-node".to_string(),
            network: network.to_string(),
            data_dir,
            state_type: StateType::Utxo,
            blocks_to_keep: -1, // Keep all blocks by default
            network_config: NetworkConfig {
                known_peers,
                ..Default::default()
            },
            api: ApiConfig::default(),
            mining: MiningConfig::default(),
            wallet: WalletConfig::default(),
        }
    }

    /// Save configuration to file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

/// Default known peers for mainnet.
fn default_known_peers() -> Vec<String> {
    vec![
        "213.239.193.208:9030".to_string(),
        "159.65.11.55:9030".to_string(),
        "165.227.26.175:9030".to_string(),
        "159.89.116.15:9030".to_string(),
    ]
}

/// Known peers for testnet.
fn testnet_known_peers() -> Vec<String> {
    vec!["213.239.193.208:9020".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NodeConfig::default_for_network("mainnet");
        assert_eq!(config.network, "mainnet");
        assert!(!config.network_config.known_peers.is_empty());
    }

    #[test]
    fn test_testnet_config() {
        let config = NodeConfig::default_for_network("testnet");
        assert_eq!(config.network, "testnet");
        assert!(config.data_dir.to_string_lossy().contains("testnet"));
    }
}
