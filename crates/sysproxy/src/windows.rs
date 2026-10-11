//! Windows：当前用户的 Internet 选项（注册表）。

use windows_registry::CURRENT_USER;

use crate::{Error, Windows};

const SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// 键或值不存在：HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
const NOT_FOUND: i32 = 0x8007_0002_u32.cast_signed();

/// 读代理设置；没有 Internet Settings 这个键时为 None。
pub(crate) fn read() -> Result<Option<Windows>, Error> {
    let key = match CURRENT_USER.open(SETTINGS) {
        Ok(key) => key,
        Err(e) if e.code().0 == NOT_FOUND => return Ok(None),
        Err(e) => return Err(Error(format!("打开注册表 HKCU\\{SETTINGS} 失败：{e}"))),
    };
    Ok(Some(Windows {
        enabled: optional("ProxyEnable", |name| key.get_u32(name))?.is_some_and(|value| value != 0),
        server: optional("ProxyServer", |name| key.get_string(name))?,
        bypass: optional("ProxyOverride", |name| key.get_string(name))?,
        auto_config_url: optional("AutoConfigURL", |name| key.get_string(name))?,
    }))
}

/// 用 `get` 读值 `name`；没有这个值时为 None，类型不对等其余错误报出。
fn optional<T>(
    name: &str,
    get: impl FnOnce(&str) -> windows_registry::Result<T>,
) -> Result<Option<T>, Error> {
    match get(name) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.code().0 == NOT_FOUND => Ok(None),
        Err(e) => Err(Error(format!(
            "读注册表 HKCU\\{SETTINGS} 的 {name} 失败：{e}"
        ))),
    }
}
