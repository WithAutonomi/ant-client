//! Read-ahead planning for random-access file readers. Transport-neutral: the
//! adapter fetches the records a plan names and releases the ones it marks.
//! DataMap sizes are untrusted, so plans bound records, never declared bytes.
use super::files::RecordLayout;
#[cfg(test)]
use self_encryption::{ChunkInfo, DataMap};
use std::ops::Range;
#[cfg(test)]
use xor_name::XorName;

impl RecordLayout {
    /// The read-ahead window: records from the one holding `start` that overlap
    /// `length` plaintext bytes, at most `max_records`. Undersized records
    /// shorten the window rather than lengthen it.
    pub(crate) fn window(&self, start: usize, length: usize, max_records: usize) -> Range<usize> {
        let records = self.overlapping(start, length);
        records.start..records.end.min(records.start.saturating_add(max_records))
    }
}

/// Held records to release so that at most `budget` stay held, never one that is
/// `kept`. Records behind `current` go first, farthest first; then those from it
/// onward, farthest first. `held` is ascending.
pub(crate) fn releasable(
    current: usize,
    budget: usize,
    held: &[usize],
    kept: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let excess = held.len().saturating_sub(budget);
    let (behind, onward): (Vec<_>, Vec<_>) = held
        .iter()
        .copied()
        .filter(|&index| !kept(index))
        .partition(|&index| index < current);
    behind
        .into_iter()
        .chain(onward.into_iter().rev())
        .take(excess)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(sizes: &[usize]) -> RecordLayout {
        let infos = sizes
            .iter()
            .enumerate()
            .map(|(index, &src_size)| ChunkInfo {
                index,
                dst_hash: XorName([index as u8 + 1; 32]),
                src_hash: XorName([0; 32]),
                src_size,
            })
            .collect();
        RecordLayout::new(&DataMap::new(infos)).unwrap()
    }

    #[test]
    fn undersized_records_shorten_the_window_instead_of_lengthening_it() {
        let layout = layout(&[10; 40]);
        // Every record overlaps the window's bytes; only the record cap is taken.
        assert_eq!(layout.window(0, 1_000, 10), 0..10);
        assert_eq!(layout.window(15, 25, 10), 1..4);
        assert_eq!(layout.window(400, 10, 10), 40..40);
    }

    #[test]
    fn records_behind_the_reader_are_released_first_and_kept_ones_never() {
        let held = (0..10).collect::<Vec<_>>();
        let window = |index| (5..9).contains(&index);
        // Ten held, budget seven: release the three farthest behind record 5.
        assert_eq!(releasable(5, 7, &held, window), vec![0, 1, 2]);
        // Within budget: nothing to release.
        assert!(releasable(5, 10, &held, window).is_empty());
        // Nothing behind: release the farthest ahead outside the window.
        assert_eq!(releasable(5, 4, &[5, 6, 7, 8, 9, 11], window), vec![11, 9]);
        // Kept records stay even over budget.
        assert!(releasable(5, 0, &[5, 6], window).is_empty());
    }
}
