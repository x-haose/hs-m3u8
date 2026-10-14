//! 换上输出：把备齐的临时输出装到输出路径，可撤回；中断与失败留下的按任务目录里的记录收拾。
//!
//! 每个输出有两个与它同级的保留名（见 [`PendingOutput`]）：临时输出，与旧输出挪开后的名字。同一任务目录同一时间
//! 只有一个任务（任务目录的锁），这两个名字上的东西只可能是这个任务目录的任务留下的，收拾时不看内容：临时输出删掉；
//! 挪开的旧输出看记录：
//! - 换上还没做完（记录在、没记为已换好）：原处空着或是空目录就放回；原处是输出就删掉（只有要替换时才挪开旧输出，
//!   原处是输出说明更新的输出已经装上，本任务的或同时写同一输出的别的任务的）；原处是别的东西就留着，报出来；
//! - 已换好（记录记为已换好）：只删不放回；
//! - 没有记录：不动，报出来。记录在挪开的旧输出删净之后才删，没有记录的只可能是放弃任务时没能放回、已报给用户处理
//!   的旧输出（记录随任务目录删了），不知道该放回还是删掉。
//!
//! 「不存在」只在根读得到时才算没有（见 [`volume::reachable`]）：读不到时报错，记录留着，下次接着收拾。
//!
//! 写临时输出之前把各输出记进任务目录（[`Commit::begin`]），全部装上后改记为已换好，删净旧输出与临时输出后删记录。之后每次运行在任务
//! 开头、检查输出之前按记录与本次各输出的名字收拾一遍（[`recover`]）。换上时只挪开检查认可替换的东西，装上与放回都
//! 不替换已有的（例外见 [`install`]）：几个任务同时写同一输出，先装上的成功，后到的报已存在。任一步失败都撤回，每一项
//! 都尽量做完；没做完的留在记录里，下次接着收拾。
//!
//! 简化：进程在换的中途（几次改名之间）被杀时不续完，MP4 与 HLS 可能一新一旧，重新运行时要求覆盖即可。换的过程
//! 变长（如跨文件系统改名）时，把每一步记进任务目录、下次运行续完。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::hls;
use crate::error::io_error;
use crate::workdir::{self, OutputKind, PendingOutput};
use crate::{Error, Leftover, LeftoverKind, Unsettled, busy, entries, volume};

/// 输出路径上现在的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Occupant {
    /// 什么也没有，且建得出来：最近的已存在的上级是目录
    Nothing,
    /// 没有内容的目录（系统自动生成的元数据文件不算）
    EmptyDir,
    /// 输出：MP4 路径上的文件，或全是本库写出的文件的 HLS 目录
    Output,
    /// 别的东西：MP4 路径上的目录，HLS 路径上的文件或有别的文件的目录
    Other,
    /// 什么也没有，但建不出来：最近的已存在的上级不是目录（是文件，或悬空的符号链接）
    Blocked { ancestor: PathBuf },
}

/// 输出路径上现在的东西；输出路径本身是符号链接时不跟随。
fn occupant(o: &PendingOutput) -> Result<Occupant, Error> {
    let target = o.target();
    let meta = match fs::symlink_metadata(target) {
        Ok(meta) => meta,
        Err(e) if is_absent(&e) => return occupant_by_ancestor(target),
        Err(cause) => return Err(io_error("检查", target)(cause)),
    };
    match (o.kind(), meta.is_dir()) {
        (OutputKind::Mp4, false) => Ok(Occupant::Output),
        (OutputKind::Hls, true) => hls::occupant(target),
        _ => Ok(Occupant::Other),
    }
}

/// 不存在的输出路径看最近的已存在的上级：是目录（跟随符号链接）就建得出来，否则建不出来。上级有一段是文件时，
/// Unix 上报「不是目录」，Windows 上报「不存在」；上级是悬空的符号链接时报「不存在」，它本身却在。都在这里查清。
/// 根读不到时报错（见 [`volume::reachable`]）。
fn occupant_by_ancestor(target: &Path) -> Result<Occupant, Error> {
    let below_root = target.ancestors().skip(1).filter(|a| a.parent().is_some());
    for ancestor in below_root {
        match fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => return Ok(Occupant::Nothing),
            Ok(_) => {}
            // 跟随后不存在：没有这一项就再往上看，有（悬空的符号链接）就建不出来
            Err(e) if is_absent(&e) => {
                if !exists(ancestor)? {
                    continue;
                }
            }
            Err(cause) => return Err(io_error("检查", ancestor)(cause)),
        }
        let ancestor = ancestor.to_path_buf();
        return Ok(Occupant::Blocked { ancestor });
    }
    // 根以下各级都不存在：根读得到就建得出来
    volume::reachable(target).map(|()| Occupant::Nothing)
}

/// 输出能否写，能写时返回路径上现在的东西：空着、是空目录，或要求覆盖而路径上是输出。是输出而没有要求覆盖时报
/// [`Error::OutputExists`]；是别的东西或建不出来时报 [`Error::OutputOccupied`]，覆盖也不替换，以免路径给错时删掉
/// 别人的文件。
pub(super) fn check(o: &PendingOutput, overwrite: bool) -> Result<Occupant, Error> {
    match occupant(o)? {
        Occupant::Other => Err(occupied(o, None)),
        Occupant::Blocked { ancestor } => Err(occupied(o, Some(ancestor))),
        Occupant::Output if !overwrite => Err(Error::OutputExists(o.target().to_path_buf())),
        occupant => Ok(occupant),
    }
}

/// 输出路径建不建得出来：路径上没有东西时看最近的已存在的上级，建不出来报 [`Error::OutputOccupied`]（带上挡住的
/// 上级）；已有东西时不读它：加锁之前，同一任务目录的任务可能正在替换它。
pub(super) fn check_creatable(o: &PendingOutput) -> Result<(), Error> {
    if exists(o.target())? {
        return Ok(());
    }
    match occupant_by_ancestor(o.target())? {
        Occupant::Blocked { ancestor } => Err(occupied(o, Some(ancestor))),
        _ => Ok(()),
    }
}

fn occupied(o: &PendingOutput, ancestor: Option<PathBuf>) -> Error {
    Error::OutputOccupied {
        path: o.target().to_path_buf(),
        ancestor,
    }
}

/// 收拾上次留下的：先按记录，再按本次各输出 `current` 的名字（没有记录的），规则见模块说明。返回留着的旧输出：原处是
/// 别的东西的（留在记录里），与没有记录的；记录里的都收拾完时删掉记录。失败时记录留着，可以重试。任务目录存在时
/// 调用方须已加锁。
pub(super) fn recover(work_dir: &Path, current: &[PendingOutput]) -> Result<Vec<Leftover>, Error> {
    let record = workdir::read_outputs(work_dir)?;
    let mut kept = Vec::new();
    for o in &record.outputs {
        if !settle(o, !record.swapped)? {
            kept.push(unsettled(o));
        }
    }
    if kept.is_empty() {
        workdir::clear_outputs(work_dir)
            .map_err(io_error("删除", &workdir::outputs_file(work_dir)))?;
    }
    let recorded = |o: &PendingOutput| record.outputs.iter().any(|r| r.aside() == o.aside());
    for o in current.iter().filter(|o| !recorded(o)) {
        remove(o.temp(), o.kind())?;
        if exists(o.aside())? {
            kept.push(Leftover {
                path: o.aside().to_path_buf(),
                kind: LeftoverKind::Displaced {
                    target: o.target().to_path_buf(),
                },
                cause: "不在任务目录的记录里（放弃任务时没能放回），不知道该放回还是删掉"
                    .to_owned(),
            });
        }
    }
    Ok(kept)
}

/// 放弃任务时按记录收拾，规则同 [`recover`]，记录不删（随任务目录删掉）。返回留下的东西：原处是别的东西而没放回的
/// 旧输出各一项；收拾某个输出出错时，`mode` 为 [`Unsettled::Report`] 就把它的两个保留名记为残留（见 [`given_up`]）、
/// 接着收拾其余的，否则报错。要读写文件系统，在阻塞线程池中调用；调用方已对任务目录加锁。
pub(crate) fn recover_recorded(work_dir: &Path, mode: Unsettled) -> Result<Vec<Leftover>, Error> {
    let record = workdir::read_outputs(work_dir)?;
    let mut leftovers = Vec::new();
    for o in &record.outputs {
        match settle(o, !record.swapped) {
            Ok(true) => {}
            Ok(false) => leftovers.push(unsettled(o)),
            Err(failure) if mode == Unsettled::Report => {
                leftovers.extend(given_up(o, record.swapped, &failure));
            }
            Err(failure) => return Err(failure),
        }
    }
    Ok(leftovers)
}

/// 收拾不了的输出 `o` 的两个保留名，各记一项残留，不一定还在：临时输出可删；挪开的旧输出在全部装上之后（`swapped`）
/// 可删，之前是没能放回原处的旧输出。
fn given_up(o: &PendingOutput, swapped: bool, failure: &Error) -> [Leftover; 2] {
    let cause = format!("没收拾（不一定还在）：{failure}");
    let aside = if swapped {
        LeftoverKind::Removable
    } else {
        LeftoverKind::Displaced {
            target: o.target().to_path_buf(),
        }
    };
    [
        Leftover {
            path: o.temp().to_path_buf(),
            kind: LeftoverKind::Removable,
            cause: cause.clone(),
        },
        Leftover {
            path: o.aside().to_path_buf(),
            kind: aside,
            cause,
        },
    ]
}

/// 记录里的旧输出因原处是别的东西而留着，作为残留报出。
fn unsettled(o: &PendingOutput) -> Leftover {
    Leftover {
        path: o.aside().to_path_buf(),
        kind: LeftoverKind::Displaced {
            target: o.target().to_path_buf(),
        },
        cause: "原处已被别的东西占用".to_owned(),
    }
}

/// 收拾一个输出的保留名：临时输出删掉；挪开的旧输出 `restorable`（换上还没做完）时看原处放回或删掉，否则删掉。
/// 旧输出因原处是别的东西而留着时为 false。
fn settle(o: &PendingOutput, restorable: bool) -> Result<bool, Error> {
    remove(o.temp(), o.kind())?;
    if !exists(o.aside())? {
        return Ok(true);
    }
    if restorable {
        let empty = match occupant(o)? {
            Occupant::Other | Occupant::Blocked { .. } => return Ok(false),
            Occupant::Output => false,
            Occupant::EmptyDir => {
                remove_empty_dir(o.target())?;
                true
            }
            Occupant::Nothing => true,
        };
        // 放回时原处刚被别人装上了，同原处是输出：删掉旧的
        if empty {
            install(o.aside(), o.target(), o.kind(), "放回")?;
        }
    }
    remove(o.aside(), o.kind())?;
    Ok(true)
}

/// 删掉只有系统自动生成的元数据文件的目录；其间有了别的内容就失败。
fn remove_empty_dir(dir: &Path) -> Result<(), Error> {
    for entry in entries::list(dir)? {
        if entry.is_system_file() {
            remove(&entry.path, OutputKind::Mp4)?;
        }
    }
    fs::remove_dir(dir).map_err(io_error("删除", dir))
}

/// 本次正在写的输出：写临时输出之前记进任务目录，最后换上（[`Commit::swap`]）或放弃（[`Commit::abandon`]）。
pub(super) struct Commit<'a> {
    work_dir: &'a Path,
    outputs: &'a [PendingOutput],
}

impl<'a> Commit<'a> {
    pub(super) fn begin(work_dir: &'a Path, outputs: &'a [PendingOutput]) -> Result<Self, Error> {
        workdir::record_outputs(work_dir, outputs, false)?;
        Ok(Commit { work_dir, outputs })
    }

    /// 换上备齐的临时输出；`existing` 为换上前检查各输出的结果（见 [`check`]），顺序同各输出。成功时返回收尾没删掉
    /// 的东西（被替换的旧输出、临时名、记录）：输出已换好；旧输出与临时名删不掉时记录留着（已换好），下次用同一任务
    /// 目录运行时在任务开头再删。
    pub(super) fn swap(self, existing: &[Occupant]) -> Result<Vec<Leftover>, Error> {
        let (mut moved, mut installed) = (Vec::new(), Vec::new());
        if let Err(failure) = self.move_and_install(existing, &mut moved, &mut installed) {
            return Err(self.undo(failure, &installed, &moved));
        }
        let mut leftovers = Vec::new();
        for o in moved {
            if let Err(e) = remove(o.aside(), o.kind()) {
                leftovers.push(leftover(o.aside(), LeftoverKind::Removable, e));
            }
        }
        self.remove_temps(&mut leftovers);
        if leftovers.is_empty() {
            self.clear_record(&mut leftovers);
        }
        Ok(leftovers)
    }

    /// 写临时输出失败后放弃：删掉临时输出。
    pub(super) fn abandon(self, failure: Error) -> Error {
        self.undo(failure, &[], &[])
    }

    /// 挪开检查认可替换的东西，装上各临时输出，都装上后记为已换好（记不下来就算失败，撤回）。
    fn move_and_install(
        &self,
        existing: &[Occupant],
        moved: &mut Vec<&'a PendingOutput>,
        installed: &mut Vec<&'a PendingOutput>,
    ) -> Result<(), Error> {
        for (o, existing) in self.outputs.iter().zip(existing) {
            if *existing == Occupant::Nothing || !move_aside(o)? {
                continue;
            }
            moved.push(o);
            // 挪开的空目录已不为空：检查之后别的任务装上了它的输出，不是本任务可以替换的
            if *existing == Occupant::EmptyDir && !entries::is_empty(o.aside())? {
                return Err(Error::OutputExists(o.target().to_path_buf()));
            }
        }
        for o in self.outputs {
            if !install(o.temp(), o.target(), o.kind(), "装上")? {
                return Err(Error::OutputExists(o.target().to_path_buf()));
            }
            installed.push(o);
        }
        workdir::record_outputs(self.work_dir, self.outputs, true)
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
            if let Err(e) = remove(o.target(), o.kind()) {
                leftovers.push(leftover(o.target(), LeftoverKind::Installed, e));
            }
        }
        for o in moved.iter().rev() {
            let displaced = |cause: String| Leftover {
                path: o.aside().to_path_buf(),
                kind: LeftoverKind::Displaced {
                    target: o.target().to_path_buf(),
                },
                cause,
            };
            match install(o.aside(), o.target(), o.kind(), "放回") {
                Ok(true) => {
                    // 用硬链接放回时挪开的名字还在
                    if let Err(e) = remove(o.aside(), o.kind()) {
                        leftovers.push(leftover(o.aside(), LeftoverKind::Removable, e));
                    }
                }
                Ok(false) => leftovers.push(displaced("原处已被占用".to_owned())),
                Err(e) => leftovers.push(displaced(cause_text(e))),
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
            if let Err(e) = remove(o.temp(), o.kind()) {
                leftovers.push(leftover(o.temp(), LeftoverKind::Removable, e));
            }
        }
    }

    fn clear_record(&self, leftovers: &mut Vec<Leftover>) {
        if let Err(e) = workdir::clear_outputs(self.work_dir) {
            leftovers.push(Leftover {
                path: workdir::outputs_file(self.work_dir),
                kind: LeftoverKind::Removable,
                cause: e.to_string(),
            });
        }
    }
}

/// `path` 上没做完的一项，`failure` 为 [`remove`] 等报的错误。
fn leftover(path: &Path, kind: LeftoverKind, failure: Error) -> Leftover {
    Leftover {
        path: path.to_path_buf(),
        kind,
        cause: cause_text(failure),
    }
}

/// 残留的原因：读写错误只取原因（残留本身已带路径），根读不到等整句。
fn cause_text(failure: Error) -> String {
    match failure {
        Error::Io { cause, .. } => cause.to_string(),
        other => other.to_string(),
    }
}

/// 把输出路径上的东西挪到挪开的名字上；已经不在了（检查之后被删掉）时为 false。
fn move_aside(o: &PendingOutput) -> Result<bool, Error> {
    match busy::retrying(|| fs::rename(o.target(), o.aside())) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(cause) => Err(io_error("挪开", o.target())(cause)),
    }
}

/// 把 `from` 装到 `to`，不替换已有的：`to` 已被占用时为 false；失败时以 `action`（装上、放回）报 `from`。文件用硬链接
/// 装，`from` 留着由调用方删除；文件系统不支持硬链接时改名，这时只在几个任务同时写同一输出的那一瞬会替换掉对方刚装上
/// 的。目录改名到非空目录失败；改名到空目录时替换它，没有可丢的。
fn install(from: &Path, to: &Path, kind: OutputKind, action: &'static str) -> Result<bool, Error> {
    let renamed = match kind {
        OutputKind::Mp4 => match fs::hard_link(from, to) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(_) => busy::retrying(|| fs::rename(from, to)),
        },
        OutputKind::Hls => busy::retrying(|| fs::rename(from, to)),
    };
    match renamed {
        Ok(()) => Ok(true),
        Err(_) if exists(to)? => Ok(false),
        Err(cause) => Err(io_error(action, from)(cause)),
    }
}

/// 删掉文件或目录；已不存在的不算失败，根读不到时报错（见 [`volume::reachable`]）。
fn remove(path: &Path, kind: OutputKind) -> Result<(), Error> {
    let removed = busy::retrying(|| match kind {
        OutputKind::Mp4 => fs::remove_file(path),
        OutputKind::Hls => fs::remove_dir_all(path),
    });
    match removed {
        Ok(()) => Ok(()),
        Err(e) if is_absent(&e) => volume::reachable(path),
        Err(cause) => Err(io_error("删除", path)(cause)),
    }
}

/// 不跟随符号链接；不存在时根读不到即报错（见 [`volume::reachable`]）。
fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if is_absent(&e) => volume::reachable(path).map(|()| false),
        Err(cause) => Err(io_error("检查", path)(cause)),
    }
}

/// 路径不存在：没有这一项，或上级有一段是文件。
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ident::Fingerprint;
    use crate::scratch::Scratch;

    /// 每个测试独立的空目录，其中 `work` 为任务目录。
    fn scratch(name: &str) -> Scratch {
        let dir = Scratch::new(&format!("commit-{name}"));
        fs::create_dir_all(dir.join("work")).unwrap();
        dir
    }

    fn pending(kind: OutputKind, target: PathBuf) -> PendingOutput {
        PendingOutput::new(kind, target, Fingerprint::of_content(b"work"))
    }

    fn outputs(dir: &Path) -> [PendingOutput; 2] {
        [
            pending(OutputKind::Mp4, dir.join("a.mp4")),
            pending(OutputKind::Hls, dir.join("a")),
        ]
    }

    /// 在 `o` 的 `path`（输出路径或某个保留名）上放内容为 `text` 的输出，HLS 放在目录里的 `index.m3u8`。
    fn put(o: &PendingOutput, path: &Path, text: &str) {
        match o.kind() {
            OutputKind::Mp4 => fs::write(path, text).unwrap(),
            OutputKind::Hls => {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("index.m3u8"), text).unwrap();
            }
        }
    }

    fn read(o: &PendingOutput, path: &Path) -> Option<String> {
        let file = match o.kind() {
            OutputKind::Mp4 => path.to_path_buf(),
            OutputKind::Hls => path.join("index.m3u8"),
        };
        fs::read_to_string(file).ok()
    }

    fn assert_no_names_left(outputs: &[PendingOutput], work: &Path) {
        for o in outputs {
            for name in [o.temp(), o.aside()] {
                assert!(!name.exists(), "{}", name.display());
            }
        }
        let record = workdir::read_outputs(work).unwrap();
        assert!(record.outputs.is_empty() && !record.swapped, "{record:?}");
    }

    #[test]
    fn replaced_outputs_are_swapped_and_old_ones_removed() {
        let dir = scratch("swap");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, o.target(), "旧");
            put(o, o.temp(), "新");
        }

        let commit = Commit::begin(&work, &outputs).unwrap();
        assert_eq!(workdir::read_outputs(&work).unwrap().outputs, outputs);
        assert_eq!(
            commit.swap(&[Occupant::Output, Occupant::Output]).unwrap(),
            []
        );

        for o in &outputs {
            assert_eq!(read(o, o.target()).as_deref(), Some("新"));
        }
        assert_no_names_left(&outputs, &work);
    }

    /// 都装上后、删旧输出之前，记录已记为已换好：此后中断，旧输出只删不放回。
    #[test]
    fn the_record_says_swapped_before_old_outputs_are_removed() {
        let dir = scratch("marked");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, o.target(), "旧");
            put(o, o.temp(), "新");
        }

        let commit = Commit::begin(&work, &outputs).unwrap();
        assert!(!workdir::read_outputs(&work).unwrap().swapped);
        let (mut moved, mut installed) = (Vec::new(), Vec::new());
        commit
            .move_and_install(
                &[Occupant::Output, Occupant::Output],
                &mut moved,
                &mut installed,
            )
            .unwrap();

        assert!(workdir::read_outputs(&work).unwrap().swapped);
        for o in &outputs {
            assert_eq!(read(o, o.aside()).as_deref(), Some("旧"));
        }
    }

    /// 换上 HLS 失败（这里是准备目录不见了）：已装上的新 MP4 撤下，挪开的旧 MP4 与旧 HLS 都放回。
    #[test]
    fn a_failed_swap_restores_the_old_outputs() {
        let dir = scratch("undo");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        for o in &outputs {
            put(o, o.target(), "旧");
        }
        put(&outputs[0], outputs[0].temp(), "新");

        let commit = Commit::begin(&work, &outputs).unwrap();
        let failure = commit
            .swap(&[Occupant::Output, Occupant::Output])
            .unwrap_err();

        assert!(matches!(failure, Error::Io { .. }), "{failure}");
        for o in &outputs {
            assert_eq!(read(o, o.target()).as_deref(), Some("旧"));
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
            put(o, o.temp(), "新");
            put(o, o.target(), "别人的");
        }

        let commit = Commit::begin(&work, &outputs).unwrap();
        match commit.swap(&[Occupant::Nothing, Occupant::Nothing]) {
            Err(Error::OutputExists(path)) => assert_eq!(path, outputs[0].target()),
            other => panic!("应报已存在：{other:?}"),
        }
        for o in &outputs {
            assert_eq!(read(o, o.target()).as_deref(), Some("别人的"));
        }
        assert_no_names_left(&outputs, &work);
    }

    /// 检查时是空目录、挪开之前别的任务装上了它的 HLS：挪开后发现不为空，放回去，报已存在。
    #[test]
    fn an_empty_directory_filled_after_the_check_is_put_back() {
        let dir = scratch("filled");
        let work = dir.join("work");
        let [_, hls] = outputs(&dir);
        put(&hls, hls.temp(), "新");
        put(&hls, hls.target(), "别人的");

        let outputs = [hls];
        let commit = Commit::begin(&work, &outputs).unwrap();
        assert!(matches!(
            commit.swap(&[Occupant::EmptyDir]),
            Err(Error::OutputExists(_))
        ));
        assert_eq!(
            read(&outputs[0], outputs[0].target()).as_deref(),
            Some("别人的")
        );
        assert_no_names_left(&outputs, &work);
    }

    /// 撤回时原处已被别的任务占了（这里是 MP4：装上时发现别的任务刚装上）：不替换它，旧输出留在挪开的名字下、
    /// 记在残留与记录里，其余照常撤回；下次收拾时原处是输出，删掉旧的。
    #[test]
    fn a_roll_back_does_not_replace_and_keeps_what_it_could_not_put_back() {
        let dir = scratch("displaced");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        for o in &outputs {
            put(o, o.aside(), "旧");
            put(o, o.temp(), "新");
        }
        put(mp4, mp4.target(), "别人的");

        let commit = Commit::begin(&work, &outputs).unwrap();
        let failure = Error::OutputExists(mp4.target().to_path_buf());
        let Error::Cleanup { leftovers, .. } = commit.undo(failure, &[], &[mp4, hls]) else {
            panic!("应报没放回的旧输出");
        };

        let target = mp4.target().to_path_buf();
        let kinds: Vec<_> = leftovers.iter().map(|l| (&*l.path, &l.kind)).collect();
        assert_eq!(kinds, [(mp4.aside(), &LeftoverKind::Displaced { target })]);
        assert_eq!(read(mp4, mp4.target()).as_deref(), Some("别人的"));
        assert_eq!(read(hls, hls.target()).as_deref(), Some("旧"));
        assert_eq!(workdir::read_outputs(&work).unwrap().outputs, outputs);

        assert_eq!(recover(&work, &[]).unwrap(), []);
        assert_eq!(read(mp4, mp4.target()).as_deref(), Some("别人的"));
        assert_no_names_left(&outputs, &work);
    }

    /// 检查 `target` 时应报建不出来，挡住它的是上级 `ancestor`。
    fn assert_blocked(target: &Path, ancestor: &Path) {
        for kind in [OutputKind::Mp4, OutputKind::Hls] {
            match check(&pending(kind, target.to_path_buf()), true) {
                Err(Error::OutputOccupied {
                    path,
                    ancestor: Some(found),
                }) => assert_eq!((&*path, &*found), (target, ancestor)),
                other => panic!("应报建不出来：{other:?}"),
            }
        }
    }

    /// 输出路径最近的已存在的上级是文件：建不出来，报路径被占用，带上挡住它的上级；上级只是缺目录的建得出来。
    #[test]
    fn an_output_under_a_file_is_occupied() {
        let dir = scratch("under_file");
        let file = dir.join("file");
        fs::write(&file, "别人的文件").unwrap();

        assert_blocked(&dir.join("file/out"), &file);
        assert_blocked(&dir.join("file/sub/out"), &file);
        for kind in [OutputKind::Mp4, OutputKind::Hls] {
            let creatable = pending(kind, dir.join("new/sub/out"));
            assert_eq!(check(&creatable, false).unwrap(), Occupant::Nothing);
        }
    }

    /// 上级是悬空的符号链接（如没挂载的外置盘）：跟随后不存在，但它本身在，同样建不出来。
    #[cfg(unix)]
    #[test]
    fn an_output_under_a_dangling_symlink_is_occupied() {
        let dir = scratch("under_dangling_link");
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("gone"), &link).unwrap();

        assert_blocked(&dir.join("link/sub/out"), &link);
    }

    /// D 到 Z 里没用的一个盘符的根。
    #[cfg(windows)]
    fn missing_drive() -> PathBuf {
        ('D'..='Z')
            .rev()
            .map(|letter| PathBuf::from(format!("{letter}:\\")))
            .find(|drive| !drive.exists())
            .expect("D 到 Z 里至少有一个没用的盘符")
    }

    /// 应报读不到盘符的根 `drive`。
    #[cfg(windows)]
    fn assert_unavailable<T: std::fmt::Debug>(result: Result<T, Error>, drive: &Path) {
        match result {
            Err(Error::VolumeUnavailable { root, .. }) => assert_eq!(root, drive),
            other => panic!("应报读不到 {}：{other:?}", drive.display()),
        }
    }

    /// 输出在不存在的盘符上（直接在根下，或在根下的目录里）：报读不到盘，不判为建得出来。
    #[cfg(windows)]
    #[test]
    fn an_output_on_a_missing_drive_is_reported() {
        let drive = missing_drive();
        for target in [drive.join("out.mp4"), drive.join("movies").join("out.mp4")] {
            assert_unavailable(check(&pending(OutputKind::Mp4, target), false), &drive);
        }
    }

    /// 上次换上没做完、输出所在的盘这次不在：保留名看不到不算没有，报读不到盘，记录留着，盘回来后接着收拾。
    #[cfg(windows)]
    #[test]
    fn leftovers_on_a_missing_drive_keep_their_record() {
        let dir = scratch("missing_drive");
        let work = dir.join("work");
        let drive = missing_drive();
        let outputs = [pending(OutputKind::Mp4, drive.join("movies").join("a.mp4"))];
        workdir::record_outputs(&work, &outputs, false).unwrap();

        assert_unavailable(recover(&work, &[]), &drive);
        assert_eq!(workdir::read_outputs(&work).unwrap().outputs, outputs);
    }

    /// 换上没做完时中断：临时输出删掉；挪开的旧输出原处空着就放回，原处是新输出就删掉。
    #[test]
    fn interrupted_swaps_are_recovered() {
        let dir = scratch("recover");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        for o in &outputs {
            put(o, o.temp(), "半个");
            put(o, o.aside(), "旧");
        }
        put(hls, hls.target(), "新");
        workdir::record_outputs(&work, &outputs, false).unwrap();

        assert_eq!(recover(&work, &outputs).unwrap(), []);

        assert_eq!(read(mp4, mp4.target()).as_deref(), Some("旧"));
        assert_eq!(read(hls, hls.target()).as_deref(), Some("新"));
        assert_no_names_left(&outputs, &work);
    }

    /// 已换好（记录记为已换好，新输出随后被用户挪走）：挪开的旧输出只删不放回。没有记录的（放弃任务时没能放回、已报给
    /// 用户的）不动，报出来；没有记录的临时输出照样删掉。
    #[test]
    fn swapped_old_outputs_are_removed_and_unrecorded_ones_left_alone() {
        let dir = scratch("swapped");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        for o in &outputs {
            put(o, o.aside(), "旧");
            put(o, o.temp(), "半个");
        }
        workdir::record_outputs(&work, std::slice::from_ref(mp4), true).unwrap();

        let kept = recover(&work, &outputs).unwrap();

        let target = hls.target().to_path_buf();
        let kinds: Vec<_> = kept.iter().map(|l| (&*l.path, &l.kind)).collect();
        assert_eq!(kinds, [(hls.aside(), &LeftoverKind::Displaced { target })]);
        assert!(!mp4.aside().exists() && !mp4.target().exists() && !hls.target().exists());
        assert_eq!(read(hls, hls.aside()).as_deref(), Some("旧"));
        assert!(!mp4.temp().exists() && !hls.temp().exists());
        let record = workdir::read_outputs(&work).unwrap();
        assert!(record.outputs.is_empty() && !record.swapped, "{record:?}");
    }

    /// 换上没做完、原处是空目录（系统自动生成的元数据文件不算）：放回旧输出。原处是别的东西（MP4 路径上的目录、
    /// 有别的文件的 HLS 目录）：留着旧输出与记录，报出来。
    #[test]
    fn old_outputs_go_back_into_empty_directories_but_not_onto_other_things() {
        let dir = scratch("occupied");
        let work = dir.join("work");
        let [mp4, hls] = outputs(&dir);
        // 另一个任务目录指纹相同的 HLS 输出放在别的目录里，保留名才不重
        fs::create_dir(dir.join("sub")).unwrap();
        let other = pending(OutputKind::Hls, dir.join("sub/b"));
        for o in [&mp4, &hls, &other] {
            put(o, o.aside(), "旧");
            fs::create_dir(o.target()).unwrap();
        }
        fs::write(hls.target().join(".DS_Store"), "访达生成的").unwrap();
        fs::write(other.target().join("notes.txt"), "别人的文件").unwrap();
        let outputs = [mp4, hls, other];
        workdir::record_outputs(&work, &outputs, false).unwrap();

        let kept = recover(&work, &[]).unwrap();

        let [mp4, hls, other] = &outputs;
        assert_eq!(kept, [unsettled(mp4), unsettled(other)]);
        assert_eq!(read(hls, hls.target()).as_deref(), Some("旧"));
        assert!(mp4.aside().exists() && other.aside().exists());
        assert!(other.target().join("notes.txt").exists());
        assert_eq!(workdir::read_outputs(&work).unwrap().outputs, outputs);
    }
}
