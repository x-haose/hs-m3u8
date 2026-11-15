//! `ranges.json`：文件任务已落盘的范围，及其序列化格式（契约）。数据文件先落盘、再写它，它记的范围在数据文件里都是
//! 完整的。

use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::record::{FILE_JOB_FORMAT_VERSION, JOB_FORMAT_VERSION, check_version};
use crate::WorkDirProblem;

pub(super) const RANGES_FILE: &str = "ranges.json";

/// `ranges.json` 的写法：`ranges` 为 [起点, 终点)，升序、不重叠、不相接、不为空。格式版本同 job.json。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangesFile {
    format_version: u32,
    ranges: Vec<[u64; 2]>,
}

/// `ranges` 须升序、不重叠、不为空（[`crate::file::RangeSet`] 给出的）。
pub(super) fn encode(ranges: &[Range<u64>]) -> Vec<u8> {
    let file = RangesFile {
        format_version: JOB_FORMAT_VERSION,
        ranges: ranges.iter().map(|r| [r.start, r.end]).collect(),
    };
    serde_json::to_vec(&file).expect("RangesFile 只含整数与数组，序列化不会失败")
}

/// 解析 `ranges.json`；各范围须升序、不重叠、不相接、不为空，且不超过总长度 `length`。
pub(super) fn decode(bytes: &[u8], length: u64) -> Result<Vec<Range<u64>>, WorkDirProblem> {
    check_version(
        bytes,
        RANGES_FILE,
        FILE_JOB_FORMAT_VERSION,
        JOB_FORMAT_VERSION,
    )?;
    let file: RangesFile = serde_json::from_slice(bytes)
        .map_err(|e| WorkDirProblem::Corrupt(format!("{RANGES_FILE} 无法解析：{e}")))?;
    let mut previous_end = None;
    let mut ranges = Vec::with_capacity(file.ranges.len());
    for [start, end] in file.ranges {
        let ordered = previous_end.is_none_or(|p| p < start);
        if !(ordered && start < end && end <= length) {
            return Err(WorkDirProblem::Corrupt(format!(
                "{RANGES_FILE} 里的范围 [{start}, {end}) 不合法（总长 {length}）"
            )));
        }
        previous_end = Some(end);
        ranges.push(start..end);
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_round_trip_and_reject_disorder() {
        let ranges = vec![0..10, 20..30];
        let bytes = encode(&ranges);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"format_version":14,"ranges":[[0,10],[20,30]]}"#
        );
        assert_eq!(decode(&bytes, 30), Ok(ranges));
        assert_eq!(
            decode(br#"{"format_version":14,"ranges":[]}"#, 0),
            Ok(vec![])
        );
        for rejected in [
            r#"{"format_version":14,"ranges":[[0,10],[10,20]]}"#,
            r#"{"format_version":14,"ranges":[[20,30],[0,10]]}"#,
            r#"{"format_version":14,"ranges":[[5,5]]}"#,
            r#"{"format_version":14,"ranges":[[0,31]]}"#,
            r#"{"format_version":13,"ranges":[]}"#,
            r#"{"format_version":14,"ranges":[],"extra":1}"#,
        ] {
            assert!(decode(rejected.as_bytes(), 30).is_err(), "{rejected}");
        }
    }
}
