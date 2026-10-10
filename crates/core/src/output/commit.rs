//! 换上输出：把备齐的临时输出装到输出路径，可撤回；中断与失败留下的按任务目录里的记录收拾。
//!
//! 每个输出有两个与它同级的保留名（见 [`PendingOutput`]）：临时输出，与旧输出挪开后的名字。同一任务目录同一时间
//! 只有一个任务（任务目录的锁），这两个名字上的东西只可能是这个任务目录的任务留下的，收拾时不看内容：临时输出删掉；
//! 挪开的旧输出看记录：
//! - 换上还没做完（记录在、没记为已换好）：原处空着或是空目录就放回；原处是输出就删掉（只有要替换时才挪开旧输出，
//!   原处是输出说明更新的输出已经装上，本任务的或同时写同一输出的别的任务的）；原处是别的东西就留着，报出来；
//! - 已换好，或没有记录（换好后删不掉而留下，记录已删）：只删不放回。
//!
//! 写临时输出之前把各输出记进任务目录（[`Commit::begin`]），全部装上后改记为已换好，再删旧输出。之后每次运行在任务
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
use crate::entries;
use crate::error::io_error;
use crate::workdir::{self, OutputKind, PendingOutput};
use crate::{Error, Leftover, LeftoverKind};

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
fn occupant_by_ancestor(target: &Path) -> Result<Occupant, Error> {
    for ancestor in target.ancestors().skip(1) {
        match fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => return Ok(Occupant::Nothing),
            Ok(_) => {}
            // 跟随后不存在：没有这一项就再往上看，有（悬空的符号链接）就建不出来
            Err(e) if is_absent(&e) => {
                if !exists(ancestor).map_err(io_error("检查", ancestor))? {
                    continue;
                }
            }
            Err(cause) => return Err(io_error("检查", ancestor)(cause)),
        }
        let ancestor = ancestor.to_path_buf();
        return Ok(Occupant::Blocked { ancestor });
    }
    Ok(Occupant::Nothing)
}

/// 输出能否写，能写时返回路径上现在的东西：空着、是空目录，或要求覆盖而路径上是输出。是输出而没有要求覆盖时报
/// [`Error::OutputExists`]；是别的东西或建不出来时报 [`Error::OutputOccupied`]，覆盖也不替换，以免路径给错时删掉
/// 别人的文件。
pub(super) fn check(o: &PendingOutput, overwrite: bool) -> Result<Occupant, Error> {
    let occupied = |ancestor| Error::OutputOccupied {
        path: o.target().to_path_buf(),
        ancestor,
    };
    match occupant(o)? {
        Occupant::Other => Err(occupied(None)),
        Occupant::Blocked { ancestor } => Err(occupied(Some(ancestor))),
        Occupant::Output if !overwrite => Err(Error::OutputExists(o.target().to_path_buf())),
        occupant => Ok(occupant),
    }
}

/// 收拾上次留下的：先按记录，再按本次各输出 `current` 的名字（没有记录的），规则见模块说明。返回旧输出因原处是别的
/// 东西而留着的输出，它们留在记录里；都收拾完时删掉记录。失败时记录留着，可以重试。任务目录存在时调用方须已加锁。
pub(super) fn recover(
    work_dir: &Path,
    current: &[PendingOutput],
) -> Result<Vec<PendingOutput>, Error> {
    let record = workdir::read_outputs(work_dir)?;
    let mut unsettled = Vec::new();
    for o in &record.outputs {
        if !settle(o, !record.swapped)? {
            unsettled.push(o.clone());
        }
    }
    let recorded = |o: &PendingOutput| record.outputs.iter().any(|r| r.aside() == o.aside());
    for o in current.iter().filter(|o| !recorded(o)) {
        settle(o, false)?;
    }
    if unsettled.is_empty() {
        workdir::clear_outputs(work_dir)
            .map_err(io_error("删除", &workdir::outputs_file(work_dir)))?;
    }
    Ok(unsettled)
}

/// [`recover`] 留着的旧输出，作为残留报出。
pub(super) fn unsettled(o: &PendingOutput) -> Leftover {
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
    remove(o.temp(), o.kind()).map_err(io_error("删除", o.temp()))?;
    if !exists(o.aside()).map_err(io_error("检查", o.aside()))? {
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
            install(o.aside(), o.target(), o.kind()).map_err(io_error("放回", o.aside()))?;
        }
    }
    remove(o.aside(), o.kind()).map_err(io_error("删除", o.aside()))?;
    Ok(true)
}

/// 删掉只有系统自动生成的元数据文件的目录；其间有了别的内容就失败。
fn remove_empty_dir(dir: &Path) -> Result<(), Error> {
    for entry in entries::list(dir)? {
        if entry.is_system_file() {
            remove(&entry.path, OutputKind::Mp4).map_err(io_error("删除", &entry.path))?;
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
    /// 的东西（被替换的旧输出、临时名、记录）：输出已换好；删不掉的旧输出与临时名，下次写同一输出时在任务开头再删。
    pub(super) fn swap(self, existing: &[Occupant]) -> Result<Vec<Leftover>, Error> {
        let (mut moved, mut installed) = (Vec::new(), Vec::new());
        if let Err(failure) = self.move_and_install(existing, &mut moved, &mut installed) {
            return Err(self.undo(failure, &installed, &moved));
        }
        let mut leftovers = Vec::new();
        for o in moved {
            if let Err(e) = remove(o.aside(), o.kind()) {
                leftovers.push(leftover(o.aside(), LeftoverKind::Removable, &e));
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
            if !install(o.temp(), o.target(), o.kind()).map_err(io_error("装上", o.temp()))? {
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
                leftovers.push(leftover(o.target(), LeftoverKind::Installed, &e));
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
            match install(o.aside(), o.target(), o.kind()) {
                Ok(true) => {
                    // 用硬链接放回时挪开的名字还在
                    if let Err(e) = remove(o.aside(), o.kind()) {
                        leftovers.push(leftover(o.aside(), LeftoverKind::Removable, &e));
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
            if let Err(e) = remove(o.temp(), o.kind()) {
                leftovers.push(leftover(o.temp(), LeftoverKind::Removable, &e));
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

/// 把输出路径上的东西挪到挪开的名字上；已经不在了（检查之后被删掉）时为 false。
fn move_aside(o: &PendingOutput) -> Result<bool, Error> {
    match fs::rename(o.target(), o.aside()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(cause) => Err(io_error("挪开", o.target())(cause)),
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
        Err(e) if is_absent(&e) => Ok(()),
        other => other,
    }
}

/// 不跟随符号链接。
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if is_absent(&e) => Ok(false),
        Err(e) => Err(e),
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

    /// 已换好（记录记为已换好，新输出随后被用户挪走），或没有记录（换好后删不掉而留下）：挪开的旧输出只删不放回。
    #[test]
    fn swapped_or_unrecorded_old_outputs_are_only_removed() {
        let dir = scratch("swapped");
        let work = dir.join("work");
        let outputs = outputs(&dir);
        let [mp4, hls] = &outputs;
        for o in &outputs {
            put(o, o.aside(), "旧");
        }
        workdir::record_outputs(&work, std::slice::from_ref(mp4), true).unwrap();

        assert_eq!(recover(&work, &outputs).unwrap(), []);

        assert!(!mp4.target().exists() && !hls.target().exists());
        assert_no_names_left(&outputs, &work);
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

        let unsettled = recover(&work, &[]).unwrap();

        let [mp4, hls, other] = &outputs;
        assert_eq!(unsettled, [mp4.clone(), other.clone()]);
        assert_eq!(read(hls, hls.target()).as_deref(), Some("旧"));
        assert!(mp4.aside().exists() && other.aside().exists());
        assert!(other.target().join("notes.txt").exists());
        assert_eq!(workdir::read_outputs(&work).unwrap().outputs, outputs);
    }
}
