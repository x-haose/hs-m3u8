//! 任务目录中的文件名：分片与 init 段的名字记下合并与续录要用的信息，只认规范写法。

use crate::ident::Fingerprint;

/// 分片文件名记录的信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentName {
    pub session: u32,
    pub sequence: u64,
    /// 会话内的不连续段编号
    pub discontinuity: u64,
    /// 所用 init 段的内容指纹
    pub init: Option<Fingerprint>,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
    /// 见 [`Fingerprint::of_segment`]
    pub id: Fingerprint,
}

/// `<会话>-<序号>-<不连续段>-<init 指纹|none>-<时长>-<身份>.seg`。
pub(super) fn segment_file_name(name: &SegmentName) -> String {
    let init = name
        .init
        .map_or_else(|| "none".to_owned(), |f| f.to_string());
    format!(
        "{}-{}-{}-{init}-{}-{}.seg",
        name.session, name.sequence, name.discontinuity, name.duration_us, name.id
    )
}

/// [`segment_file_name`] 的逆；只认它写出的规范写法。
pub(super) fn parse_segment_name(file_name: &str) -> Option<SegmentName> {
    let stem = file_name.strip_suffix(".seg")?;
    let mut parts = stem.split('-');
    let name = SegmentName {
        session: parts.next()?.parse().ok()?,
        sequence: parts.next()?.parse().ok()?,
        discontinuity: parts.next()?.parse().ok()?,
        init: match parts.next()? {
            "none" => None,
            text => Some(Fingerprint::parse(text)?),
        },
        duration_us: parts.next()?.parse().ok()?,
        id: Fingerprint::parse(parts.next()?)?,
    };
    (parts.next().is_none() && segment_file_name(&name) == file_name).then_some(name)
}

/// `init-<指纹>.mp4`。
pub(super) fn init_file_name(fingerprint: Fingerprint) -> String {
    format!("init-{fingerprint}.mp4")
}

/// [`init_file_name`] 的逆。
pub(super) fn parse_init_name(file_name: &str) -> Option<Fingerprint> {
    Fingerprint::parse(file_name.strip_prefix("init-")?.strip_suffix(".mp4")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(text: &str) -> Fingerprint {
        Fingerprint::parse(text).unwrap()
    }

    #[test]
    fn segment_names_round_trip_and_reject_non_canonical_forms() {
        let name = SegmentName {
            session: 2,
            sequence: u64::MAX,
            discontinuity: 7,
            init: Some(fingerprint("00000000000000ff")),
            duration_us: 6_006_000,
            id: fingerprint("0123456789abcdef"),
        };
        assert_eq!(parse_segment_name(&segment_file_name(&name)), Some(name));
        let plain = SegmentName { init: None, ..name };
        assert_eq!(parse_segment_name(&segment_file_name(&plain)), Some(plain));

        let id = "0123456789abcdef";
        for other in [
            format!("0-1-0-none-1000-{id}.seg.part"),
            format!("0-01-0-none-1000-{id}.seg"),
            format!("0-+1-0-none-1000-{id}.seg"),
            "0-1-0-none-1000.seg".to_owned(),
            format!("0-1-0-none-1000-{id}-9.seg"),
            format!("0-1-0-3-1000-{id}.seg"),
            "init-0123456789abcdef.mp4".to_owned(),
        ] {
            assert_eq!(parse_segment_name(&other), None, "{other}");
        }
        assert_eq!(
            parse_init_name("init-0123456789abcdef.mp4"),
            Some(fingerprint(id))
        );
        for other in ["init-3.mp4", "init-0123456789abcdef.mp4.part"] {
            assert_eq!(parse_init_name(other), None, "{other}");
        }
    }
}
