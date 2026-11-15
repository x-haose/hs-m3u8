//! 动态分段：开始时一个连接覆盖要下的全部；有连接空出来（或新开的连接要活干）时，先给还没分出去的段，没有就把剩得
//! 最多的一段劈成两半、后半给它。被劈的连接读到新的终点就停。

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;

/// 剩下的不到它的两倍就不劈：劈出的段太小，新开连接的开销比省下的时间多。
pub(crate) const MIN_SPLIT: u64 = 1 << 20;

/// 各连接在下的段与还没分出去的段。
#[derive(Debug)]
pub(crate) struct Schedule {
    /// 还没分出去的段，按先后
    pending: VecDeque<Range<u64>>,
    /// 连接编号 → 它从当前位置到终点（不含）还要下的；终点会因劈段变小
    active: BTreeMap<u64, Range<u64>>,
    next_id: u64,
    limit: usize,
}

impl Schedule {
    /// 要下 `missing`（升序、不重叠、不为空的段），同时至多 `limit` 个连接。
    pub(crate) fn new(missing: Vec<Range<u64>>, limit: usize) -> Self {
        Schedule {
            pending: missing.into(),
            active: BTreeMap::new(),
            next_id: 0,
            limit,
        }
    }

    /// 在途的连接数。
    pub(crate) fn active(&self) -> usize {
        self.active.len()
    }

    /// 全部下完：没有在途的，也没有没分出去的。
    pub(crate) fn done(&self) -> bool {
        self.active.is_empty() && self.pending.is_empty()
    }

    /// 分一段给一个要活干的连接，返回编号与这一段；在途的已到上限，或没有可分的（没分出去的段已分完，在途的段剩下的都
    /// 不到 [`MIN_SPLIT`] 的两倍）时为 None。
    pub(crate) fn assign(&mut self) -> Option<(u64, Range<u64>)> {
        if self.active.len() >= self.limit {
            return None;
        }
        let range = match self.pending.pop_front() {
            Some(range) => range,
            None => {
                let (_, widest) = self
                    .active
                    .iter_mut()
                    .max_by_key(|(_, r)| r.end - r.start)?;
                let remaining = widest.end - widest.start;
                if remaining < 2 * MIN_SPLIT {
                    return None;
                }
                let middle = widest.start + remaining / 2;
                let back = middle..widest.end;
                widest.end = middle;
                back
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        self.active.insert(id, range.clone());
        Some((id, range))
    }

    /// 连接 `id` 还要下的：当前位置到终点。
    pub(crate) fn remaining(&self, id: u64) -> Range<u64> {
        self.active[&id].clone()
    }

    /// 连接 `id` 从当前位置起收到 `len` 字节：返回其中属于它的字节数（到它这段的终点为止），位置前移这么多。
    pub(crate) fn advance(&mut self, id: u64, len: u64) -> u64 {
        let range = self.active.get_mut(&id).expect("在途的连接才收数据");
        let taken = len.min(range.end - range.start);
        range.start += taken;
        taken
    }

    /// 连接 `id` 不再下（下完了、被限、出错或取消）：没下完的放回最前面，等别的连接接着下。
    pub(crate) fn release(&mut self, id: u64) {
        let range = self.active.remove(&id).expect("在途的连接才放手");
        if !range.is_empty() {
            self.pending.push_front(range);
        }
    }

    /// 服务器限制了连接数：上限降到在途的连接数减一（失败的那个算在途），至少 1；返回降到的上限。
    pub(crate) fn lower_limit(&mut self) -> usize {
        self.limit = self.limit.min(self.active.len()).saturating_sub(1).max(1);
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = MIN_SPLIT;

    /// 要下的只有 `range` 一段。
    fn only(range: Range<u64>) -> Vec<Range<u64>> {
        vec![range]
    }

    #[test]
    fn idle_connections_split_the_widest_remaining_piece() {
        let mut s = Schedule::new(only(0..8 * M), 3);
        assert_eq!(s.assign(), Some((0, 0..8 * M)));
        assert_eq!(s.advance(0, M), M);
        // 剩 7M，从 1M 起劈成 [1M, 4.5M) 与 [4.5M, 8M)
        let half = M + 7 * M / 2;
        assert_eq!(s.assign(), Some((1, half..8 * M)));
        assert_eq!(s.remaining(0), M..half);
        // 连接 1 已收到一个字节，剩得最多的是连接 0
        s.advance(1, 1);
        assert_eq!(s.assign(), Some((2, M + 7 * M / 4..half)));
        assert_eq!(s.assign(), None, "在途的已到上限");
        // 被劈的连接读到新的终点即停
        assert_eq!(s.advance(0, 8 * M), 7 * M / 4);
        s.release(0);
        assert!(!s.done());
    }

    #[test]
    fn small_pieces_are_not_split_and_pending_ranges_go_first() {
        let mut s = Schedule::new(vec![0..10, 2 * M..4 * M - 1], 4);
        assert_eq!(s.assign(), Some((0, 0..10)));
        assert_eq!(s.assign(), Some((1, 2 * M..4 * M - 1)));
        assert_eq!(s.assign(), None, "剩下的不到 2M，不劈");
        s.advance(0, 10);
        s.release(0);
        s.advance(1, 2 * M - 1);
        s.release(1);
        assert!(s.done());
    }

    #[test]
    fn released_remainders_and_lowered_limits() {
        let mut s = Schedule::new(only(0..100), 8);
        let (a, _) = s.assign().unwrap();
        s.advance(a, 30);
        s.release(a);
        assert_eq!(s.assign(), Some((1, 30..100)));
        // 在途 1 个时降：至少 1
        assert_eq!(s.lower_limit(), 1);
        assert_eq!(s.assign(), None);

        let mut s = Schedule::new(only(0..100 * M), 8);
        for _ in 0..3 {
            s.assign().unwrap();
        }
        // 第 3 个连接被限：上限降到 2
        assert_eq!(s.lower_limit(), 2);
    }
}
