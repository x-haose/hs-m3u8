//! 录制中核对能否合并进 MP4（见 [`output::check_mp4`]）：每个（会话, 不连续段）组在各轨都录到分片时，拿各轨录到的
//! 第一个分片与合并时的第 0 组一起核对一次。放不进 MP4 的原因交给调用方放进进度
//! （[`crate::Progress::mp4_unsupported`]），录制照常进行：直播错过的内容录不回来，停不停由调用方定；合并 MP4 时照样
//! 报错。

use std::collections::{BTreeSet, HashMap};

use hs_m3u8_remux::Streams;

use super::merge::MergePlan;
use crate::fetch::ItemInfo;
use crate::output::{self, GroupTrack};
use crate::workdir::{GroupKey, Layout, SegmentName};
use crate::{Error, Mp4Unsupported, Unsupported, blocking};

pub(super) struct Mp4Check {
    /// 各轨的取流方式
    streams: Vec<Streams>,
    /// 合并时的第 0 组（各轨都有分片的组里键最小的）的键与各轨开头的分片；还没有这样的组时为 None
    first: Option<(GroupKey, Vec<GroupTrack>)>,
    /// 各轨都有分片的组：录制开始时任务目录里的与本次录齐的
    complete: BTreeSet<GroupKey>,
    /// 还没录齐的组里各轨录到的第一个分片
    pending: HashMap<GroupKey, Vec<Option<SegmentName>>>,
    /// 已核对出放不进：之后不再核对
    unsupported: bool,
}

impl Mp4Check {
    /// `plan` 为录制开始时任务目录的合并计划：它的第 0 组即合并时的第 0 组，之前几次运行录的组不再核对。
    pub(super) fn new(streams: Vec<Streams>, plan: &MergePlan) -> Self {
        let first = plan.keys.first().map(|&key| {
            let heads = plan.groups[0]
                .iter()
                .map(|t| GroupTrack {
                    init: t.init.clone(),
                    segments: t.segments[..1].to_vec(),
                })
                .collect();
            (key, heads)
        });
        Mp4Check {
            streams,
            first,
            complete: plan.keys.iter().copied().collect(),
            pending: HashMap::new(),
            unsupported: false,
        }
    }

    /// 分片 `id` 录到了任务目录；它所在的组各轨都有分片时核对这一组，放不进 MP4 时返回原因。文件读写等其余失败上抛。
    pub(super) async fn recorded(
        &mut self,
        id: ItemInfo,
        layout: &Layout,
    ) -> Result<Option<Mp4Unsupported>, Error> {
        let key = id.name.group();
        if self.unsupported || self.complete.contains(&key) {
            return Ok(None);
        }
        let heads = self
            .pending
            .entry(key)
            .or_insert_with(|| vec![None; self.streams.len()]);
        heads[id.track].get_or_insert(id.name);
        if heads.iter().any(Option::is_none) {
            return Ok(None);
        }
        let group: Vec<GroupTrack> = self
            .pending
            .remove(&key)
            .into_iter()
            .flatten()
            .flatten()
            .enumerate()
            .map(|(track, name)| GroupTrack::of(layout, track, [&name]))
            .collect();
        self.complete.insert(key);
        // 合并时的组号：各轨都有分片的组按键排列。新录齐的组排在原来的第 0 组前面时（续录接上的会话里，之前没录齐
        // 的组这次录齐）它成为第 0 组
        let number = |key: &GroupKey| self.complete.range(..key).count();
        let groups = match self.first.take() {
            None => vec![(0, group.clone())],
            Some((first_key, first)) if key < first_key => {
                vec![(0, group.clone()), (number(&first_key), first)]
            }
            Some((first_key, first)) => {
                let numbered = vec![(0, first.clone()), (number(&key), group.clone())];
                self.first = Some((first_key, first));
                numbered
            }
        };
        self.first.get_or_insert((key, group));
        let streams = self.streams.clone();
        match blocking(move || output::check_mp4(&streams, &groups)).await? {
            Ok(()) => Ok(None),
            Err(Error::Unsupported(Unsupported::Mp4(reason))) => {
                self.unsupported = true;
                Ok(Some(reason))
            }
            Err(e) => Err(e),
        }
    }
}
