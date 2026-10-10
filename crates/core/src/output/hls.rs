//! 本地 HLS 输出：任务目录中已解密的分片原样放进一个可直接播放的目录，不经 FFmpeg。
//!
//! ```text
//! index.m3u8              入口：单轨时为媒体播放列表；视频与独立音频分离时为主播放列表
//! <轨道>/index.m3u8        音视频分离时这条轨的媒体播放列表
//! <轨道>/<n>.<扩展名>       分片，n 为这条轨里的播放顺序（从 0 起）；扩展名见 extension
//! <轨道>/init-<指纹>.mp4    fMP4 的 init 段，文件名同任务目录
//! ```
//!
//! 先在准备目录里备齐，最后改名为目标目录。只放各轨都有的不连续段组（与 MP4 相同），组与组之间加
//! EXT-X-DISCONTINUITY；组内缺失的分片不标出，保留原时间戳，与 MP4 一致：缺失往往只在一条轨上，只给一条轨
//! 加不连续标记会让各轨的不连续段编号对不上（RFC 8216 6.2.4 要求各轨一致）。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{GroupTrack, io_error};
use crate::ident::Fingerprint;
use crate::selection::SelectionKey;
use crate::verify::id3_len;
use crate::verify::{Standalone, standalone_format};
use crate::{Error, Unsupported, WorkDirProblem, workdir};

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

/// 目标目录能否写：不存在、没有内容（系统自动生成的元数据文件不算，见 [`workdir::is_system_file`]），或 `overwrite` 且其中
/// 全是本库写出的文件。全是本库写出的文件而没有要求覆盖时报 [`Error::OutputExists`]；有别的文件或不是目录时报
/// [`Error::OutputOccupied`]，覆盖也不替换。
pub(super) fn check(dir: &Path, overwrite: bool) -> Result<(), Error> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(cause) => return Err(io_error("检查", dir)(cause)),
    };
    if !meta.is_dir() || !written_by_us(dir)? {
        return Err(Error::OutputOccupied(dir.to_path_buf()));
    }
    if overwrite || is_empty(dir)? {
        Ok(())
    } else {
        Err(Error::OutputExists(dir.to_path_buf()))
    }
}

/// 本地 HLS 能否表示这些组：每条轨要么各组都有 init 段，要么都没有。EXT-X-MAP 一直作用到下一个 EXT-X-MAP，
/// 没有 init 段的组接在有的组后面时，播放器会把前面的 init 段用在它上面。
pub(super) fn check_layout(groups: &[Vec<GroupTrack>]) -> Result<(), Error> {
    let tracks = groups.first().map_or(0, Vec::len);
    for track in 0..tracks {
        let fmp4 = groups[0][track].init.is_some();
        if groups.iter().any(|g| g[track].init.is_some() != fmp4) {
            return Err(Error::Unsupported(Unsupported::HlsMixedInit { track }));
        }
    }
    Ok(())
}

/// 在准备目录 `stage` 里备齐 HLS 输出；`work_dir` 为分片所在的任务目录，用于报告目录内容无法识别。上次中断
/// 留下的同名准备目录全是本库写出的文件才删，否则报已存在、什么也不动；备齐失败时删掉 `stage`。
pub(super) fn stage(
    stage: &Path,
    work_dir: &Path,
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
) -> Result<(), Error> {
    remove_stale_stage(stage)?;
    if let Some(parent) = stage.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error("创建", parent))?;
    }
    fs::create_dir(stage).map_err(io_error("创建", stage))?;
    fill(stage, work_dir, groups, selection).map_err(|failure| match fs::remove_dir_all(stage) {
        Ok(()) => failure,
        Err(cause) => Error::Cleanup {
            failure: Box::new(failure),
            path: stage.to_path_buf(),
            cause,
        },
    })
}

/// 备齐的 `stage` 改名为 `target`：`target` 已存在时按 [`check`] 替换。备齐期间 `target` 可能被别人占用，
/// 替换前再查一次；查过之后别人又抢先写好了 `target`（几个任务输出到同一处）时改名失败，报
/// [`Error::OutputExists`]，不会混在一起。
pub(super) fn replace(stage: &Path, target: &Path, overwrite: bool) -> Result<(), Error> {
    check(target, overwrite)?;
    if let Err(e) = fs::remove_dir_all(target)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(io_error("删除", target)(e));
    }
    fs::rename(stage, target).map_err(|e| match fs::symlink_metadata(target) {
        Ok(_) => Error::OutputExists(target.to_path_buf()),
        Err(_) => io_error("重命名", stage)(e),
    })
}

/// 上次中断留下的准备目录：全是本库写出的文件才删，否则报已存在。
fn remove_stale_stage(stage: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(stage) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(io_error("检查", stage)(cause)),
        Ok(meta) if meta.is_dir() && written_by_us(stage)? => {
            fs::remove_dir_all(stage).map_err(io_error("删除", stage))
        }
        Ok(_) => Err(Error::OutputOccupied(stage.to_path_buf())),
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
    let mut tracks = Vec::with_capacity(track_count);
    for track in 0..track_count {
        let dir = stage.join(track.to_string());
        fs::create_dir(&dir).map_err(io_error("创建", &dir))?;
        tracks.push(fill_track(
            &dir,
            work_dir,
            groups.iter().map(|g| &g[track]),
        )?);
    }
    match (&tracks[..], selection) {
        ([only], _) => write_file(&stage.join(INDEX), &media_playlist(&only.groups, "0/")),
        ([video, audio], Some(selection)) => {
            for (track, written) in tracks.iter().enumerate() {
                let path = stage.join(track.to_string()).join(INDEX);
                write_file(&path, &media_playlist(&written.groups, ""))?;
            }
            let bandwidth = video
                .peak_bps
                .zip(audio.peak_bps)
                .map(|(v, a)| v.saturating_add(a));
            write_file(&stage.join(INDEX), &master_playlist(selection, bandwidth))
        }
        (tracks, selection) => panic!(
            "轨道只有所选变体与独立音频两种，后者只来自主播放列表：{} 条，有选轨 {}",
            tracks.len(),
            selection.is_some()
        ),
    }
}

/// 一条轨放进 HLS 目录的内容。
struct TrackFiles {
    /// 各组在播放列表中的内容（地址相对于轨道目录）
    groups: Vec<PlaylistGroup>,
    /// 分片的峰值码率（字节数 × 8 ÷ 声明时长，向上取整），bit/s；没有声明时长大于 0 的分片时为 None
    peak_bps: Option<u64>,
}

/// 放好一条轨各组的分片与 init 段。
fn fill_track<'a>(
    dir: &Path,
    work_dir: &Path,
    groups: impl Iterator<Item = &'a GroupTrack>,
) -> Result<TrackFiles, Error> {
    let mut placed_inits = HashSet::new();
    let mut written = TrackFiles {
        groups: Vec::new(),
        peak_bps: None,
    };
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
            let len = place(&segment.path, &dir.join(&name))?;
            if let Some(bps) = bits_per_second(len, segment.duration_us) {
                written.peak_bps = Some(written.peak_bps.map_or(bps, |peak| peak.max(bps)));
            }
            segments.push((name, segment.duration_us));
            index += 1;
        }
        written.groups.push(PlaylistGroup { init, segments });
    }
    Ok(written)
}

/// `len` 字节、声明时长 `duration_us` 微秒的分片的码率，bit/s，向上取整；时长为 0 时为 None。
fn bits_per_second(len: u64, duration_us: u64) -> Option<u64> {
    let bits = u128::from(len) * 8 * 1_000_000;
    let duration = u128::from(duration_us);
    (duration > 0).then(|| u64::try_from(bits.div_ceil(duration)).unwrap_or(u64::MAX))
}

/// 识别格式时每次读取的字节数：够判断 TS 的前两个包（376 字节）与 ID3 标签头。
const SNIFF_LEN: u64 = 1024;

/// 没有 init 段的分片的格式。打包音频前的 ID3 标签按长度跳过，不整个读进来（带封面图时可以很大）。
fn sniff(path: &Path, work_dir: &Path) -> Result<Standalone, Error> {
    let mut file = File::open(path).map_err(io_error("读取", path))?;
    let mut offset = 0u64;
    loop {
        let mut head = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| (&mut file).take(SNIFF_LEN).read_to_end(&mut head))
            .map_err(io_error("读取", path))?;
        match id3_len(&head) {
            Some(len) => offset += len as u64,
            None => {
                return standalone_format(&head).ok_or_else(|| Error::WorkDir {
                    path: work_dir.to_path_buf(),
                    problem: WorkDirProblem::Corrupt(format!(
                        "分片内容无法识别：{}",
                        path.display()
                    )),
                });
            }
        }
    }
}

/// 把 `src` 放到新文件 `dst`，返回字节数：能硬链接就硬链接，不占额外空间；做不了时（跨文件系统、文件系统不支持）
/// 复制并落盘。`dst` 已存在时报错，不写穿已有的文件：它可能是别处文件的硬链接。
fn place(src: &Path, dst: &Path) -> Result<u64, Error> {
    match fs::hard_link(src, dst) {
        Ok(()) => return Ok(fs::metadata(dst).map_err(io_error("读取", dst))?.len()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io_error("链接", dst)(e));
        }
        Err(_) => {}
    }
    let mut to = File::options()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(io_error("创建", dst))?;
    let len = File::open(src)
        .and_then(|mut from| io::copy(&mut from, &mut to))
        .map_err(io_error("复制", src))?;
    to.sync_all().map_err(io_error("落盘", dst))?;
    Ok(len)
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

/// 视频与独立音频分离时的主播放列表：第 0 条轨为变体，第 1 条为音频 rendition。BANDWIDTH 为 `bandwidth`（两条轨
/// 分片峰值码率之和，是 RFC 8216 4.3.4.2 要求的上限；算不出时不写），分辨率与编码照抄来源（来源没写的也不写）；
/// 音频组固定为 `audio`。名称与语言照抄来源，带有引号或换行（带引号的字符串里不允许）
/// 时不用：没有可用的名称时名称为 `audio`，语言不写。
fn master_playlist(selection: &SelectionKey, bandwidth: Option<u64>) -> String {
    let audio = &selection
        .audio
        .as_ref()
        .expect("两条轨时第 1 条是独立的音频 rendition")
        .attributes;
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
    if let Some(bandwidth) = bandwidth {
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

/// 目录里除了系统自动生成的元数据文件之外没有别的。
fn is_empty(dir: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        if !is_system_file(&entry)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_system_file(entry: &fs::DirEntry) -> Result<bool, Error> {
    let kind = entry.file_type().map_err(io_error("读取", &entry.path()))?;
    Ok(workdir::is_system_file(&entry.file_name(), kind))
}

/// 目录里是否全是本库写出的 HLS 文件（空目录也算，系统自动生成的元数据文件不算），即删掉它不会丢别人的文件。
/// 不跟随符号链接：符号链接不是本库写出的。
fn written_by_us(dir: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        let kind = entry.file_type().map_err(io_error("读取", &entry.path()))?;
        let name = entry.file_name();
        let ours = match name.to_str() {
            Some(INDEX) => kind.is_file(),
            Some(name) if is_number(name) && kind.is_dir() => track_written_by_us(&entry.path())?,
            _ => is_system_file(&entry)?,
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
        let ours = kind.is_file() && name.to_str().is_some_and(is_track_file_name);
        if !ours && !is_system_file(&entry)? {
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
    use crate::selection::{AudioAttributes, AudioKey, VariantAttributes, VariantKey};

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
            audio: Some(AudioKey {
                attributes: AudioAttributes {
                    group_id: "source-group".into(),
                    language: language.map(str::to_owned),
                    name: name.map(str::to_owned),
                },
                occurrence: 0,
            }),
        }
    }

    #[test]
    fn master_playlist_round_trips_through_the_parser() {
        let key = selection(Some("中文"), Some("zh"), &["avc1.64001f", "mp4a.40.2"]);
        let Ok(Playlist::Master(master)) = parse(&master_playlist(&key, Some(2_500_000)), &url())
        else {
            panic!("应为主播放列表");
        };
        let variant = &master.variants[0];
        assert_eq!(variant.uri.path(), "/out/0/index.m3u8");
        assert_eq!(variant.bandwidth, Some(2_500_000));
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
        let Ok(Playlist::Master(master)) = parse(&master_playlist(&odd, None), &url()) else {
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
