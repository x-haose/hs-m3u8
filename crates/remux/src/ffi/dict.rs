//! 选项与元数据字典（AVDictionary）。

use std::ffi::{CStr, CString};

use hs_m3u8_ffmpeg_sys as sys;

use super::out_of_memory;

/// 一个字典，独占，析构时释放。空字典不分配（指针为空）。
pub(crate) struct Dictionary(*mut sys::AVDictionary);

impl Dictionary {
    pub(crate) fn new() -> Dictionary {
        Dictionary(std::ptr::null_mut())
    }

    /// 设 `key` 为 `value`（不含 NUL），已有的覆盖。
    pub(crate) fn set(&mut self, key: &str, value: &str) {
        let key = CString::new(key).expect("键不含 NUL");
        let value = CString::new(value).expect("值不含 NUL");
        // SAFETY: 字典为空或归 self 所有；键与值为 NUL 结尾的字符串，av_dict_set 复制它们。键非空时只会因分配失败而
        // 失败。
        if unsafe { sys::av_dict_set(&raw mut self.0, key.as_ptr(), value.as_ptr(), 0) } < 0 {
            out_of_memory(key.as_bytes().len() + value.as_bytes().len());
        }
    }

    /// 全部条目，按加入的顺序。
    pub(crate) fn entries(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        let mut entry = std::ptr::null();
        loop {
            // SAFETY: 字典为空或归 self 所有；空键加 AV_DICT_IGNORE_SUFFIX 匹配每个条目，传入上一个返回的条目取下一个；
            // 条目在字典不变期间有效，键与值为 NUL 结尾的字符串，此处复制出来。
            unsafe {
                entry = sys::av_dict_get(self.0, c"".as_ptr(), entry, sys::AV_DICT_IGNORE_SUFFIX);
                let Some(found) = entry.as_ref() else {
                    return entries;
                };
                entries.push((
                    CStr::from_ptr(found.key).to_string_lossy().into_owned(),
                    CStr::from_ptr(found.value).to_string_lossy().into_owned(),
                ));
            }
        }
    }

    /// 交给 FFmpeg 读写的位置：FFmpeg 可能换掉字典（如把没用掉的选项留下），新的仍归 self 所有。
    pub(super) fn as_mut_ptr(&mut self) -> *mut *mut sys::AVDictionary {
        &raw mut self.0
    }

    /// 交出字典的所有权；调用方负责释放（或交给接管它的 FFmpeg 对象）。
    pub(super) fn into_raw(self) -> *mut sys::AVDictionary {
        std::mem::ManuallyDrop::new(self).0
    }
}

impl Drop for Dictionary {
    fn drop(&mut self) {
        // SAFETY: 字典为空或归本结构所有；av_dict_free 释放并置空。
        unsafe { sys::av_dict_free(&raw mut self.0) };
    }
}
