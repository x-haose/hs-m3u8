//! 录制中核对能否合并进 MP4（见 [`output::check_mp4`]）：每个（会话, 不连续段）组在各轨都录到分片时，拿各轨录到的
//! 第一个分片核对一次，放不进 MP4 的内容不必录完才报。

use std::collections::{HashMap, HashSet};

use hs_m3u8_remux::Streams;

use crate::fetch::ItemInfo;
use crate::output::{self, GroupSegment, GroupTrack};
use crate::workdir::Layout;
use crate::{Error, blocking};

/// 分组的键：（会话, 不连续段）。
type Key = (u32, u64);

pub(super) struct Mp4Check {
    /// 各轨的取流方式
    streams: Vec<Streams>,
    /// 第一个核对过的组：之后的组与它一起核对，流种类与参数须与它相同
    first: Option<Vec<GroupTrack>>,
    checked: HashSet<Key>,
    /// 还没核对的组里各轨录到的第一个分片
    heads: HashMap<Key, Vec<Option<GroupTrack>>>,
}

impl Mp4Check {
    pub(super) fn new(streams: Vec<Streams>) -> Self {
        Mp4Check {
            streams,
            first: None,
            checked: HashSet::new(),
            heads: HashMap::new(),
        }
    }

    /// 分片 `id` 录到了任务目录；它所在的组各轨都有分片时核对这一组。
    pub(super) async fn recorded(&mut self, id: ItemInfo, layout: &Layout) -> Result<(), Error> {
        let key = (id.name.session, id.name.discontinuity);
        if self.checked.contains(&key) {
            return Ok(());
        }
        let heads = self
            .heads
            .entry(key)
            .or_insert_with(|| vec![None; self.streams.len()]);
        heads[id.track].get_or_insert_with(|| GroupTrack {
            init: id.name.init.map(|f| layout.init(id.track, f)),
            segments: vec![GroupSegment {
                path: layout.segment(id.track, &id.name),
                duration_us: id.name.duration_us,
            }],
        });
        if heads.iter().any(Option::is_none) {
            return Ok(());
        }
        let group: Vec<GroupTrack> = self
            .heads
            .remove(&key)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        let groups: Vec<Vec<GroupTrack>> =
            self.first.iter().cloned().chain([group.clone()]).collect();
        let streams = self.streams.clone();
        blocking(move || output::check_mp4(&streams, &groups)).await??;
        self.checked.insert(key);
        self.first.get_or_insert(group);
        Ok(())
    }
}
