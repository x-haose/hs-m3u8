//! 换上输出：把备齐的临时输出装到输出路径，可撤回；中断与失败留下的按任务目录里的记录收拾。
//!
//! 每个输出有两个与它同级的保留名（见 [`pending`]）：临时输出，与旧输出挪开后的名字。同一任务目录同一时间只有
//! 一个任务（任务目录的锁），这两个名字上的东西只可能是这个任务目录的任务留下的，不看内容，按同一条规则收拾：
//! 临时输出删掉；挪开的旧输出，原处空着就放回（不替换已有的），原处已有就删掉。只有要替换时才挪开旧输出，原处
//! 已有说明更新的输出已经装上（本任务的，或同时写同一输出的别的任务的）。
//!
//! 写临时输出之前把各输出记进任务目录（[`Commit::begin`]）；之后每次运行在任务开头、检查输出之前，按记录与本次
//! 各输出的名字收拾一遍（[`recover`]）。换上时只挪开检查认可替换的东西，装上与放回都不替换已有的：几个任务同时
//! 写同一输出，先装上的成功，后到的报已存在。任一步失败都撤回，每一项都尽量做完；没做完的（没能放回的旧输出、
//! 没删掉的临时输出）留在记录里，下次接着收拾。
//!
//! 简化：进程在换的中途（几次改名之间）被杀时不续完，MP4 与 HLS 可能一新一旧，重新运行时要求覆盖即可。换的过程
//! 变长（如跨文件系统改名）时，把每一步记进任务目录、下次运行续完。

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::options::sibling;
use crate::entries;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::workdir::{self, OutputKind, PendingOutput};
use crate::{Error, Leftover, LeftoverKind};

/// 输出 `target` 的临时名与挪开旧输出的名字：与它同级，名为 `hsdl-<任务目录指纹>.<mp4|hls>.part` 与 `.old`。
/// 定长，不随输出名变长；以任务目录区分，几个任务输出到同一处时各写各的。
pub(super) fn pending(kind: OutputKind, target: PathBuf, work_dir: Fingerprint) -> PendingOutput {
    let ext = match kind {
        OutputKind::Mp4 => "mp4",
        OutputKind::Hls => "hls",
    };
    let name = |suffix: &str| OsString::from(format!("hsdl-{work_dir}.{ext}.{suffix}"));
    PendingOutput {
        kind,
        temp: sibling(&target, &name("part"), ""),
        aside: sibling(&target, &name("old"), ""),
        target,
    }
}

/// 检查时输出路径上已有的东西，决定换上时挪不挪开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Existing {
    Nothing,
    /// 要求覆盖、可以替换的旧输出
    Output,
    /// 没有内容的目录（系统自动生成的元数据文件不算），不要求覆盖也可以替换
    EmptyDir,
}

/// 收拾上次留下的：记录里的，以及本次各输出 `current` 的保留名上的（记录跟着任务目录删掉后，同名的残留仍会被
/// 收拾），规则见模块说明；都收拾完删掉记录。失败时记录留着，可以重试。任务目录存在时调用方须已加锁。
pub(super) fn recover(work_dir: &Path, current: &[PendingOutput]) -> Result<(), Error> {
    for o in workdir::read_outputs(work_dir)?.iter().chain(current) {
        settle(o)?;
    }
    workdir::clear_outputs(work_dir).map_err(io_error("删除", &workdir::outputs_file(work_dir)))
}

fn settle(o: &PendingOutput) -> Result<(), Error> {
    remove(&o.temp, o.kind).map_err(io_error("删除", &o.temp))?;
    if !exists(&o.aside).map_err(io_error("检查", &o.aside))? {
        return Ok(());
    }
    // 放回时原处刚被别人占了，同原处已有
    if !exists(&o.target).map_err(io_error("检查", &o.target))? {
        install(&o.aside, &o.target, o.kind).map_err(io_error("放回", &o.aside))?;
    }
    remove(&o.aside, o.kind).map_err(io_error("删除", &o.aside))
}

/// 本次正在写的输出：写临时输出之前记进任务目录，最后换上（[`Commit::swap`]）或放弃（[`Commit::abandon`]）。
pub(super) struct Commit<'a> {
    work_dir: &'a Path,
    outputs: &'a [PendingOutput],
}

impl<'a> Commit<'a> {
    pub(super) fn begin(work_dir: &'a Path, outputs: &'a [PendingOutput]) -> Result<Self, Error> {
        workdir::record_outputs(work_dir, outputs)?;
        Ok(Commit { work_dir, outputs })
    }

    /// 换上备齐的临时输出；`existing` 为换上前检查各输出的结果，顺序同各输出。成功时返回收尾没删掉的东西（被替换的
    /// 旧输出、临时名、记录）：输出已换好。删不掉的旧输出与临时名下次写同一输出时在任务开头再删。
    pub(super) fn swap(self, existing: &[Existing]) -> Result<Vec<Leftover>, Error> {
        let (mut moved, mut installed) = (Vec::new(), Vec::new());
        if let Err(failure) = self.move_and_install(existing, &mut moved, &mut installed) {
            return Err(self.undo(failure, &installed, &moved));
        }
        let mut leftovers = Vec::new();
        for o in moved {
            if let Err(e) = remove(&o.aside, o.kind) {
                leftovers.push(leftover(&o.aside, LeftoverKind::Removable, &e));
            }
        }
        self.remove_temps(&mut leftovers);
        self.clear_record(&mut leftovers);
        Ok(leftovers)
    }

    /// 写临时输出失败后放弃：删掉临时输出。
    pub(super) fn abandon(self, failure: Error) -> Error {
        self.undo(failure, &[], &[])
    }

    fn move_and_install(
        &self,
        existing: &[Existing],
        moved: &mut Vec<&'a PendingOutput>,
        installed: &mut Vec<&'a PendingOutput>,
    ) -> Result<(), Error> {
        for (o, &existing) in self.outputs.iter().zip(existing) {
            if existing == Existing::Nothing || !move_aside(o)? {
                continue;
            }
            moved.push(o);
            // 挪开的空目录已不为空：检查之后别的任务装上了它的输出，不是本任务可以替换的
            if existing == Existing::EmptyDir && !entries::is_empty(&o.aside)? {
                return Err(Error::OutputExists(o.target.clone()));
            }
        }
        for o in self.outputs {
            if !install(&o.temp, &o.target, o.kind).map_err(io_error("装上", &o.temp))? {
                return Err(Error::OutputExists(o.target.clone()));
            }
            installed.push(o);
        }
        Ok(())
    }

    /// 撤回到换之前，每一项都尽量做完：装上的新输出撤下，挪开的旧输出放回（不替换已有的），临时输出删掉。没能放回
    /// 的旧输出与没删掉的临时输出留在记录里，下次收拾（[`recover`]）；都做完了才删记录。
    fn undo(
        &self,
        failure: Error,
        installed: &[&PendingOutput],
        moved: &[&PendingOutput],
    ) -> Error {
        let mut leftovers = Vec::new();
        for o in installed.iter().rev() {
            if let Err(e) = remove(&o.target, o.kind) {
                leftovers.push(leftover(&o.target, LeftoverKind::Installed, &e));
            }
        }
        for o in moved.iter().rev() {
            let displaced = |cause: String| Leftover {
                path: o.aside.clone(),
                kind: LeftoverKind::Displaced {
                    target: o.target.clone(),
                },
                cause,
            };
            match install(&o.aside, &o.target, o.kind) {
                Ok(true) => {
                    // 文件是硬链接放回的，挪开的名字还在
                    if let Err(e) = remove(&o.aside, o.kind) {
                        leftovers.push(leftover(&o.aside, LeftoverKind::Removable, &e));
                    }
                }
                Ok(false) => leftovers.push(displaced("原处已被占用".to_owned())),
                Err(e) => leftovers.push(displaced(e.to_string())),
            }
        }
        self.remove_temps(&mut leftovers);
        if leftovers.iter().all(|l| l.kind == LeftoverKind::Installed) {
            self.clear_record(&mut leftovers);
        }
        if leftovers.is_empty() {
            failure
        } else {
            Error::Cleanup {
                failure: Box::new(failure),
                leftovers,
            }
        }
    }

    fn remove_temps(&self, leftovers: &mut Vec<Leftover>) {
        for o in self.outputs {
            if let Err(e) = remove(&o.temp, o.kind) {
                leftovers.push(leftover(&o.temp, LeftoverKind::Removable, &e));
            }
        }
    }

    fn clear_record(&self, leftovers: &mut Vec<Leftover>) {
        if let Err(e) = workdir::clear_outputs(self.work_dir) {
            let path = workdir::outputs_file(self.work_dir);
            leftovers.push(leftover(&path, LeftoverKind::Removable, &e));
        }
    }
}

fn leftover(path: &Path, kind: LeftoverKind, cause: &io::Error) -> Leftover {
    Leftover {
        path: path.to_path_buf(),
        kind,
        cause: cause.to_string(),
    }
}

/// 把输出路径上的东西挪到 `aside`；已经不在了（检查之后被删掉）时为 false。
fn move_aside(o: &PendingOutput) -> Result<bool, Error> {
    match fs::rename(&o.target, &o.aside) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(cause) => Err(io_error("挪开", &o.target)(cause)),
    }
}

/// 把 `from` 装到 `to`，不替换已有的：`to` 已被占用时为 false。文件用硬链接装，`from` 留着由调用方删除；文件系统
/// 不支持硬链接时改名，这时只在几个任务同时写同一输出的那一瞬会替换掉对方刚装上的。目录改名到非空目录失败；改名
/// 到空目录时替换它，没有可丢的。
fn install(from: &Path, to: &Path, kind: OutputKind) -> io::Result<bool> {
    let renamed = match kind {
        OutputKind::Mp4 => match fs::hard_link(from, to) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(_) => fs::rename(from, to),
        },
        OutputKind::Hls => fs::rename(from, to),
    };
    match renamed {
        Ok(()) => Ok(true),
        Err(_) if exists(to)? => Ok(false),
        Err(cause) => Err(cause),
    }
}

/// 删掉文件或目录；已不存在的不算失败。
fn remove(path: &Path, kind: OutputKind) -> io::Result<()> {
    let removed = match kind {
        OutputKind::Mp4 => fs::remove_file(path),
        OutputKind::Hls => fs::remove_dir_all(path),
    };
    match removed {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// 不跟随符号链接。
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立的空目录，其中 `work` 为任务目录。
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hs-m3u8-commit-{}-{name}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(dir.join("work")).unwrap();
        dir
    }

    fn outputs(dir: &Path) -> [PendingOutput; 2] {
        let fingerprint = Fingerprint::of_content(b"work");
        [
            pending(OutputKind::Mp4, dir.join("a.mp4"), fingerprint),
            pending(OutputKind::Hls, dir.join("a"), fingerprint),
        ]
    }

    /// 在 `o` 的 `path`（输出、临时名或挪开的名字）上放内容为 `text` 的文件，HLS 放在目录里的 `index.m3u8`。
    fn put(o: &PendingOutput, path: &Path, text: &str) {
        match o.kind {
            OutputKind::Mp4 => fs::write(path, text).unwrap(),
            OutputKind::Hls => {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("index.m3u8"), text).unwrap();
            }
        }
    }

    fn read(o: &PendingOutput, path: &Path) -> Option<String> {
        let file = match o.kind {
            OutputKind::Mp4 => path.to_path_buf(),
            OutputKind::Hls => path.join("index.m3u8"),
        };
        fs::read_to_string(file).ok()
    }

    fn assert_no_names_left(outputs: &[PendingOutput], work: &Path) {
        for o in outputs {
            for name in [&o.temp, &o.aside] {
                assert!(!name.exists(), "{}", name.display());
            }
        }
        assert_eq!(workdir::read_outputs(work).unwrap(), []);
    }

    #[test]
    fn replaced_outputs_are_swapped_and_old_ones_removed() {
        let dir = scratch("swap");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, &o.target, "旧");
            put(o, &o.temp, "新");
        }

        let commit = Commit::begin(&work, &outputs).unwrap();
        assert_eq!(workdir::read_outputs(&work).unwrap(), outputs);
        assert_eq!(commit.swap(&[Existing::Output; 2]).unwrap(), []);

        for o in &outputs {
            assert_eq!(read(o, &o.target).as_deref(), Some("新"));
        }
        assert_no_names_left(&outputs, &work);
    }

    /// 换上 HLS 失败（这里是准备目录不见了）：已装上的新 MP4 撤下，挪开的旧 MP4 与旧 HLS 都放回。
    #[test]
    fn a_failed_swap_restores_the_old_outputs() {
        let dir = scratch("undo");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, &o.target, "旧");
        }
        put(&outputs[0], &outputs[0].temp, "新");

        let commit = Commit::begin(&work, &outputs).unwrap();
        let failure = commit.swap(&[Existing::Output; 2]).unwrap_err();

        assert!(matches!(failure, Error::Io { .. }), "{failure}");
        for o in &outputs {
            assert_eq!(read(o, &o.target).as_deref(), Some("旧"));
        }
        assert_no_names_left(&outputs, &work);
    }

    /// 检查时输出还不存在、换上前别的任务装上了：不挪开、不替换，报已存在，别人的输出原样。
    #[test]
    fn an_output_installed_after_the_check_is_not_replaced() {
        let dir = scratch("taken");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, &o.temp, "新");
            put(o, &o.target, "别人的");
        }

        let commit = Commit::begin(&work, &outputs).unwrap();
        match commit.swap(&[Existing::Nothing; 2]) {
            Err(Error::OutputExists(path)) => assert_eq!(path, outputs[0].target),
            other => panic!("应报已存在：{other:?}"),
        }
        for o in &outputs {
            assert_eq!(read(o, &o.target).as_deref(), Some("别人的"));
        }
        assert_no_names_left(&outputs, &work);
    }

    /// 检查时是空目录、挪开之前别的任务装上了它的 HLS：挪开后发现不为空，放回去，报已存在。
    #[test]
    fn an_empty_directory_filled_after_the_check_is_put_back() {
        let dir = scratch("filled");
        let work = dir.join("work");
        let [_, hls] = outputs(&dir);
        put(&hls, &hls.temp, "新");
        put(&hls, &hls.target, "别人的");

        let outputs = [hls];
        let commit = Commit::begin(&work, &outputs).unwrap();
        assert!(matches!(
            commit.swap(&[Existing::EmptyDir]),
            Err(Error::OutputExists(_))
        ));
        assert_eq!(
            read(&outputs[0], &outputs[0].target).as_deref(),
            Some("别人的")
        );
        assert_no_names_left(&outputs, &work);
    }

    /// 撤回时原处已被别的任务占了（这里是 MP4：装上时发现别的任务刚装上）：不替换它，旧输出留在挪开的名字下、
    /// 记在残留与记录里，其余照常撤回；下次收拾时原处已有输出，删掉旧的。
    #[test]
    fn a_roll_back_does_not_replace_and_keeps_what_it_could_not_put_back() {
        let dir = scratch("displaced");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        for o in &outputs {
            put(o, &o.aside, "旧");
            put(o, &o.temp, "新");
        }
        put(mp4, &mp4.target, "别人的");

        let commit = Commit::begin(&work, &outputs).unwrap();
        let failure = Error::OutputExists(mp4.target.clone());
        let Error::Cleanup { leftovers, .. } = commit.undo(failure, &[], &[mp4, hls]) else {
            panic!("应报没放回的旧输出");
        };

        let target = mp4.target.clone();
        let kinds: Vec<_> = leftovers.iter().map(|l| (&l.path, &l.kind)).collect();
        assert_eq!(kinds, [(&mp4.aside, &LeftoverKind::Displaced { target })]);
        assert_eq!(read(mp4, &mp4.target).as_deref(), Some("别人的"));
        assert_eq!(read(hls, &hls.target).as_deref(), Some("旧"));
        assert_eq!(workdir::read_outputs(&work).unwrap(), outputs);

        recover(&work, &[]).unwrap();
        assert_eq!(read(mp4, &mp4.target).as_deref(), Some("别人的"));
        assert_no_names_left(&outputs, &work);
    }

    /// 上次中断留下的：临时输出删掉；挪开的旧输出原处空着就放回，原处已有新的就删掉。记录丢了的（这里 HLS
    /// 不在记录里）按本次各输出的名字一样收拾。
    #[test]
    fn interrupted_swaps_are_recovered() {
        let dir = scratch("recover");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        put(mp4, &mp4.temp, "半个");
        put(mp4, &mp4.aside, "旧");
        put(hls, &hls.temp, "半个");
        put(hls, &hls.aside, "旧");
        put(hls, &hls.target, "新");
        workdir::record_outputs(&work, std::slice::from_ref(mp4)).unwrap();

        recover(&work, &outputs).unwrap();

        assert_eq!(read(mp4, &mp4.target).as_deref(), Some("旧"));
        assert_eq!(read(hls, &hls.target).as_deref(), Some("新"));
        assert_no_names_left(&outputs, &work);
    }
}
