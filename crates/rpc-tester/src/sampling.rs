//! Helpers for sampling historical blocks.

use alloy_primitives::BlockNumber;

/// Log-spaced offsets from the chain head used by [`historical_blocks`].
///
/// Recent blocks are typically served from in-memory or hot storage, while deeper blocks exercise
/// cold history such as static files or pruned tables. Log-spacing keeps the number of sampled
/// blocks small while still crossing those boundaries.
pub const HISTORICAL_OFFSETS: [u64; 5] = [128, 1_024, 10_000, 100_000, 1_000_000];

/// Returns log-spaced historical block numbers sampled backwards from `head`.
///
/// Applies every offset in [`HISTORICAL_OFFSETS`], skipping offsets that would reach past genesis.
/// The result is sorted ascending and deduplicated.
pub fn historical_blocks(head: BlockNumber) -> Vec<BlockNumber> {
    let mut blocks: Vec<BlockNumber> =
        HISTORICAL_OFFSETS.iter().filter_map(|offset| head.checked_sub(*offset)).collect();
    blocks.sort_unstable();
    blocks.dedup();
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_all_offsets_on_deep_chain() {
        let head = 10_000_000;
        assert_eq!(
            historical_blocks(head),
            vec![9_000_000, 9_900_000, 9_990_000, 9_998_976, 9_999_872]
        );
    }

    #[test]
    fn skips_offsets_past_genesis() {
        assert_eq!(historical_blocks(10_000), vec![0, 8_976, 9_872]);
        assert_eq!(historical_blocks(128), vec![0]);
        assert_eq!(historical_blocks(127), Vec::<BlockNumber>::new());
        assert_eq!(historical_blocks(0), Vec::<BlockNumber>::new());
    }
}
