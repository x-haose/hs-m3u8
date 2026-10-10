//! 本地 HLS 输出：任务目录中已解密的分片原样放进一个可直接播放的目录，不经 FFmpeg。
//!
//! ```text
//! index.m3u8              入口：单轨时为媒体播放列表；视频与独立音频分离时为主播放列表
//! <轨道>/index.m3u8        音视频分离时这条轨的媒体播放列表
//! <轨道>/<n>.<扩展名>       分片，n 为这条轨里的播放顺序（从 0 起）；扩展名见 extension
//! <轨道>/init-<指纹>.mp4    fMP4 的 init 段，文件名同任务目录
//! ```
//!
//! 先在 `<目录>.part` 里备齐，最后改名为目标目录。只放各轨都有的不连续段组（与 MP4 相同），组与组之间加
//! EXT-X-DISCONTINUITY；组内缺失的分片不标出，保留原时间戳，与 MP4 一致：缺失往往只在一条轨上，只给一条轨
//! 加不连续标记会让各轨的不连续段编号对不上（RFC 8216 6.2.4 要求各轨一致）。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use super::options::sibling;
use super::{GroupTrack, io_error};
use crate::ident::Fingerprint;
use crate::selection::SelectionKey;
use crate::verify::{Standalone, standalone_format};
use crate::{Error, WorkDirProblem};

const INDEX: &str = "index.m3u8";

/// fMP4 分片的扩展名。
const FMP4: &str = "m4s";

/// 没有 init 段的分片的扩展名，与 FFmpeg 识别出的格式对应：FFmpeg 读 HLS 时核对分片的扩展名与识别出的格式
/// （`extension_picky`，默认开启），不一致即拒绝。
fn extension(format: Standalone) -> &'static str {
    match format {
        Standalone::Ts => "ts",
        Standalone::Aac => "aac",
        Standalone::Mp3 => "mp3",
        Standalone::Ac3 => "ac3",
        Standalone::Eac3 => "eac3",
    }
}

/// 目标目录能否写：不存在、为空，或 `overwrite` 且其中全是本库写出的文件。
pub(super) fn check(dir: &Path, overwrite: bool) -> Result<(), Error> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(cause) => return Err(io_error("检查", dir)(cause)),
    };
    let replaceable = meta.is_dir() && (is_empty(dir)? || overwrite && written_by_us(dir)?);
    if replaceable {
        Ok(())
    } else {
        Err(Error::OutputExists(dir.to_path_buf()))
    }
}

/// 写出 HLS 目录 `target`；`work_dir` 为分片所在的任务目录，用于报告目录内容无法识别。
pub(super) fn write(
    target: &Path,
    work_dir: &Path,
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    overwrite: bool,
) -> Result<(), Error> {
    let name = target.file_name().expect("校验过：输出路径都有文件名");
    let stage = sibling(target, name, ".part");
    remove_stale_stage(&stage)?;
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error("创建", parent))?;
    }
    fs::create_dir(&stage).map_err(io_error("创建", &stage))?;
    let written = fill(&stage, work_dir, groups, selection).and_then(|()| {
        // 开始时检查过；备齐期间目标可能被别人占用，替换前再查一次
        check(target, overwrite)?;
        match fs::remove_dir_all(target) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_error("删除", target)(e)),
            _ => fs::rename(&stage, target).map_err(io_error("重命名", &stage)),
        }
    });
    written.map_err(|failure| match fs::remove_dir_all(&stage) {
        Ok(()) => failure,
        Err(cause) => Error::Cleanup {
            failure: Box::new(failure),
            path: stage.clone(),
            cause,
        },
    })
}

/// 上次中断留下的 `<目录>.part`：全是本库写出的文件才删，否则报已存在。
fn remove_stale_stage(stage: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(stage) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(io_error("检查", stage)(cause)),
        Ok(meta) if meta.is_dir() && written_by_us(stage)? => {
            fs::remove_dir_all(stage).map_err(io_error("删除", stage))
        }
        Ok(_) => Err(Error::OutputExists(stage.to_path_buf())),
    }
}

/// 在 `stage` 里放好各轨的分片、init 段与播放列表。
fn fill(
    stage: &Path,
    work_dir: &Path,
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
) -> Result<(), Error> {
    let track_count = groups.first().map_or(0, Vec::len);
    let mut playlists = Vec::with_capacity(track_count);
    for track in 0..track_count {
        let dir = stage.join(track.to_string());
        fs::create_dir(&dir).map_err(io_error("创建", &dir))?;
        let tracks = groups.iter().map(|g| &g[track]);
        playlists.push(fill_track(&dir, work_dir, tracks)?);
    }
    match (track_count, selection) {
        (1, _) => write_file(&stage.join(INDEX), &media_playlist(&playlists[0], "0/")),
        (2, Some(selection)) => {
            for (track, groups) in playlists.iter().enumerate() {
                let path = stage.join(track.to_string()).join(INDEX);
                write_file(&path, &media_playlist(groups, ""))?;
            }
            write_file(&stage.join(INDEX), &master_playlist(selection))
        }
        (count, selection) => panic!(
            "轨道只有所选变体与独立音频两种，后者只来自主播放列表：{count} 条，有选轨 {}",
            selection.is_some()
        ),
    }
}

/// 放好一条轨各组的分片与 init 段，返回这条轨各组在播放列表中的内容（地址相对于轨道目录）。
fn fill_track<'a>(
    dir: &Path,
    work_dir: &Path,
    groups: impl Iterator<Item = &'a GroupTrack>,
) -> Result<Vec<PlaylistGroup>, Error> {
    let mut placed_inits = HashSet::new();
    let mut playlist = Vec::new();
    let mut index = 0usize;
    for group in groups {
        let init = match &group.init {
            Some(path) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .expect("init 段文件名由本库按指纹生成")
                    .to_owned();
                if placed_inits.insert(name.clone()) {
                    place(path, &dir.join(&name))?;
                }
                Some(name)
            }
            None => None,
        };
        let first = &group.segments.first().expect("每组每轨至少一个分片").path;
        // 同一不连续段组内文件格式不变（RFC 8216 6.2.1：格式变化处必须有 EXT-X-DISCONTINUITY）
        let ext = match group.init {
            Some(_) => FMP4,
            None => extension(sniff(first, work_dir)?),
        };
        let mut segments = Vec::with_capacity(group.segments.len());
        for segment in &group.segments {
            let name = format!("{index}.{ext}");
            place(&segment.path, &dir.join(&name))?;
            segments.push((name, segment.duration_us));
            index += 1;
        }
        playlist.push(PlaylistGroup { init, segments });
    }
    Ok(playlist)
}

/// 识别时读取分片开头的字节数。
const SNIFF_LEN: u64 = 64 * 1024;

/// 没有 init 段的分片的格式。
///
/// 简化：只看开头 64 KiB；打包音频前的 ID3 标签更长（如带封面图）时识别不出、报目录内容无法识别，
/// 遇到时改为按标签长度跳读。
fn sniff(path: &Path, work_dir: &Path) -> Result<Standalone, Error> {
    let mut head = Vec::new();
    File::open(path)
        .and_then(|file| file.take(SNIFF_LEN).read_to_end(&mut head))
        .map_err(io_error("读取", path))?;
    standalone_format(&head).ok_or_else(|| Error::WorkDir {
        path: work_dir.to_path_buf(),
        problem: WorkDirProblem::Corrupt(format!("分片内容无法识别：{}", path.display())),
    })
}

/// 把 `src` 放到 `dst`：能硬链接就硬链接，不占额外空间；做不了时（跨文件系统、文件系统不支持）复制并落盘，
/// 复制也失败才报错。
fn place(src: &Path, dst: &Path) -> Result<(), Error> {
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map_err(io_error("复制", src))?;
    File::options()
        .write(true)
        .open(dst)
        .and_then(|file| file.sync_all())
        .map_err(io_error("落盘", dst))
}

fn write_file(path: &Path, text: &str) -> Result<(), Error> {
    File::create(path)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .map_err(io_error("写入", path))
}

/// 媒体播放列表中的一个不连续段组：init 段与分片的文件名，以及分片的声明时长（微秒）。
struct PlaylistGroup {
    init: Option<String>,
    segments: Vec<(String, u64)>,
}

/// 点播的媒体播放列表；`prefix` 加在每个文件名前，构成相对于播放列表的地址。
///
/// 版本 6 是非 I 帧播放列表使用 EXT-X-MAP 所要求的（RFC 8216 7）。TARGETDURATION 取各分片时长四舍五入到整数秒
/// 后的最大值（RFC 8216 4.3.3.1）。时长按微秒精确写出。
fn media_playlist(groups: &[PlaylistGroup], prefix: &str) -> String {
    let target = groups
        .iter()
        .flat_map(|g| &g.segments)
        .map(|&(_, us)| us / 1_000_000 + u64::from(us % 1_000_000 >= 500_000))
        .max()
        .unwrap_or(0);
    let mut text = format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-PLAYLIST-TYPE:VOD\n"
    );
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            text.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if let Some(init) = &group.init {
            text.push_str(&format!("#EXT-X-MAP:URI=\"{prefix}{init}\"\n"));
        }
        for (name, us) in &group.segments {
            let (seconds, micros) = (us / 1_000_000, us % 1_000_000);
            text.push_str(&format!("#EXTINF:{seconds}.{micros:06},\n{prefix}{name}\n"));
        }
    }
    text + "#EXT-X-ENDLIST\n"
}

/// 视频与独立音频分离时的主播放列表：第 0 条轨为变体，第 1 条为音频 rendition。变体的带宽、分辨率与编码照抄
/// 来源（来源没写的也不写）；音频组固定为 `audio`。名称与语言照抄来源，带有引号或换行（带引号的字符串里不允许）
/// 时不用：没有可用的名称时名称为 `audio`，语言不写。
fn master_playlist(selection: &SelectionKey) -> String {
    let audio = selection
        .audio
        .as_ref()
        .expect("两条轨时第 1 条是独立的音频 rendition");
    let quotable = |text: &&str| !text.contains(['"', '\r', '\n']);
    let language = audio.language.as_deref().filter(quotable);
    let name = audio
        .name
        .as_deref()
        .filter(quotable)
        .or(language)
        .unwrap_or("audio");
    let mut media = format!("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\"");
    if let Some(language) = language {
        media.push_str(&format!(",LANGUAGE=\"{language}\""));
    }
    media.push_str(",DEFAULT=YES,AUTOSELECT=YES,URI=\"1/index.m3u8\"\n");

    let variant = &selection.variant.attributes;
    let mut attributes = Vec::new();
    if let Some(bandwidth) = variant.bandwidth {
        attributes.push(format!("BANDWIDTH={bandwidth}"));
    }
    if let Some(r) = variant.resolution {
        attributes.push(format!("RESOLUTION={}x{}", r.width, r.height));
    }
    let codecs = variant.codecs.join(",");
    if !codecs.is_empty() && quotable(&codecs.as_str()) {
        attributes.push(format!("CODECS=\"{codecs}\""));
    }
    attributes.push("AUDIO=\"audio\"".to_owned());
    format!(
        "#EXTM3U\n{media}#EXT-X-STREAM-INF:{}\n0/index.m3u8\n",
        attributes.join(",")
    )
}

fn is_empty(dir: &Path) -> Result<bool, Error> {
    Ok(fs::read_dir(dir)
        .map_err(io_error("读取", dir))?
        .next()
        .is_none())
}

/// 目录里是否全是本库写出的 HLS 文件（空目录也算），即删掉它不会丢别人的文件。不跟随符号链接：
/// 符号链接不是本库写出的。
fn written_by_us(dir: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        let kind = entry.file_type().map_err(io_error("读取", &entry.path()))?;
        let name = entry.file_name();
        let ours = match name.to_str() {
            Some(INDEX) => kind.is_file(),
            Some(name) if is_number(name) && kind.is_dir() => track_written_by_us(&entry.path())?,
            _ => false,
        };
        if !ours {
            return Ok(false);
        }
    }
    Ok(true)
}

fn track_written_by_us(dir: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        let kind = entry.file_type().map_err(io_error("读取", &entry.path()))?;
        let name = entry.file_name();
        if !kind.is_file() || !name.to_str().is_some_and(is_track_file_name) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 轨道目录里本库写出的文件名：`index.m3u8`、`init-<指纹>.mp4`、`<n>.<扩展名>`。
fn is_track_file_name(name: &str) -> bool {
    if name == INDEX {
        return true;
    }
    if let Some(fingerprint) = name
        .strip_prefix("init-")
        .and_then(|n| n.strip_suffix(".mp4"))
    {
        return Fingerprint::parse(fingerprint).is_some();
    }
    name.split_once('.').is_some_and(|(n, ext)| {
        is_number(n) && (ext == FMP4 || Standalone::ALL.into_iter().any(|f| extension(f) == ext))
    })
}

/// 规范写法的十进制非负整数：没有多余的前导零。
fn is_number(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
}

#[cfg(test)]
mod tests {
    use hs_m3u8_hls::{Playlist, Resolution, Url, parse};

    use super::*;
    use crate::selection::{RenditionKey, VariantAttributes, VariantKey};

    fn url() -> Url {
        Url::parse("file:///out/index.m3u8").unwrap()
    }

    #[test]
    fn media_playlist_round_trips_through_the_parser() {
        let groups = [
            PlaylistGroup {
                init: Some("init-0123456789abcdef.mp4".into()),
                segments: vec![("0.m4s".into(), 6_006_000), ("1.m4s".into(), 2_499_999)],
            },
            PlaylistGroup {
                init: Some("init-00000000000000ff.mp4".into()),
                segments: vec![("2.m4s".into(), 6_500_000)],
            },
        ];
        let text = media_playlist(&groups, "0/");
        let Ok(Playlist::Media(media)) = parse(&text, &url()) else {
            panic!("应为媒体播放列表：{text}");
        };
        // 6.5 秒四舍五入为 7
        assert_eq!(media.target_duration_us, Some(7_000_000));
        assert!(media.ended);
        let seen: Vec<(&str, u64, u64, &str)> = media
            .segments
            .iter()
            .map(|s| {
                let init = s.init.as_ref().unwrap().uri.path();
                (s.uri.path(), s.duration_us, s.discontinuity, init)
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    "/out/0/0.m4s",
                    6_006_000,
                    0,
                    "/out/0/init-0123456789abcdef.mp4"
                ),
                (
                    "/out/0/1.m4s",
                    2_499_999,
                    0,
                    "/out/0/init-0123456789abcdef.mp4"
                ),
                (
                    "/out/0/2.m4s",
                    6_500_000,
                    1,
                    "/out/0/init-00000000000000ff.mp4"
                ),
            ]
        );
    }

    fn selection(name: Option<&str>, language: Option<&str>, codecs: &[&str]) -> SelectionKey {
        SelectionKey {
            variant: VariantKey {
                attributes: VariantAttributes {
                    bandwidth: Some(2_000_000),
                    resolution: Some(Resolution {
                        width: 1280,
                        height: 720,
                    }),
                    codecs: codecs.iter().map(|c| (*c).to_owned()).collect(),
                    audio_group: Some("source-group".into()),
                },
                occurrence: 0,
            },
            audio: Some(RenditionKey {
                group_id: "source-group".into(),
                language: language.map(str::to_owned),
                name: name.map(str::to_owned),
            }),
        }
    }

    #[test]
    fn master_playlist_round_trips_through_the_parser() {
        let key = selection(Some("中文"), Some("zh"), &["avc1.64001f", "mp4a.40.2"]);
        let Ok(Playlist::Master(master)) = parse(&master_playlist(&key), &url()) else {
            panic!("应为主播放列表");
        };
        let variant = &master.variants[0];
        assert_eq!(variant.uri.path(), "/out/0/index.m3u8");
        assert_eq!(variant.bandwidth, Some(2_000_000));
        assert_eq!(variant.codecs, ["avc1.64001f", "mp4a.40.2"]);
        assert_eq!(variant.audio.as_deref(), Some("audio"));
        let audio = &master.renditions[0];
        assert_eq!(audio.uri.as_ref().unwrap().path(), "/out/1/index.m3u8");
        assert_eq!(
            (
                audio.group_id.as_str(),
                audio.name.as_deref(),
                audio.language.as_deref()
            ),
            ("audio", Some("中文"), Some("zh"))
        );
        assert!(audio.default);

        // 带引号的名称与编码写不进带引号的字符串：名称改用语言，编码不写
        let odd = selection(Some("a\"b"), Some("en"), &["avc1\"x"]);
        let Ok(Playlist::Master(master)) = parse(&master_playlist(&odd), &url()) else {
            panic!("应为主播放列表");
        };
        assert_eq!(master.renditions[0].name.as_deref(), Some("en"));
        assert!(master.variants[0].codecs.is_empty());
    }

    #[test]
    fn only_names_written_by_the_library_are_recognized() {
        for name in [
            "index.m3u8",
            "init-0123456789abcdef.mp4",
            "0.ts",
            "12.m4s",
            "3.eac3",
        ] {
            assert!(is_track_file_name(name), "{name}");
        }
        for name in [
            "index.m3u8.part",
            "init-xyz.mp4",
            "01.ts",
            "1.mp4",
            "a.ts",
            ".ts",
            "1",
            "notes.txt",
        ] {
            assert!(!is_track_file_name(name), "{name}");
        }
        assert!(is_number("0") && is_number("10"));
        assert!(!is_number("") && !is_number("00") && !is_number("-1"));
    }
}
