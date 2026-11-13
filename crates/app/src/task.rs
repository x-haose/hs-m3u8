//! 任务：用户新建的一次下载或录制，及其在存储里的状态。

use std::path::PathBuf;

use hs_m3u8_core::{OutputOptions, Target, Url};

use crate::preference::StoredPreference;

/// 任务存储里的编号，新建时由 SQLite 分配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub i64);

/// 新建任务时给定、之后不变的内容：续传与续录照它重建请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// 列表里显示的名字
    pub title: String,
    pub url: Url,
    pub preference: StoredPreference,
    /// 额外的请求头，原样存储（含凭据时同样明文，存储文件只有本人可读写）
    pub headers: Vec<(String, String)>,
    pub output: OutputSpec,
    /// 来源是直播时录制；点播时不起作用
    pub live: bool,
}

/// 输出：路径都是以文件名结尾的绝对路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSpec {
    Mp4(PathBuf),
    Hls(PathBuf),
    Both { mp4: PathBuf, hls: PathBuf },
}

impl OutputSpec {
    /// 核心的输出选项：任务目录取默认位置（与输出同级的 `<主名>.hsdl`），重开应用时由同一输出路径找回。
    pub fn options(&self) -> OutputOptions {
        OutputOptions::new(match self.clone() {
            OutputSpec::Mp4(path) => Target::Mp4(path),
            OutputSpec::Hls(path) => Target::Hls(path),
            OutputSpec::Both { mp4, hls } => Target::Both { mp4, hls },
        })
    }
}

/// 任务在存储里的状态。运行中的细节（进度、阶段）不存，见 [`crate::Event`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// 在下载或录制
    Running,
    /// 用户暂停；任务目录保留，继续时续传。应用异常退出时还在运行的，重开后也是这个状态
    Paused,
    /// 以错误结束，值为错误的说明；任务目录保留
    Failed(String),
    /// 已写出输出
    Done,
}

/// 存储里的一个任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub spec: TaskSpec,
    pub status: Status,
    /// 新建时间，Unix 秒
    pub created_at: i64,
}
