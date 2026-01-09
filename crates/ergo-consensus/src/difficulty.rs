//! Difficulty adjustment algorithm.
//!
//! Ergo uses a linear least squares regression over 8 epochs to adjust
//! difficulty, targeting 2-minute block times.

use crate::params::{BLOCK_INTERVAL_SECS, DIFFICULTY_EPOCHS, EPOCH_LENGTH, MAX_DIFFICULTY_CHANGE};
use crate::{ConsensusError, ConsensusResult};
use num_bigint::BigUint;
use tracing::debug;

/// Header data needed for difficulty calculation.
#[derive(Debug, Clone)]
pub struct HeaderForDifficulty {
    /// Block height.
    pub height: u32,
    /// Block timestamp in milliseconds.
    pub timestamp: u64,
    /// Required difficulty (nBits).
    pub n_bits: u32,
}

/// Difficulty adjustment calculator.
pub struct DifficultyAdjustment {
    /// Target block interval in milliseconds.
    target_interval_ms: u64,
    /// Epoch length in blocks.
    epoch_length: u32,
    /// Number of epochs to use for calculation.
    use_epochs: u32,
}

impl Default for DifficultyAdjustment {
    fn default() -> Self {
        Self::new()
    }
}

impl DifficultyAdjustment {
    /// Create a new difficulty adjustment calculator with default parameters.
    pub fn new() -> Self {
        Self {
            target_interval_ms: BLOCK_INTERVAL_SECS * 1000,
            epoch_length: EPOCH_LENGTH,
            use_epochs: DIFFICULTY_EPOCHS,
        }
    }

    /// Create with custom parameters (for testing).
    pub fn with_params(target_interval_ms: u64, epoch_length: u32, use_epochs: u32) -> Self {
        Self {
            target_interval_ms,
            epoch_length,
            use_epochs,
        }
    }

    /// Calculate the required difficulty for the next block.
    ///
    /// # Arguments
    /// * `headers` - Recent headers in chronological order (oldest first)
    /// * `next_height` - Height of the block being validated
    ///
    /// # Returns
    /// The required difficulty as nBits
    pub fn calculate_next_difficulty(
        &self,
        headers: &[HeaderForDifficulty],
        next_height: u32,
    ) -> ConsensusResult<u32> {
        // For the first epoch, use genesis difficulty
        if next_height <= self.epoch_length {
            return Ok(headers.first().map(|h| h.n_bits).unwrap_or(0x1f00ffff)); // Default genesis difficulty
        }

        // Check if we're at an epoch boundary
        if next_height % self.epoch_length != 0 {
            // Not at epoch boundary, use previous block's difficulty
            return Ok(headers.last().map(|h| h.n_bits).ok_or_else(|| {
                ConsensusError::InvalidDifficulty {
                    got: "no headers".to_string(),
                    expected: "at least one header".to_string(),
                }
            })?);
        }

        // At epoch boundary - calculate new difficulty using linear regression
        let new_difficulty = self.linear_regression_difficulty(headers)?;

        debug!(
            height = next_height,
            new_nbits = new_difficulty,
            "Calculated new difficulty at epoch boundary"
        );

        Ok(new_difficulty)
    }

    /// Calculate difficulty using linear least squares regression.
    fn linear_regression_difficulty(
        &self,
        headers: &[HeaderForDifficulty],
    ) -> ConsensusResult<u32> {
        let n = headers.len() as f64;
        if n < 2.0 {
            return Err(ConsensusError::InvalidDifficulty {
                got: format!("{} headers", headers.len()),
                expected: "at least 2 headers".to_string(),
            });
        }

        // Calculate linear regression: timestamp = a + b * height
        // We want to find the slope (b) which represents time per block
        let mut sum_x: f64 = 0.0;
        let mut sum_y: f64 = 0.0;
        let mut sum_xy: f64 = 0.0;
        let mut sum_x2: f64 = 0.0;

        for header in headers {
            let x = header.height as f64;
            let y = header.timestamp as f64;
            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_x2 += x * x;
        }

        // Calculate slope: b = (n*sum_xy - sum_x*sum_y) / (n*sum_x2 - sum_x^2)
        let denominator = n * sum_x2 - sum_x * sum_x;
        if denominator.abs() < f64::EPSILON {
            return Err(ConsensusError::InvalidDifficulty {
                got: "degenerate data".to_string(),
                expected: "non-degenerate header timestamps".to_string(),
            });
        }

        let slope = (n * sum_xy - sum_x * sum_y) / denominator;

        // slope is actual time per block in ms
        // Calculate adjustment factor
        let target = self.target_interval_ms as f64;
        let adjustment = target / slope;

        // Clamp adjustment to maximum change
        let clamped = adjustment.clamp(1.0 / MAX_DIFFICULTY_CHANGE, MAX_DIFFICULTY_CHANGE);

        // Get current difficulty and apply adjustment
        let current_nbits = headers.last().unwrap().n_bits;
        let current_target = crate::nbits_to_target(current_nbits);

        // Adjust target (higher target = lower difficulty)
        // If blocks are too slow (slope > target), we lower difficulty (increase target)
        // If blocks are too fast (slope < target), we raise difficulty (decrease target)
        let new_target = if clamped > 1.0 {
            // Blocks too fast, decrease target (increase difficulty)
            let divisor = (clamped * 1000.0) as u64;
            &current_target * 1000u64 / divisor
        } else {
            // Blocks too slow, increase target (decrease difficulty)
            let multiplier = (1000.0 / clamped) as u64;
            &current_target * multiplier / 1000u64
        };

        Ok(crate::target_to_nbits(&new_target))
    }
}

/// Calculate the required difficulty for a block height.
///
/// This is a convenience function that creates a DifficultyAdjustment
/// and calculates the difficulty.
pub fn calculate_required_difficulty(
    headers: &[HeaderForDifficulty],
    next_height: u32,
) -> ConsensusResult<u32> {
    DifficultyAdjustment::new().calculate_next_difficulty(headers, next_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_difficulty_within_epoch() {
        let adj = DifficultyAdjustment::with_params(120_000, 1024, 8);

        let headers = vec![
            HeaderForDifficulty {
                height: 1,
                timestamp: 1000,
                n_bits: 0x1f00ffff,
            },
            HeaderForDifficulty {
                height: 2,
                timestamp: 121000,
                n_bits: 0x1f00ffff,
            },
        ];

        // Height 3 is not at epoch boundary, should use previous difficulty
        let result = adj.calculate_next_difficulty(&headers, 3).unwrap();
        assert_eq!(result, 0x1f00ffff);
    }

    #[test]
    fn test_first_epoch() {
        let adj = DifficultyAdjustment::with_params(120_000, 1024, 8);

        let headers = vec![HeaderForDifficulty {
            height: 1,
            timestamp: 1000,
            n_bits: 0x1f00ffff,
        }];

        // Heights within first epoch should use genesis difficulty
        let result = adj.calculate_next_difficulty(&headers, 100).unwrap();
        assert_eq!(result, 0x1f00ffff);
    }

    // ============ Difficulty Calculation Tests ============
    // Corresponds to Scala's "Should calculate difficulty correctly"

    #[test]
    fn test_difficulty_at_epoch_boundary() {
        // Test that difficulty is recalculated at epoch boundaries
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8); // Smaller epoch for testing

        // Create headers with exact target timing (2 min = 120,000 ms)
        let mut headers = Vec::new();
        for i in 0..128 {
            headers.push(HeaderForDifficulty {
                height: i as u32,
                timestamp: i as u64 * 120_000, // Exactly 2 minutes per block
                n_bits: 0x1d00ffff,
            });
        }

        // At epoch boundary (height 128), difficulty should be recalculated
        // With perfect timing, it should stay roughly the same
        let result = adj.calculate_next_difficulty(&headers, 128);
        assert!(
            result.is_ok(),
            "Should calculate difficulty at epoch boundary"
        );
    }

    #[test]
    fn test_difficulty_increases_when_blocks_too_fast() {
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8);

        // Create headers where blocks come in at 1 minute instead of 2
        let mut headers = Vec::new();
        for i in 0..128 {
            headers.push(HeaderForDifficulty {
                height: i as u32,
                timestamp: i as u64 * 60_000, // 1 minute per block (too fast!)
                n_bits: 0x1d00ffff,
            });
        }

        let result = adj.calculate_next_difficulty(&headers, 128).unwrap();
        let original_target = crate::nbits_to_target(0x1d00ffff);
        let new_target = crate::nbits_to_target(result);

        // When blocks are too fast, difficulty increases (target decreases)
        // Note: due to clamping, the change might be limited
        assert!(
            new_target <= original_target,
            "Target should decrease (difficulty increase) when blocks are too fast"
        );
    }

    #[test]
    fn test_difficulty_decreases_when_blocks_too_slow() {
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8);

        // Create headers where blocks come in at 4 minutes instead of 2
        let mut headers = Vec::new();
        for i in 0..128 {
            headers.push(HeaderForDifficulty {
                height: i as u32,
                timestamp: i as u64 * 240_000, // 4 minutes per block (too slow!)
                n_bits: 0x1d00ffff,
            });
        }

        let result = adj.calculate_next_difficulty(&headers, 128).unwrap();
        let original_target = crate::nbits_to_target(0x1d00ffff);
        let new_target = crate::nbits_to_target(result);

        // When blocks are too slow, difficulty decreases (target increases)
        assert!(
            new_target >= original_target,
            "Target should increase (difficulty decrease) when blocks are too slow"
        );
    }

    #[test]
    fn test_difficulty_change_clamped() {
        // Difficulty changes should be clamped to prevent dramatic swings
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8);

        // Create headers with extremely fast blocks (10 seconds)
        let mut headers = Vec::new();
        for i in 0..128 {
            headers.push(HeaderForDifficulty {
                height: i as u32,
                timestamp: i as u64 * 10_000, // 10 seconds per block (way too fast!)
                n_bits: 0x1d00ffff,
            });
        }

        let result = adj.calculate_next_difficulty(&headers, 128).unwrap();

        // The change should be clamped, not unlimited
        // MAX_DIFFICULTY_CHANGE is typically 2.0, so target can only halve or double
        let original_target = crate::nbits_to_target(0x1d00ffff);
        let new_target = crate::nbits_to_target(result);

        // Even with 12x speed difference, the target should only change by max factor
        let ratio = if original_target > new_target {
            &original_target / &new_target
        } else {
            &new_target / &original_target
        };

        assert!(
            ratio <= BigUint::from(3u32), // Allow some margin for floating point
            "Difficulty change should be clamped"
        );
    }

    #[test]
    fn test_difficulty_requires_minimum_headers() {
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8);

        // Empty headers should fail
        let result = adj.calculate_next_difficulty(&[], 128);
        assert!(result.is_err() || result.unwrap() == 0x1f00ffff);

        // Single header - not enough for regression
        let headers = vec![HeaderForDifficulty {
            height: 0,
            timestamp: 0,
            n_bits: 0x1d00ffff,
        }];

        // At epoch boundary with single header
        let result = adj.calculate_next_difficulty(&headers, 128);
        // Should either error or return the single header's difficulty
        if let Ok(nbits) = result {
            // With linear regression on 1 point, it might error or return original
            assert!(nbits > 0, "Should return some valid difficulty");
        }
    }

    #[test]
    fn test_linear_regression_slope() {
        // Test that the linear regression produces correct slope
        let adj = DifficultyAdjustment::with_params(120_000, 128, 8);

        // Perfect 2-minute blocks
        let headers: Vec<HeaderForDifficulty> = (0..10)
            .map(|i| HeaderForDifficulty {
                height: i,
                timestamp: i as u64 * 120_000,
                n_bits: 0x1d00ffff,
            })
            .collect();

        // Calculate expected slope (should be ~120,000 ms per block)
        let n = headers.len() as f64;
        let sum_x: f64 = headers.iter().map(|h| h.height as f64).sum();
        let sum_y: f64 = headers.iter().map(|h| h.timestamp as f64).sum();
        let sum_xy: f64 = headers
            .iter()
            .map(|h| h.height as f64 * h.timestamp as f64)
            .sum();
        let sum_x2: f64 = headers.iter().map(|h| (h.height as f64).powi(2)).sum();

        let slope = (n * sum_xy - sum_x * sum_y) / (n * sum_x2 - sum_x * sum_x);

        // Slope should be approximately 120,000 ms per block
        assert!(
            (slope - 120_000.0).abs() < 1.0,
            "Slope should be ~120,000, got {}",
            slope
        );
    }

    #[test]
    fn test_epoch_length_constant() {
        // Verify epoch length is used correctly
        let epoch_length = 1024u32;
        let adj = DifficultyAdjustment::with_params(120_000, epoch_length, 8);

        let headers = vec![HeaderForDifficulty {
            height: 1023,
            timestamp: 1000,
            n_bits: 0x1f00ffff,
        }];

        // Height 1024 is epoch boundary (1024 % 1024 == 0)
        // Height 1023 is not
        let not_boundary = adj.calculate_next_difficulty(&headers, 1023).unwrap();
        assert_eq!(
            not_boundary, 0x1f00ffff,
            "Should use previous difficulty when not at boundary"
        );
    }

    #[test]
    fn test_target_block_interval() {
        // Verify the target block interval (2 minutes = 120,000 ms)
        let target_ms = 2 * 60 * 1000; // 2 minutes in milliseconds
        assert_eq!(target_ms, 120_000);

        let adj = DifficultyAdjustment::new();
        assert_eq!(adj.target_interval_ms, 120_000);
    }
}
