//! 任务编排：点播下载、直播录制、直播只合并三条流程，共用写出与收尾。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_remux::Streams;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::Fetcher;
use crate::http::{Http, Permit};
use crate::info::Selected;
use crate::live::{self, Context, Outcome};
use crate::output::{self, Content, ResolvedOutput};
use crate::request::JobRequest;
use crate::resolve::{self, Resolved, SubtitleScope};
use crate::selection::track_kinds;
use crate::vod::{self, Plan};
use crate::workdir::{
    self, HlsRecord, Lock, RecordKind, Recorded, SelectionCheck, Stored, WorkDir,
    preference_digest, read_resumable, url_digest,
};
use crate::{
    Error, JobType, Leftover, LeftoverKind, LiveReport, Output, Progress, Stage, TailCause,
    UnrecordedTail, Unsettled, WorkDirProblem, blocking,
};

/// 一次运行用到的共享对象。
struct Task {
    request: JobRequest,
    /// 校验过的 `request.output`
    output: ResolvedOutput,
    http: Arc<Http>,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
}

pub(crate) async fn run(
    request: JobRequest,
    output: ResolvedOutput,
    http: Http,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    let lock = prepare(&output).await?;
    let task = Task {
        request,
        output,
        http: Arc::new(http),
        cancel,
        stop,
        progress,
    };
    let (request, root) = (&task.request, &task.output.work_dir);
    let preference = preference_digest(&request.source.preference);
    // 任务目录里有可续的记录时，选轨偏好不符在联网之前就报（WorkDir::open 加锁后照样核对）；相符时按记录的选轨找回同一
    // 条轨（重新选轨时两样都不做，按这次的偏好选）。记录的是直播且请求开启了直播时按直播继续，即使播放列表已出现
    // ENDLIST（中断期间直播结束了）。没开启直播时按点播运行，由 WorkDir::open 报类型不符；记录的是文件任务时同样由它
    // 报：本次是点播还是直播要拉了播放列表才知道
    let recorded = match read_resumable(root.clone()).await? {
        Some(Recorded::Hls(record)) => Some(*record),
        Some(Recorded::File(_)) | None => None,
    };
    if !request.reselect
        && let Some(problem) = recorded
            .as_ref()
            .and_then(|r| r.preference_conflict(&preference))
    {
        return Err(Error::WorkDir {
            path: root.clone(),
            problem,
        });
    }
    let continuing_live = recorded
        .as_ref()
        .is_some_and(|r| matches!(r.kind, RecordKind::Live { .. }));
    let selection = recorded
        .as_ref()
        .filter(|_| !request.reselect)
        .and_then(|r| r.selection.as_ref());
    let resolved = resolve::resolve(
        &task.http,
        &request.source,
        selection,
        SubtitleScope::Selected,
        &task.cancel,
    )
    .await?
    .ok_or_else(|| Error::WorkDir {
        path: root.clone(),
        problem: WorkDirProblem::SelectionGone,
    })?;
    let selection = resolved.master.as_ref().map(Selected::of);
    let kinds = track_kinds(&resolved.streams());
    task.progress.send_modify(|p| {
        p.selected = selection;
        p.set_tracks(&kinds);
    });
    if resolved.is_live() || continuing_live && request.live.is_some() {
        run_live(task, preference, resolved, lock).await
    } else {
        run_vod(task, preference, resolved, lock).await
    }
}

/// 任务开头、联网之前：先查各输出的路径建不建得出来（见 [`output::check_creatable`]），任务目录已存在时加锁（挡住
/// 同时使用它的任务），收拾上次写输出留下的，再检查各输出能否写（见 [`output::prepare`]）。返回的锁交给
/// [`WorkDir::open`]（文件任务为 [`workdir::FileDir::open`]）。
pub(crate) async fn prepare(output: &ResolvedOutput) -> Result<Option<Lock>, Error> {
    let options = output.clone();
    blocking(move || output::check_creatable(&options)).await??;
    let lock = WorkDir::lock_existing(output.work_dir.clone()).await?;
    let output = output.clone();
    blocking(move || output::prepare(&output)).await??;
    Ok(lock)
}

/// 按任务目录记录的选轨加上另存的字幕文件（见 [`ResolvedOutput::with_subtitle_files`]），并在下载之前核对它们（见
/// [`output::check_more`]）。
async fn with_subtitle_files(
    output: &ResolvedOutput,
    dir: &WorkDir,
) -> Result<ResolvedOutput, Error> {
    let subtitles: Vec<(Option<&str>, bool)> = dir
        .record()
        .selection
        .iter()
        .flat_map(|s| &s.subtitles)
        .map(|s| (s.language.as_deref(), s.forced))
        .collect();
    let extended = output.with_subtitle_files(&subtitles)?;
    let checked = extended.clone();
    blocking(move || output::check_more(&checked)).await??;
    Ok(extended)
}

/// 直播：录制，再合并任务目录中录到的全部分片。
async fn run_live(
    mut task: Task,
    preference_digest: String,
    resolved: Resolved,
    lock: Option<Lock>,
) -> Result<Output, Error> {
    let options = task.request.live.ok_or(Error::LiveNotEnabled)?;
    let custom_key = task.request.key.is_some();
    for t in &resolved.tracks {
        resolve::require_key_uris(&t.url, &t.playlist.segments, custom_key)?;
    }
    let record = HlsRecord {
        preference_digest,
        url_digest: url_digest(&task.request.source.url),
        selection: resolved.selection_record(),
        kind: RecordKind::Live { tails: Vec::new() },
    };
    let dir = WorkDir::open(
        task.output.work_dir.clone(),
        record,
        lock,
        task.selection_check(),
    )
    .await?;
    task.output = with_subtitle_files(&task.output, &dir).await?;
    let mut fetcher = task.fetcher(&dir, Permit::Live);
    let ctx = Context {
        http: task.http.clone(),
        hooks: task.request.source.hooks.clone(),
        dir: &dir,
        fetcher: &mut fetcher,
        progress: &task.progress,
        cancel: &task.cancel,
        stop: &task.stop,
    };
    let merge = task
        .output
        .merged()
        .map(|(_, container)| (dir.record().streams(), container));
    let recording = live::record(ctx, resolved.tracks, options, merge).await?;
    let streams = dir.record().streams();
    let stored = dir.scan(streams.len()).await?;
    let input = merge_live(&dir, &stored, streams, Some(recording))?;
    task.finish(dir, input).await
}

/// 点播：下载计划中尚未完成的部分，再合并。
async fn run_vod(
    mut task: Task,
    preference_digest: String,
    resolved: Resolved,
    lock: Option<Lock>,
) -> Result<Output, Error> {
    let streams = resolved.streams();
    let selection = resolved.selection_record();
    let plan = Plan::new(resolved.tracks, &streams, task.request.key.is_some())?;
    let record = HlsRecord {
        preference_digest,
        url_digest: url_digest(&task.request.source.url),
        selection,
        kind: RecordKind::Vod {
            plan_digest: plan.digest(),
        },
    };
    let dir = WorkDir::open(
        task.output.work_dir.clone(),
        record,
        lock,
        task.selection_check(),
    )
    .await?;
    task.output = with_subtitle_files(&task.output, &dir).await?;
    let mut fetcher = task.fetcher(&dir, Permit::Vod);
    let merge = task
        .output
        .merged()
        .map(|(_, container)| (streams.as_slice(), container));
    let groups = vod::download(
        &mut fetcher,
        &plan,
        &dir,
        merge,
        &task.progress,
        &task.cancel,
    )
    .await?;
    let input = Content {
        streams,
        groups,
        selection: dir.record().selection.clone(),
        missed: plan.missed(),
        live: None,
    };
    task.finish(dir, input).await
}

/// 不联网，只合并任务目录中已录到的直播分片。
pub(crate) async fn merge_recorded(
    output: ResolvedOutput,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    let lock = prepare(&output).await?;
    let root = output.work_dir.clone();
    let record = match read_resumable(root.clone()).await? {
        None => return Err(Error::NothingRecorded),
        Some(Recorded::Hls(record)) => *record,
        Some(Recorded::File(_)) => {
            return Err(Error::WorkDir {
                path: root,
                problem: WorkDirProblem::KindMismatch {
                    recorded: JobType::File,
                    current: JobType::Live,
                },
            });
        }
    };
    if let RecordKind::Vod { .. } = record.kind {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::NotLiveRecording,
        });
    }
    let dir = WorkDir::open(root, record, lock, SelectionCheck::SameTracks).await?;
    let streams = dir.record().streams();
    let stored = dir.scan(streams.len()).await?;
    // 录到了字幕分片才要求字幕有去处：一片也没录到时什么都不会丢
    let recorded_subtitles = streams
        .iter()
        .zip(&stored.segments)
        .any(|(s, segments)| *s == Streams::Subtitles && !segments.is_empty());
    if recorded_subtitles {
        output.require_subtitle_use()?;
    }
    let output = with_subtitle_files(&output, &dir).await?;
    let kinds = track_kinds(&streams);
    progress.send_modify(|p| p.set_tracks(&kinds));
    live::count_stored(&stored, &progress);
    let content = merge_live(&dir, &stored, streams, None)?;
    finish(dir, content, &output, &cancel, &progress).await
}

/// 放弃任务（见 [`crate::Engine::discard`]）：加锁，按记录收拾写到一半的输出，再删除任务目录（见
/// [`workdir::remove`]）。`cancel` 在步与步之间取消时停下，报 [`Error::Cancelled`]，锁随即放开。
pub(crate) async fn discard(
    root: PathBuf,
    unsettled: Unsettled,
    cancel: &CancellationToken,
) -> Result<Vec<Leftover>, Error> {
    let lock = match WorkDir::lock_existing(root.clone()).await {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(Vec::new()),
        Err(failure @ Error::VolumeUnavailable { .. }) if unsettled == Unsettled::Report => {
            return Ok(vec![Leftover {
                path: root,
                kind: LeftoverKind::WorkDir,
                cause: failure.to_string(),
            }]);
        }
        Err(failure) => return Err(failure),
    };
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let work_dir = root.clone();
    // 锁随收拾进阻塞线程：调用方中途丢弃这个 future 时，收拾照常做完才放锁，别的任务不会与它同时收拾
    let (mut leftovers, lock) = blocking(move || {
        output::recover_recorded(&work_dir, unsettled).map(|leftovers| (leftovers, lock))
    })
    .await??;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    leftovers.extend(workdir::remove(root, lock, cancel).await?);
    Ok(leftovers)
}

/// 第 `session` 个会话里第 `track` 条轨合并进输出的最后一个分片的结束时刻（见 [`UnrecordedTail::at`]）：之前各组按
/// 声明时长（音视频轨中最长的）首尾相接，加上这一组里这条轨各分片的声明时长；这个会话里它没有分片合并进输出时为 None。
fn tail_at(
    plan: &live::MergePlan,
    streams: &[Streams],
    session: u32,
    track: usize,
) -> Option<Duration> {
    let last = plan
        .keys
        .iter()
        .zip(&plan.groups)
        .rposition(|((s, _), group)| *s == session && !group[track].segments.is_empty())?;
    let declared =
        |segments: &[output::GroupSegment]| -> u64 { segments.iter().map(|s| s.duration_us).sum() };
    let before: u64 = plan.groups[..last]
        .iter()
        .map(|group| {
            group
                .iter()
                .zip(streams)
                .filter(|(_, s)| **s != Streams::Subtitles)
                .map(|(t, _)| declared(&t.segments))
                .max()
                .unwrap_or(0)
        })
        .sum();
    Some(Duration::from_micros(
        before + declared(&plan.groups[last][track].segments),
    ))
}

/// 合并直播录到的分片；`recording` 为本次运行的录制，只合并时为 None。
fn merge_live(
    dir: &WorkDir,
    stored: &Stored,
    streams: Vec<Streams>,
    recording: Option<Outcome>,
) -> Result<Content, Error> {
    let plan = live::merge_plan(&stored.segments, &dir.record().streams(), dir.layout())?;
    if plan.groups.is_empty() {
        return Err(Error::NothingRecorded);
    }
    let (end, known, new_sessions, causes) = match recording {
        Some(r) => {
            let causes: Vec<(u32, usize, TailCause)> = match r.session {
                Some(session) => r
                    .tails
                    .into_iter()
                    .map(|(track, cause)| (session, track, cause))
                    .collect(),
                None => Vec::new(),
            };
            (Some(r.end), r.missed, r.new_sessions, causes)
        }
        None => (None, Vec::new(), Vec::new(), Vec::new()),
    };
    let unrecorded_tails = dir
        .tails()
        .iter()
        .map(|&(session, track)| UnrecordedTail {
            session,
            track,
            at: tail_at(&plan, &streams, session, track),
            after: stored.segments[track]
                .iter()
                .filter(|f| f.name.session == session)
                .map(|f| f.name.sequence)
                .max(),
            cause: causes
                .iter()
                .find(|(s, t, _)| (*s, *t) == (session, track))
                .map(|(_, _, cause)| cause.clone()),
        })
        .collect();
    let report = LiveReport {
        end,
        session_count: plan.sessions,
        new_sessions,
        unrecorded_tails,
    };
    Ok(Content {
        streams,
        missed: live::report_missed(&plan, known),
        groups: plan.groups,
        selection: dir.record().selection.clone(),
        live: Some(report),
    })
}

impl Task {
    /// 打开任务目录时怎样比较所选的轨：重新选轨时只要求轨道结构相同。
    fn selection_check(&self) -> SelectionCheck {
        if self.request.reselect {
            SelectionCheck::SameLayout
        } else {
            SelectionCheck::SameTracks
        }
    }

    /// 下载任务目录 `dir` 的分片的下载器；`permit` 见 [`Fetcher::new`]。
    fn fetcher(&self, dir: &WorkDir, permit: Permit) -> Fetcher {
        Fetcher::new(
            self.http.clone(),
            self.request.source.hooks.clone(),
            self.request.key,
            self.request.concurrency,
            dir,
            permit,
            &self.cancel,
        )
    }

    async fn finish(self, dir: WorkDir, content: Content) -> Result<Output, Error> {
        finish(dir, content, &self.output, &self.cancel, &self.progress).await
    }
}

/// 写出输出，按选项删除任务目录。取消在换上输出之前都生效（见 [`output::write`]）。
async fn finish(
    dir: WorkDir,
    content: Content,
    options: &ResolvedOutput,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    progress.send_modify(|p| p.stage = Stage::Writing);
    let bytes = progress.borrow().bytes;
    let output = output::write(dir, content, options, bytes, cancel, progress).await?;
    progress.send_modify(|p| p.stage = Stage::Done);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{GroupSegment, GroupTrack};

    fn track(durations: &[u64]) -> GroupTrack {
        GroupTrack {
            init: None,
            segments: durations
                .iter()
                .map(|&duration_us| GroupSegment {
                    path: PathBuf::new(),
                    duration_us,
                })
                .collect(),
            gaps: Vec::new(),
        }
    }

    /// 尾部的时刻：之前各组按音视频轨中最长的声明时长首尾相接，加上这一组里字幕轨的；这个会话里字幕轨没有分片的组不算。
    #[test]
    fn tail_times_add_up_the_groups_before() {
        let streams = [Streams::Video, Streams::Audio, Streams::Subtitles];
        let plan = live::MergePlan {
            groups: vec![
                vec![track(&[4, 4]), track(&[9]), track(&[4])],
                vec![track(&[2]), track(&[2]), track(&[1, 1])],
                vec![track(&[5]), track(&[5]), track(&[])],
                vec![track(&[3]), track(&[3]), track(&[])],
            ],
            keys: vec![(0, 0), (0, 1), (0, 2), (1, 0)],
            sessions: 2,
            unmergeable: Vec::new(),
            holes: Vec::new(),
        };
        let at = |session| tail_at(&plan, &streams, session, 2);
        assert_eq!(at(0), Some(Duration::from_micros(9 + 2)));
        assert_eq!(at(1), None);
    }
}
