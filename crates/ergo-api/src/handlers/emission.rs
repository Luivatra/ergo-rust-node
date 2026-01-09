//! Emission API handlers.
//!
//! Provides endpoints for querying emission schedule information:
//! - Emission at a specific height
//! - Emission-related scripts

use crate::{ApiResult};
use axum::{
    extract::Path,
    Json,
};
use serde::Serialize;

// ==================== Constants ====================

/// Total ERG supply in nanoERG (97,739,924 ERG).
const TOTAL_SUPPLY: u64 = 97_739_924_000_000_000;

/// Initial block reward in nanoERG (75 ERG).
const INITIAL_REWARD: u64 = 75_000_000_000;

/// Reward reduction per block in nanoERG (3 ERG per 64800 blocks = ~3 months).
const REWARD_REDUCTION: u64 = 3_000_000_000;

/// Blocks per reduction period (approximately 3 months).
const REDUCTION_PERIOD: u64 = 64_800;

/// Fixed rate period (first 2 years = 525600 blocks at 2 min/block).
const FIXED_RATE_PERIOD: u64 = 525_600;

/// Minimum block reward in nanoERG (3 ERG).
const MIN_REWARD: u64 = 3_000_000_000;

/// Emission delay in blocks (720 blocks before miner can spend reward).
const EMISSION_DELAY: u32 = 720;

// ==================== Response Types ====================

/// Emission information at a specific height.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmissionInfo {
    /// Block height.
    pub height: u32,
    /// Miner reward at this height in nanoERG.
    pub miner_reward: u64,
    /// Total coins issued up to this height in nanoERG.
    pub total_coins_issued: u64,
    /// Total remaining coins in emission contract in nanoERG.
    pub total_remain_coins: u64,
    /// Re-emission amount (EIP-27) in nanoERG.
    pub reemission_amt: u64,
}

/// Emission scripts information.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmissionScripts {
    /// Emission box proposition address.
    pub emission: String,
    /// Re-emission box proposition address (EIP-27).
    pub reemission: String,
    /// Pay-to-reemission address (EIP-27).
    pub pay_to_reemission: String,
}

// ==================== Handlers ====================

/// GET /emission/at/:height
/// Get emission information at a specific block height.
pub async fn get_emission_at_height(
    Path(height): Path<u32>,
) -> ApiResult<Json<EmissionInfo>> {
    let info = calculate_emission_info(height);
    Ok(Json(info))
}

/// GET /emission/scripts
/// Get emission-related script addresses.
pub async fn get_emission_scripts() -> ApiResult<Json<EmissionScripts>> {
    // These are placeholder addresses - in a real implementation,
    // they would be derived from the actual emission contracts
    let scripts = EmissionScripts {
        emission: "emission_contract_address".to_string(),
        reemission: "reemission_contract_address".to_string(),
        pay_to_reemission: "pay_to_reemission_address".to_string(),
    };
    Ok(Json(scripts))
}

// ==================== Helper Functions ====================

/// Calculate emission information at a given height.
fn calculate_emission_info(height: u32) -> EmissionInfo {
    let miner_reward = miners_reward_at_height(height as u64);
    let total_coins_issued = issued_coins_after_height(height as u64);
    let total_remain_coins = TOTAL_SUPPLY.saturating_sub(total_coins_issued);

    // Re-emission is 0 for now (EIP-27 not yet implemented)
    let reemission_amt = 0;

    EmissionInfo {
        height,
        miner_reward,
        total_coins_issued,
        total_remain_coins,
        reemission_amt,
    }
}

/// Calculate miner reward at a specific height.
///
/// Emission schedule:
/// - Blocks 1-525600 (first ~2 years): 75 ERG per block
/// - Blocks 525601+: Decreases by 3 ERG every 64800 blocks
/// - Minimum reward: 3 ERG per block
fn miners_reward_at_height(height: u64) -> u64 {
    if height == 0 {
        return 0;
    }

    if height <= FIXED_RATE_PERIOD {
        // First 2 years: fixed 75 ERG
        INITIAL_REWARD
    } else {
        // After 2 years: decreasing rewards
        let blocks_after_fixed = height - FIXED_RATE_PERIOD;
        let reduction_epochs = blocks_after_fixed / REDUCTION_PERIOD;
        let reduction = reduction_epochs * REWARD_REDUCTION;

        if reduction >= INITIAL_REWARD - MIN_REWARD {
            MIN_REWARD
        } else {
            INITIAL_REWARD - reduction
        }
    }
}

/// Calculate total coins issued up to and including a given height.
fn issued_coins_after_height(height: u64) -> u64 {
    if height == 0 {
        return 0;
    }

    let mut total = 0u64;

    if height <= FIXED_RATE_PERIOD {
        // All blocks at fixed rate
        total = height * INITIAL_REWARD;
    } else {
        // Fixed rate period
        total = FIXED_RATE_PERIOD * INITIAL_REWARD;

        // Decreasing rate period
        let remaining_height = height - FIXED_RATE_PERIOD;
        let mut current_reward = INITIAL_REWARD;
        let mut blocks_processed = 0u64;

        while blocks_processed < remaining_height && current_reward > MIN_REWARD {
            let blocks_at_this_rate = std::cmp::min(
                REDUCTION_PERIOD,
                remaining_height - blocks_processed,
            );
            total += blocks_at_this_rate * current_reward;
            blocks_processed += blocks_at_this_rate;

            if current_reward > REWARD_REDUCTION {
                current_reward -= REWARD_REDUCTION;
            } else {
                current_reward = MIN_REWARD;
            }
        }

        // Remaining blocks at minimum rate
        if blocks_processed < remaining_height {
            total += (remaining_height - blocks_processed) * MIN_REWARD;
        }
    }

    // Cap at total supply
    std::cmp::min(total, TOTAL_SUPPLY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_miners_reward_height_0() {
        assert_eq!(miners_reward_at_height(0), 0);
    }

    #[test]
    fn test_miners_reward_first_block() {
        assert_eq!(miners_reward_at_height(1), INITIAL_REWARD);
    }

    #[test]
    fn test_miners_reward_fixed_period() {
        // Within fixed rate period
        assert_eq!(miners_reward_at_height(100), INITIAL_REWARD);
        assert_eq!(miners_reward_at_height(100_000), INITIAL_REWARD);
        assert_eq!(miners_reward_at_height(FIXED_RATE_PERIOD), INITIAL_REWARD);
    }

    #[test]
    fn test_miners_reward_decreasing() {
        // Just after fixed period
        let reward = miners_reward_at_height(FIXED_RATE_PERIOD + 1);
        assert_eq!(reward, INITIAL_REWARD); // First reduction period

        // After one reduction period
        let reward = miners_reward_at_height(FIXED_RATE_PERIOD + REDUCTION_PERIOD + 1);
        assert_eq!(reward, INITIAL_REWARD - REWARD_REDUCTION);
    }

    #[test]
    fn test_miners_reward_minimum() {
        // Very high block - should be at minimum
        let reward = miners_reward_at_height(10_000_000);
        assert_eq!(reward, MIN_REWARD);
    }

    #[test]
    fn test_issued_coins_height_0() {
        assert_eq!(issued_coins_after_height(0), 0);
    }

    #[test]
    fn test_issued_coins_first_block() {
        assert_eq!(issued_coins_after_height(1), INITIAL_REWARD);
    }

    #[test]
    fn test_issued_coins_fixed_period() {
        let expected = FIXED_RATE_PERIOD * INITIAL_REWARD;
        assert_eq!(issued_coins_after_height(FIXED_RATE_PERIOD), expected);
    }

    #[test]
    fn test_emission_info() {
        let info = calculate_emission_info(100);

        assert_eq!(info.height, 100);
        assert_eq!(info.miner_reward, INITIAL_REWARD);
        assert_eq!(info.total_coins_issued, 100 * INITIAL_REWARD);
        assert_eq!(info.total_remain_coins, TOTAL_SUPPLY - (100 * INITIAL_REWARD));
        assert_eq!(info.reemission_amt, 0);
    }
}
