//! 输出：写什么、写到哪里，以及放弃任务时收拾不了的怎么办。

use std::hash::Hash;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::gen_stub_pymethods;

use crate::convert::PyPath;
use crate::error::to_pyerr;
use crate::record::{record, variants};

variants! {
    /// 输出什么。各路径与任务目录须以文件名结尾，不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时
    /// 在任务开头与写出前创建；相对路径按开始任务时的当前目录补全，之后改变当前目录不影响任务；补全后最后一段变了
    /// （Windows 去掉结尾的点与空格）时报参数错误。
    pub enum Target {
        /// MP4 文件，各路流原样复制、不重编码。MP3、MP2 音轨放进 MP4 后，苹果基于 AVFoundation 的播放器（QuickTime
        /// Player、Safari 等）认不出
        Mp4 {
            /// MP4 的路径
            path: PyPath,
        },
        /// 可直接播放的本地 HLS 目录：不经 FFmpeg，原样保留分片（已解密），不受 MP4 合并对编码的限制。入口为其中的
        /// `index.m3u8`，布局见 `Output.hls`
        Hls {
            /// 目录的路径
            path: PyPath,
        },
        /// 两者都要，内容相同
        #[pyo3(constructor = (*, mp4, hls))]
        Both {
            /// MP4 的路径
            mp4: PyPath,
            /// 本地 HLS 目录的路径
            hls: PyPath,
        },
    }
}

record! {
    @custom_new
    /// 输出到哪里、任务目录在哪里；下载（`JobRequest.output`）与只合并（`Engine.merge_recorded`）共用。
    pub struct OutputOptions {
        /// 输出什么
        pub target: Target,
        /// 任务目录；None 时与输出同级，名为输出的主名加 `.hsdl`：主名为 MP4 文件名去掉扩展名，只输出 HLS 时为 HLS
        /// 目录名。`a.mp4` 与 HLS 目录 `a` 共用 `a.hsdl`，改变输出种类后接着用已下载的分片。只能是空目录、不存在的目录
        /// 或本库建立的任务目录
        pub work_dir: Option<PyPath>,
        /// 输出已存在时替换：MP4 文件直接替换；HLS 目录只在其中全是本库写出的文件时替换。路径被别的东西占用（见
        /// `OutputOccupiedError`）时覆盖也不替换。为 False 时 HLS 目录须不存在或为空（系统自动生成的元数据文件
        /// 不算），否则报 `OutputExistsError`
        pub overwrite: bool,
        /// 成功后保留任务目录
        pub keep_work_dir: bool,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl OutputOptions {
    /// 各参数即同名属性。
    #[new]
    #[pyo3(signature = (target, *, work_dir = None, overwrite = false, keep_work_dir = false))]
    fn new(target: Target, work_dir: Option<PyPath>, overwrite: bool, keep_work_dir: bool) -> Self {
        OutputOptions {
            target,
            work_dir,
            overwrite,
            keep_work_dir,
        }
    }

    /// 实际使用的任务目录：`work_dir`，未指定时见 `work_dir` 的说明；与开始任务时一样补全为绝对路径、校验，不合
    /// 要求时抛 `InvalidInputError`。
    fn resolved_work_dir(&self, py: Python<'_>) -> PyResult<PyPath> {
        self.to_core()
            .resolved_work_dir()
            .map(PyPath::from)
            .map_err(|error| to_pyerr(py, error))
    }
}

variants! {
    /// 放弃任务（`Engine.discard`）时，任务目录或记录里的输出所在的盘读不到，或收拾输出出错，怎么办。
    pub enum Unsettled {
        /// 报错（盘读不到时为 `VolumeUnavailableError`），任务目录与记录都留着，接上或排除后重试
        Fail {},
        /// 记为残留，照常放弃：每个收拾不了的输出，它旁边可能留着的写了一半的临时输出与替换时挪开的旧输出各记一项
        /// （盘读不到时核实不了，不一定还在），怎么处理见 `Leftover.kind`；任务目录本身读不到时记一项
        /// `LeftoverKind.WorkDir`。用于盘不再接上、用户确认仍要放弃时
        Report {},
    }
}

impl Target {
    fn to_core(&self) -> core::Target {
        match self {
            Target::Mp4 { path } => core::Target::Mp4(path.0.clone()),
            Target::Hls { path } => core::Target::Hls(path.0.clone()),
            Target::Both { mp4, hls } => core::Target::Both {
                mp4: mp4.0.clone(),
                hls: hls.0.clone(),
            },
        }
    }
}

impl OutputOptions {
    pub(crate) fn to_core(&self) -> core::OutputOptions {
        let mut options = core::OutputOptions::new(self.target.to_core());
        options.work_dir = self.work_dir.as_ref().map(|dir| dir.0.clone());
        options.overwrite = self.overwrite;
        options.keep_work_dir = self.keep_work_dir;
        options
    }
}

impl From<&Unsettled> for core::Unsettled {
    fn from(unsettled: &Unsettled) -> Self {
        match unsettled {
            Unsettled::Fail {} => core::Unsettled::Fail,
            Unsettled::Report {} => core::Unsettled::Report,
        }
    }
}
