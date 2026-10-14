//! 路径所在的盘：访问路径得到「不存在」时，分清是没有还是看不到。

use std::fs;
use std::path::Path;

use crate::Error;

/// 绝对路径 `path` 的根读得到。访问 `path` 得到「不存在」后，据此当作没有（删东西、丢记录、判为建得出来）之前先查
/// 这里：根读不到（Windows 上不存在的盘符、连不上的共享）是看不到，报 [`Error::VolumeUnavailable`]。macOS 上卷卸下后
/// 挂载点消失而根 `/` 还在，与目录被删分不出来，按没有处理。
pub(crate) fn reachable(path: &Path) -> Result<(), Error> {
    let root = path.ancestors().last().expect("ancestors 至少给出路径本身");
    fs::metadata(root)
        .map(drop)
        .map_err(|cause| Error::VolumeUnavailable {
            root: root.to_path_buf(),
            cause,
        })
}

/// 网络共享只有 Windows 上有这种写法。
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 连得上的共享（本机的管理共享）的根读得到，不误报看不到。要求本机开着管理共享 C$ 且当前用户能访问（管理员）。
    #[test]
    fn a_reachable_share_is_reachable() {
        reachable(Path::new(r"\\localhost\C$\hsdl-absent")).unwrap();
    }
}
