//! Block planning (`ARCHITECTURE.md` §7.4): how a file is split into
//! blocks, from its size alone. Unlike Plexo, which sizes blocks from
//! the stream count, the number of connections a file will get here
//! isn't known at planning time — a file can gain or lose slots as
//! others in the queue activate or finish.

use crate::config::EngineConfig;
use crate::model::{BlockLayout, ByteRange, PartStatus};

/// Lays out a file's blocks from its size. Uses `EngineConfig::default()`
/// for the bounds (block size, alignment, block-count cap); pass an
/// explicit config with `plan_layout_with` to plan against other bounds.
///
/// ```text
/// not splittable or size unknown → 1 block (whole file / open-ended)
/// size ≤ 4 MiB                   → 1 block
/// otherwise block_size = clamp(ceil(size / 32), 1 MiB, 8 MiB)
///           block_size = max(block_size, ceil(size / 4096))   // block-count cap
///           round up to a multiple of 64 KiB
/// ```
#[must_use]
pub fn plan_layout(size: Option<u64>, splittable: bool) -> BlockLayout {
    plan_layout_with(size, splittable, &EngineConfig::default())
}

/// As `plan_layout`, against an explicit `config` rather than the
/// defaults.
#[must_use]
pub fn plan_layout_with(size: Option<u64>, splittable: bool, config: &EngineConfig) -> BlockLayout {
    let Some(size) = size else {
        // Size unknown: a single open-ended block, however big it turns
        // out to be. See `BlockLayout`'s doc comment for the sentinel.
        return BlockLayout {
            block_size: u64::MAX,
            count: 1,
        };
    };

    if !splittable || size <= config.single_block_threshold_bytes {
        // A known size, but either the server can't serve ranges, or the
        // file is small enough that splitting it wouldn't pay for the
        // extra round trips. One bounded block covers the whole file —
        // `size == 0` for an empty file is a legitimate, valid block.
        return BlockLayout {
            block_size: size,
            count: 1,
        };
    }

    let target = size.div_ceil(32);
    let mut block_size = target.clamp(config.min_block_bytes, config.max_block_bytes);

    // The block-count cap is a safety valve for multi-terabyte files —
    // past it, blocks grow instead of multiplying further. Raising
    // `block_size` here (rather than capping `count` after the fact)
    // keeps the partition exact: `count` below is always derived from
    // the final `block_size`, never adjusted independently of it.
    let min_for_cap = size.div_ceil(u64::from(config.max_blocks));
    block_size = block_size.max(min_for_cap);

    let alignment = config.block_alignment_bytes;
    block_size = block_size.div_ceil(alignment) * alignment;

    let count = size.div_ceil(block_size);
    // `block_size >= min_for_cap` guarantees `count <= max_blocks`, and
    // rounding `block_size` up afterwards only shrinks `count` further —
    // so this conversion cannot fail. `unwrap_or` rather than `unwrap()`
    // regardless, so a bug here degrades to a clamped block count
    // instead of a panic.
    let count = u32::try_from(count).unwrap_or(config.max_blocks);

    BlockLayout { block_size, count }
}

/// A block's length in bytes: `Some(len)` for any bounded block —
/// `Some(0)` is valid, for an empty file's single block — and `None` for
/// the open-ended sentinel, or for an index past `layout.count`.
/// `total_size` is unused (and can be anything) when `layout` is the
/// open-ended sentinel.
#[must_use]
pub fn block_len(layout: &BlockLayout, total_size: u64, index: u32) -> Option<u64> {
    if index >= layout.count {
        return None;
    }
    if layout.is_single_open_ended() {
        return None;
    }
    if index + 1 == layout.count {
        // The last block holds the remainder, which may be shorter than
        // `block_size` (or, when `count == 1`, equal to the whole file).
        let consumed = u64::from(index) * layout.block_size;
        Some(total_size.saturating_sub(consumed))
    } else {
        Some(layout.block_size)
    }
}

/// The byte range a block covers. `None` when there is nothing to fetch
/// for it — an out-of-range index, or the degenerate empty-file block
/// (`block_len == Some(0)`), since an inclusive `ByteRange` cannot
/// represent zero bytes and there is genuinely no request to make for
/// one.
#[must_use]
pub fn block_range(layout: &BlockLayout, total_size: u64, index: u32) -> Option<ByteRange> {
    if layout.is_single_open_ended() {
        return (index == 0).then(|| ByteRange::open_ended(0));
    }
    let len = block_len(layout, total_size, index)?;
    if len == 0 {
        return None;
    }
    let start = u64::from(index) * layout.block_size;
    ByteRange::bounded(start, start + len - 1)
}

/// Checks the §4 invariants between a layout and the parts tracking its
/// progress: one part per block, and no part claiming more than its
/// block actually holds.
#[must_use]
pub fn parts_consistent(layout: &BlockLayout, total_size: u64, parts: &[PartStatus]) -> bool {
    if parts.len() != layout.count as usize {
        return false;
    }
    parts.iter().enumerate().all(|(index, part)| {
        // `parts.len() == layout.count` (a `u32`) was just checked above,
        // so every index here fits back into a `u32`.
        let index = index as u32;
        match block_len(layout, total_size, index) {
            Some(len) => part.durable <= len,
            // Open-ended: no fixed length to check `durable` against yet.
            None => true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LinkId;
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn unknown_size_is_a_single_open_ended_block() {
        let layout = plan_layout(None, true);
        assert!(layout.is_single_open_ended());
        assert_eq!(block_range(&layout, 0, 0), Some(ByteRange::open_ended(0)));
        assert_eq!(block_range(&layout, 0, 1), None);
    }

    #[test]
    fn unsplittable_known_size_is_one_bounded_block() {
        let layout = plan_layout(Some(10 * MIB), false);
        assert_eq!(layout.count, 1);
        assert!(!layout.is_single_open_ended());
        assert_eq!(
            block_range(&layout, 10 * MIB, 0),
            ByteRange::bounded(0, 10 * MIB - 1)
        );
    }

    #[test]
    fn empty_file_is_one_zero_length_block_with_nothing_to_fetch() {
        let layout = plan_layout(Some(0), true);
        assert_eq!(layout.count, 1);
        assert!(!layout.is_single_open_ended());
        assert_eq!(block_len(&layout, 0, 0), Some(0));
        assert_eq!(block_range(&layout, 0, 0), None);
    }

    #[test]
    fn small_splittable_file_is_still_one_block() {
        let layout = plan_layout(Some(4 * MIB), true);
        assert_eq!(layout.count, 1);
        let layout = plan_layout(Some(1), true);
        assert_eq!(layout.count, 1);
    }

    #[test]
    fn just_over_the_threshold_splits() {
        let layout = plan_layout(Some(4 * MIB + 1), true);
        assert!(layout.count > 1);
    }

    #[test]
    fn thirty_two_mib_gets_at_least_32_blocks() {
        let layout = plan_layout(Some(32 * MIB), true);
        assert!(layout.count >= 32, "count was {}", layout.count);
        let layout = plan_layout(Some(64 * MIB), true);
        assert!(layout.count >= 32, "count was {}", layout.count);
    }

    #[test]
    fn huge_file_stays_within_the_block_count_cap() {
        let size = 500 * 1024 * MIB; // 500 TiB
        let layout = plan_layout(Some(size), true);
        assert!(layout.count <= 4096);
        assert_eq!(sum_of_block_lengths(&layout, size), size);
    }

    #[test]
    fn parts_consistent_rejects_a_mismatched_part_count() {
        let layout = plan_layout(Some(20 * MIB), true);
        let parts = vec![PartStatus::not_started(); layout.count as usize - 1];
        assert!(!parts_consistent(&layout, 20 * MIB, &parts));
    }

    #[test]
    fn parts_consistent_rejects_durable_past_the_block_end() {
        let layout = plan_layout(Some(4 * MIB), true); // single 4 MiB block
        let mut parts = vec![PartStatus::not_started(); layout.count as usize];
        parts[0].durable = 4 * MIB + 1;
        assert!(!parts_consistent(&layout, 4 * MIB, &parts));
    }

    #[test]
    fn parts_consistent_accepts_a_fully_durable_layout() {
        let layout = plan_layout(Some(20 * MIB), true);
        let mut parts = vec![PartStatus::not_started(); layout.count as usize];
        for (index, part) in parts.iter_mut().enumerate() {
            part.durable = block_len(&layout, 20 * MIB, index as u32).unwrap();
            part.link = Some(LinkId::new(1));
        }
        assert!(parts_consistent(&layout, 20 * MIB, &parts));
    }

    fn sum_of_block_lengths(layout: &BlockLayout, total_size: u64) -> u64 {
        (0..layout.count)
            .map(|index| block_len(layout, total_size, index).unwrap_or(0))
            .sum()
    }

    proptest! {
        /// Blocks exactly partition `[0, size)`: their lengths sum to
        /// the file size, and `block_range` gives contiguous,
        /// non-overlapping, in-order coverage.
        #[test]
        fn blocks_partition_the_file(size in 0u64..(200 * 1024 * MIB), splittable in any::<bool>()) {
            let layout = plan_layout(Some(size), splittable);
            prop_assert_eq!(sum_of_block_lengths(&layout, size), size);

            let mut expected_start = 0u64;
            for index in 0..layout.count {
                match block_range(&layout, size, index) {
                    Some(range) => {
                        prop_assert_eq!(range.start, expected_start);
                        let len = range.len().expect("a block_range result is always bounded");
                        expected_start += len;
                    }
                    None => {
                        // Only the empty-file degenerate block has nothing
                        // to fetch, and that's only possible as the sole block.
                        prop_assert_eq!(layout.count, 1);
                        prop_assert_eq!(block_len(&layout, size, index), Some(0));
                    }
                }
            }
            prop_assert_eq!(expected_start, size);
        }

        #[test]
        fn count_never_exceeds_the_cap(size in 0u64..(200 * 1024 * MIB), splittable in any::<bool>()) {
            let layout = plan_layout(Some(size), splittable);
            prop_assert!(layout.count <= 4096);
        }

        #[test]
        fn small_or_unsplittable_files_get_one_block(
            size in 0u64..=(4 * MIB),
            splittable in any::<bool>(),
        ) {
            let layout = plan_layout(Some(size), splittable);
            prop_assert_eq!(layout.count, 1);
        }

        #[test]
        fn unsplittable_always_gets_one_block(size in 0u64..(200 * 1024 * MIB)) {
            let layout = plan_layout(Some(size), false);
            prop_assert_eq!(layout.count, 1);
        }

        /// Interior block boundaries land on a 64 KiB multiple whenever a
        /// file is actually split into more than one block — the
        /// single-block cases have no interior boundary to align.
        #[test]
        fn multi_block_layouts_are_64kib_aligned(size in (4 * MIB + 1)..(200 * 1024 * MIB)) {
            let layout = plan_layout(Some(size), true);
            if layout.count > 1 {
                prop_assert_eq!(layout.block_size % (64 * 1024), 0);
            }
        }
    }
}
