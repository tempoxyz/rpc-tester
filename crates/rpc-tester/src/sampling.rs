//! Helpers for sampling historical blocks.

use alloy_primitives::BlockNumber;
use std::time::{SystemTime, UNIX_EPOCH};

/// Shallowest offset of the near-history window, exclusive of the tip range.
pub const NEAR_WINDOW_MIN_OFFSET: u64 = 8;

/// Deepest offset of the near-history window.
///
/// Blocks in `[head - NEAR_WINDOW_MAX_OFFSET, head - NEAR_WINDOW_MIN_OFFSET]` span the region
/// where clients with lazy persistence cross from in-memory state to persisted storage, so
/// random samples from this window exercise both sides of that boundary.
pub const NEAR_WINDOW_MAX_OFFSET: u64 = 128;

/// Deep-history offset strata. One block is sampled uniformly from each `(start, end]` offset
/// range, keeping depth coverage log-spread without fixed blind spots between samples.
pub const DEEP_STRATA: [(u64, u64); 4] =
    [(128, 1_024), (1_024, 10_000), (10_000, 100_000), (100_000, 1_000_000)];

/// Returns randomly sampled historical block numbers below `head`.
///
/// Samples `near_samples` blocks uniformly from the near-history window (see
/// [`NEAR_WINDOW_MAX_OFFSET`]) plus one block per [`DEEP_STRATA`] stratum, skipping any range
/// that reaches past genesis. The result is sorted ascending and deduplicated, so it may contain
/// fewer entries than requested. Randomness means repeated runs accumulate coverage instead of
/// re-testing the same blocks; the caller should log the sampled set.
pub fn historical_blocks(head: BlockNumber, near_samples: usize) -> Vec<BlockNumber> {
    sample_blocks(head, near_samples, &mut Rng::from_entropy())
}

/// Deterministic core of [`historical_blocks`].
fn sample_blocks(head: BlockNumber, near_samples: usize, rng: &mut Rng) -> Vec<BlockNumber> {
    let mut blocks = Vec::new();

    if head > NEAR_WINDOW_MIN_OFFSET {
        let max_offset = NEAR_WINDOW_MAX_OFFSET.min(head);
        for _ in 0..near_samples {
            blocks.push(head - rng.sample_range(NEAR_WINDOW_MIN_OFFSET, max_offset));
        }
    }

    for (start, end) in DEEP_STRATA {
        if head <= start {
            break;
        }
        let end = end.min(head);
        blocks.push(head - rng.sample_range(start + 1, end));
    }

    blocks.sort_unstable();
    blocks.dedup();
    blocks
}

/// Minimal xorshift64* generator.
///
/// The sampling here has no quality or security requirements, so this avoids pulling in a rng
/// crate.
struct Rng(u64);

impl Rng {
    /// Seeds the generator from the system clock.
    fn from_entropy() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        // Never zero, which would make xorshift degenerate.
        Self((u64::from(nanos) << 32 | secs & 0xffff_ffff) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Returns a value in `[min, max]`. The modulo bias is irrelevant at these range sizes.
    fn sample_range(&mut self, min: u64, max: u64) -> u64 {
        min + self.next() % (max - min + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_stay_in_their_ranges() {
        let head = 10_000_000;
        let blocks = sample_blocks(head, 8, &mut Rng(42));

        let near: Vec<_> = blocks
            .iter()
            .filter(|b| {
                **b >= head - NEAR_WINDOW_MAX_OFFSET && **b <= head - NEAR_WINDOW_MIN_OFFSET
            })
            .collect();
        assert!(!near.is_empty() && near.len() <= 8);

        for (start, end) in DEEP_STRATA {
            let in_stratum =
                blocks.iter().filter(|b| **b >= head - end && **b < head - start).count();
            assert_eq!(in_stratum, 1, "expected exactly one sample in stratum ({start}, {end}]");
        }

        assert!(blocks.windows(2).all(|w| w[0] < w[1]), "sorted and deduplicated");
    }

    #[test]
    fn deterministic_for_same_seed() {
        assert_eq!(
            sample_blocks(10_000_000, 8, &mut Rng(7)),
            sample_blocks(10_000_000, 8, &mut Rng(7))
        );
        assert_ne!(
            sample_blocks(10_000_000, 8, &mut Rng(7)),
            sample_blocks(10_000_000, 8, &mut Rng(8))
        );
    }

    #[test]
    fn young_chains_clamp_to_genesis() {
        // Head below the near window: nothing to sample.
        assert!(sample_blocks(8, 8, &mut Rng(1)).is_empty());
        assert!(sample_blocks(0, 8, &mut Rng(1)).is_empty());

        // Head inside the near window: samples stay within [0, head - min offset].
        let blocks = sample_blocks(100, 8, &mut Rng(1));
        assert!(!blocks.is_empty());
        assert!(blocks.iter().all(|b| *b <= 100 - NEAR_WINDOW_MIN_OFFSET));

        // Head inside a deep stratum: the partial stratum is still sampled, deeper ones skipped.
        let blocks = sample_blocks(2_000, 0, &mut Rng(1));
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| *b < 2_000 - 128));
    }
}
