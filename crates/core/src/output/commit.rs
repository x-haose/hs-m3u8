//! 换上输出：把备齐的临时输出改名为输出。
//!
//! 输出已存在（要求覆盖、检查过可以替换）时先把旧输出挪开，装上新的，成功后再删旧的；任一步失败都撤回到换之前：
//! 装上的新输出删掉、挪开的旧输出放回，撤回也失败时一并报告。装上时不替换已有的：几个任务同时提交，先装上的成功，
//! 后到的报已存在。
//!
//! 简化：进程在换的中途（几次改名之间）被杀时不续完，下次运行按任务目录里的记录（见
//! [`crate::workdir::PendingOutput`]）清掉临时输出、把挪开了而原处空着的旧输出放回；MP4 与 HLS 可能一新一旧，
//! 重新运行时要求覆盖即可。换的过程变长（如跨文件系统改名）时，把每一步记进任务目录、下次运行续完。

use std::fs;
use std::io;
use std::path::Path;

use super::io_error;
use crate::Error;
use crate::workdir::PendingOutput;

/// 换上备齐的输出（调用方已检查过各输出能否写）。成功时返回删不掉的旧输出的说明：输出已换好，只是旧的还在。
pub(super) fn swap_in(outputs: &[PendingOutput]) -> Result<Vec<String>, Error> {
    let mut moved = Vec::new();
    let mut installed = Vec::new();
    if let Err(failure) = move_and_install(outputs, &mut moved, &mut installed) {
        return Err(roll_back(failure, &installed, &moved));
    }
    Ok(moved
        .iter()
        .filter_map(|o| {
            remove(&o.aside, o.dir)
                .err()
                .map(|e| format!("删除被替换的旧输出 {} 失败：{e}", o.aside.display()))
        })
        .collect())
}

fn move_and_install<'a>(
    outputs: &'a [PendingOutput],
    moved: &mut Vec<&'a PendingOutput>,
    installed: &mut Vec<&'a PendingOutput>,
) -> Result<(), Error> {
    for o in outputs {
        if exists(&o.target)? {
            fs::rename(&o.target, &o.aside).map_err(io_error("挪开", &o.target))?;
            moved.push(o);
        }
    }
    for o in outputs {
        install(o)?;
        installed.push(o);
    }
    Ok(())
}

/// 把 `temp` 装到 `target`，不替换已有的。
fn install(o: &PendingOutput) -> Result<(), Error> {
    let taken = || Error::OutputExists(o.target.clone());
    if o.dir {
        // 目标是非空目录时改名失败；是空目录时被替换，没有可丢的
        return fs::rename(&o.temp, &o.target).map_err(|e| match exists(&o.target) {
            Ok(true) => taken(),
            _ => io_error("重命名", &o.temp)(e),
        });
    }
    // 硬链接在目标已存在时失败，用它做不替换的改名。文件系统不支持硬链接时只能直接改名：只在几个任务同时提交的
    // 那一瞬会替换掉对方刚装上的输出
    match fs::hard_link(&o.temp, &o.target) {
        Ok(()) => fs::remove_file(&o.temp).map_err(io_error("删除", &o.temp)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(taken()),
        Err(_) => fs::rename(&o.temp, &o.target).map_err(io_error("重命名", &o.temp)),
    }
}

/// 撤回到换之前：装上的新输出删掉，挪开的旧输出放回。撤回也失败时把两者一并返回。
fn roll_back(failure: Error, installed: &[&PendingOutput], moved: &[&PendingOutput]) -> Error {
    let undone = installed
        .iter()
        .rev()
        .try_for_each(|o| remove(&o.target, o.dir).map_err(|e| (o.target.clone(), e)))
        .and_then(|()| {
            moved
                .iter()
                .rev()
                .try_for_each(|o| fs::rename(&o.aside, &o.target).map_err(|e| (o.aside.clone(), e)))
        });
    match undone {
        Ok(()) => failure,
        Err((path, cause)) => Error::Cleanup {
            failure: Box::new(failure),
            path,
            cause,
        },
    }
}

/// 上次中断留下的：临时输出删掉；挪开了的旧输出，原处空着就放回，原处已有（新的已装上）就删掉。
pub(super) fn recover(outputs: &[PendingOutput]) -> Result<(), Error> {
    for o in outputs {
        remove(&o.temp, o.dir).map_err(io_error("删除", &o.temp))?;
        if !exists(&o.aside)? {
            continue;
        }
        if exists(&o.target)? {
            remove(&o.aside, o.dir).map_err(io_error("删除", &o.aside))?;
        } else {
            fs::rename(&o.aside, &o.target).map_err(io_error("放回", &o.aside))?;
        }
    }
    Ok(())
}

/// 删掉文件或目录；已不存在的不算失败。
pub(super) fn remove(path: &Path, dir: bool) -> io::Result<()> {
    let removed = if dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// 不跟随符号链接。
fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(cause) => Err(io_error("检查", path)(cause)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// 每个测试独立的空目录。
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hs-m3u8-commit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn output(dir: &Path, name: &str, is_dir: bool) -> PendingOutput {
        PendingOutput {
            target: dir.join(name),
            temp: dir.join(format!("{name}.part")),
            aside: dir.join(format!("{name}.old")),
            dir: is_dir,
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn replaced_outputs_are_swapped_and_old_ones_removed() {
        let dir = scratch("swap");
        let (mp4, hls) = (output(&dir, "a.mp4", false), output(&dir, "a", true));
        fs::write(&mp4.target, "旧").unwrap();
        fs::write(&mp4.temp, "新").unwrap();
        fs::create_dir(&hls.target).unwrap();
        fs::write(hls.target.join("index.m3u8"), "旧").unwrap();
        fs::create_dir(&hls.temp).unwrap();
        fs::write(hls.temp.join("index.m3u8"), "新").unwrap();

        assert_eq!(
            swap_in(&[mp4.clone(), hls.clone()]).unwrap(),
            Vec::<String>::new()
        );

        assert_eq!(read(&mp4.target), "新");
        assert_eq!(read(&hls.target.join("index.m3u8")), "新");
        for leftover in [&mp4.temp, &mp4.aside, &hls.temp, &hls.aside] {
            assert!(!leftover.exists(), "{}", leftover.display());
        }
    }

    /// 换上 HLS 失败（这里是准备目录不见了）：已换上的新 MP4 删掉，挪开的旧 MP4 与旧 HLS 都放回。
    #[test]
    fn a_failed_swap_restores_the_old_outputs() {
        let dir = scratch("roll_back");
        let (mp4, hls) = (output(&dir, "a.mp4", false), output(&dir, "a", true));
        fs::write(&mp4.target, "旧").unwrap();
        fs::write(&mp4.temp, "新").unwrap();
        fs::create_dir(&hls.target).unwrap();
        fs::write(hls.target.join("index.m3u8"), "旧").unwrap();

        assert!(swap_in(&[mp4.clone(), hls.clone()]).is_err());

        assert_eq!(read(&mp4.target), "旧");
        assert_eq!(read(&hls.target.join("index.m3u8")), "旧");
        assert!(!mp4.aside.exists() && !hls.aside.exists());
    }

    /// 装上之前别人刚写好了同一个输出（几个任务同时提交）：不替换，报已存在。
    #[test]
    fn an_output_taken_meanwhile_is_not_replaced() {
        let dir = scratch("taken");
        let (mp4, hls) = (output(&dir, "a.mp4", false), output(&dir, "a", true));
        fs::write(&mp4.temp, "新").unwrap();
        fs::write(&mp4.target, "别人的").unwrap();
        fs::create_dir(&hls.temp).unwrap();
        fs::create_dir(&hls.target).unwrap();
        fs::write(hls.target.join("index.m3u8"), "别人的").unwrap();

        for o in [&mp4, &hls] {
            match install(o) {
                Err(Error::OutputExists(path)) => assert_eq!(path, o.target),
                other => panic!("应报已存在：{other:?}"),
            }
        }
        assert_eq!(read(&mp4.target), "别人的");
        assert_eq!(read(&hls.target.join("index.m3u8")), "别人的");
    }

    /// 上次中断留下的：临时输出删掉；挪开的旧输出原处空着就放回，原处已有新的就删掉。
    #[test]
    fn interrupted_swaps_are_recovered() {
        let dir = scratch("recover");
        let (mp4, hls) = (output(&dir, "a.mp4", false), output(&dir, "a", true));
        fs::write(&mp4.temp, "半个").unwrap();
        fs::write(&mp4.aside, "旧").unwrap();
        fs::create_dir(&hls.temp).unwrap();
        fs::create_dir(&hls.target).unwrap();
        fs::write(hls.target.join("index.m3u8"), "新").unwrap();
        fs::create_dir(&hls.aside).unwrap();

        recover(&[mp4.clone(), hls.clone()]).unwrap();

        assert_eq!(read(&mp4.target), "旧");
        assert_eq!(read(&hls.target.join("index.m3u8")), "新");
        for leftover in [&mp4.temp, &mp4.aside, &hls.temp, &hls.aside] {
            assert!(!leftover.exists(), "{}", leftover.display());
        }
    }
}
