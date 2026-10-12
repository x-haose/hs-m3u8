//! 录制中核对能否合并进 MP4（见 [`output::check_mp4`]）。放不进 MP4 的原因交给调用方放进进度
//! （[`crate::Progress::mp4_unsupported`]），录制照常进行：直播错过的内容录不回来，停不停由调用方定；合并 MP4 时照样
//! 报错。
//! - 录制开始时任务目录里已录齐的组（之前几次运行录的）在后台一起核对一次。
//! - 之后每个（会话, 不连续段）组在各轨都有分片时（含之前录了一部分、这次才录齐的），拿各轨的第一个分片与合并时的第 0
//!   组一起核对一次；第 0 组开头的分片判断不了时只核对这一组自身。报的组号为核对时的：之后才录齐、排在前面的组会让
//!   合并时的组号变大。

use std::collections::{BTreeSet, HashMap};

use hs_m3u8_remux::Streams;

use super::merge::MergePlan;
use crate::fetch::ItemInfo;
use crate::output::{self, GroupTrack};
use crate::workdir::{GroupKey, Layout, SegmentName, Stored};
use crate::{Error, Mp4Unsupported, Unsupported, blocking};

pub(super) struct Mp4Check {
    /// 各轨的取流方式
    streams: Vec<Streams>,
    /// 合并时的第 0 组（各轨都有分片的组里键最小的）的键与各轨开头的分片；还没有这样的组时为 None
    first: Option<(GroupKey, Vec<GroupTrack>)>,
    /// 各轨都有分片的组：录制开始时任务目录里的与本次录齐的
    complete: BTreeSet<GroupKey>,
    /// 还没录齐的组里各轨的第一个分片：录制开始时任务目录里的，加上本次录到的
    pending: HashMap<GroupKey, Vec<Option<SegmentName>>>,
    /// 已核对出放不进：之后不再核对
    unsupported: bool,
}

impl Mp4Check {
    /// `plan` 为录制开始时任务目录的合并计划，`stored` 为目录里的分片。另返回目录里已录齐的组（各轨的第一个分片与
    /// 合并时的组号），由调用方交给后台一起核对一次；没有时为空。
    pub(super) fn new(
        streams: Vec<Streams>,
        plan: &MergePlan,
        stored: &Stored,
    ) -> (Self, Vec<(usize, Vec<GroupTrack>)>) {
        let recorded: Vec<(usize, Vec<GroupTrack>)> = plan
            .groups
            .iter()
            .map(|tracks| {
                tracks
                    .iter()
                    .map(|t| GroupTrack {
                        init: t.init.clone(),
                        segments: t.segments[..1].to_vec(),
                    })
                    .collect()
            })
            .enumerate()
            .collect();
        let complete: BTreeSet<GroupKey> = plan.keys.iter().copied().collect();
        let mut pending: HashMap<GroupKey, Vec<Option<SegmentName>>> = HashMap::new();
        for (track, files) in stored.segments.iter().enumerate() {
            for file in files.iter().filter(|f| !complete.contains(&f.name.group())) {
                pending
                    .entry(file.name.group())
                    .or_insert_with(|| vec![None; streams.len()])[track]
                    .get_or_insert(file.name);
            }
        }
        let check = Mp4Check {
            first: plan
                .keys
                .first()
                .copied()
                .zip(recorded.first().map(|(_, heads)| heads.clone())),
            streams,
            complete,
            pending,
            unsupported: false,
        };
        (check, recorded)
    }

    /// 各轨的取流方式。
    pub(super) fn streams(&self) -> &[Streams] {
        &self.streams
    }

    /// 分片 `id` 录到了任务目录；它所在的组各轨都有分片时核对这一组，放不进 MP4 时返回原因（见 [`Mp4Check::judged`]）。
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
        let root = layout.root().to_path_buf();
        let result = blocking(move || output::check_mp4(&streams, &groups, &root)).await?;
        self.judged(result)
    }

    /// 一次核对的结果：放不进 MP4 时返回原因，之后不再核对；文件读写等其余失败上抛。
    pub(super) fn judged(
        &mut self,
        result: Result<(), Error>,
    ) -> Result<Option<Mp4Unsupported>, Error> {
        match result {
            Ok(()) => Ok(None),
            Err(Error::Unsupported(Unsupported::Mp4(reason))) => {
                self.unsupported = true;
                Ok(Some(reason))
            }
            Err(e) => Err(e),
        }
    }
}
