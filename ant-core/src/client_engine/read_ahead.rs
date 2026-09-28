//! Read-ahead planning for random-access file readers. Transport-neutral: the
//! adapter fetches the records a plan names and releases the ones it marks.
use super::files::RecordLayout;
#[cfg(test)]
use self_encryption::{ChunkInfo, DataMap};
#[cfg(test)]
use xor_name::XorName;

impl RecordLayout {
    /// Held records to release so that at most `budget` stay held, never one that
    /// is `kept`. Records behind the one holding `position` go first, farthest
    /// first; then those from it onward, farthest first. `held` is ascending.
    pub(crate) fn releasable(
        &self,
        position: usize,
        budget: usize,
        held: &[usize],
        kept: impl Fn(usize) -> bool,
    ) -> Vec<usize> {
        let excess = held.len().saturating_sub(budget);
        let current = self.record_at(position);
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
    fn records_behind_the_reader_are_released_first_and_kept_ones_never() {
        let layout = layout(&[10; 12]);
        let held = (0..10).collect::<Vec<_>>();
        let window = |index| (5..9).contains(&index);
        // Ten held, budget seven: release the three farthest behind record 5.
        assert_eq!(layout.releasable(55, 7, &held, window), vec![0, 1, 2]);
        // Within budget: nothing to release.
        assert!(layout.releasable(55, 10, &held, window).is_empty());
        // Nothing behind: release the farthest ahead outside the window.
        let held = [5, 6, 7, 8, 9, 11];
        assert_eq!(layout.releasable(55, 4, &held, window), vec![11, 9]);
        // Kept records stay even over budget.
        assert!(layout.releasable(55, 0, &[5, 6], window).is_empty());
    }
}
