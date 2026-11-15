//! 已完成的字节范围的集合。

use std::ops::Range;

/// 字节范围的并集：各段升序、不重叠、不相接、不为空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RangeSet(Vec<Range<u64>>);

impl RangeSet {
    /// `ranges` 须升序、不重叠、不相接、不为空（任务目录里的记录，读取时已核对）。
    pub(crate) fn from_sorted(ranges: Vec<Range<u64>>) -> Self {
        debug_assert!(ranges.windows(2).all(|w| w[0].end < w[1].start));
        RangeSet(ranges)
    }

    pub(crate) fn ranges(&self) -> &[Range<u64>] {
        &self.0
    }

    /// 并入 `range`；空范围不起作用。
    pub(crate) fn insert(&mut self, range: Range<u64>) {
        if range.is_empty() {
            return;
        }
        // 与 range 重叠或相接的段是连续的一串：[first, last)
        let first = self.0.partition_point(|r| r.end < range.start);
        let last = self.0.partition_point(|r| r.start <= range.end);
        let merged = if first < last {
            self.0[first].start.min(range.start)..self.0[last - 1].end.max(range.end)
        } else {
            range
        };
        self.0.splice(first..last, [merged]);
    }

    /// 各段长度之和。
    pub(crate) fn len(&self) -> u64 {
        self.0.iter().map(|r| r.end - r.start).sum()
    }

    /// `0..total` 里不在集合中的各段，升序。
    pub(crate) fn missing(&self, total: u64) -> Vec<Range<u64>> {
        let mut gaps = Vec::new();
        let mut at = 0;
        for r in &self.0 {
            if r.start > at {
                gaps.push(at..r.start.min(total));
            }
            at = at.max(r.end);
        }
        if at < total {
            gaps.push(at..total);
        }
        gaps.retain(|g| !g.is_empty());
        gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_merge_overlapping_and_adjacent_ranges() {
        let mut set = RangeSet::default();
        for r in [10..20, 30..40, 0..5, 20..25, 26..30, 5..6, 3..4, 50..50] {
            set.insert(r);
        }
        assert_eq!(set.ranges(), [0..6, 10..25, 26..40]);
        set.insert(6..26);
        let whole = 0..40;
        assert_eq!(set.ranges(), [whole]);
        assert_eq!(set.len(), 40);
    }

    #[test]
    fn missing_lists_the_gaps() {
        let set = RangeSet::from_sorted(vec![2..4, 6..10]);
        assert_eq!(set.missing(12), [0..2, 4..6, 10..12]);
        assert_eq!(set.missing(10), [0..2, 4..6]);
        let whole = 0..3;
        assert_eq!(RangeSet::default().missing(3), std::slice::from_ref(&whole));
        assert_eq!(RangeSet::from_sorted(vec![whole]).missing(3), []);
    }
}
