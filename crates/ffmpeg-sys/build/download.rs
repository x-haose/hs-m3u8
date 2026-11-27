//! 预编译的 FFmpeg：没设 `FFMPEG_DIR` 时，从 hs-m3u8 的 GitHub Release 下载本目标的静态库压缩包（由
//! .github/workflows/ffmpeg.yml 编出并发布，内容与 build.sh 的安装目录相同），按 [`PREBUILT`] 里的 SHA-256 校验后存进
//! 缓存目录，每次构建解压到 `$OUT_DIR/ffmpeg`。
//!
//! - 缓存：`HS_M3U8_FFMPEG_CACHE_DIR`，没设时为用户缓存目录（macOS 的 `~/Library/Caches`、Linux 的
//!   `$XDG_CACHE_HOME` 或 `~/.cache`、Windows 的 LocalAppData）下的 `hs-m3u8-ffmpeg`。文件以 SHA-256 命名；哈希不符的
//!   删掉重下，不拿来用。
//! - 镜像：`HS_M3U8_FFMPEG_MIRROR` 换掉地址里的 [`RELEASES`] 这一段，其下的路径不变；哈希照样校验，镜像只能提供同样
//!   的文件。
//! - 离线：`CARGO_NET_OFFLINE` 为 `true`、缓存里又没有时直接失败。cargo 不把 `--offline` 告诉构建脚本，只认这个环境变量。
//! - 代理：读 `ALL_PROXY`、`HTTPS_PROXY`、`HTTP_PROXY`（含小写）与 `NO_PROXY`，支持 http 与 socks5 代理。

use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// 预编译库对应的 FFmpeg 版本与构建修订号：Release 的标签为 `ffmpeg-<版本>-<修订号>`。换任一项时 crate 升次版本号，
/// 依赖方 `cargo update` 不会换掉链接的 FFmpeg。
const VERSION: &str = "9.0.2";
const REVISION: u32 = 1;

/// 各目标压缩包 `<目标>.tar.gz` 的 SHA-256（小写十六进制），取自 Release 的 SHA256SUMS。
const PREBUILT: [(&str, &str); 4] = [
    (
        "aarch64-apple-darwin",
        "0b29a0be77aad053ebebf6d02a2acf3f96206639d5b71a3dd4b2b7fef7787a2d",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "0c49ce6ddf41e7b000c23fc02c2eb7aff8ec1b2c44330d5a2b99b7ac1dc6b230",
    ),
    (
        "x86_64-pc-windows-msvc",
        "b6fa7786c28f54faec4e4b7f587debb1a805260753f6cd7c67c872fdbb8ce07b",
    ),
    (
        "x86_64-unknown-linux-gnu",
        "baf42c008bb08bc705e2c3c673d9aa92ec76230227e30b5a020d23c5b78c6238",
    ),
];

/// Release 下载地址里标签之前的部分。
const RELEASES: &str = "https://github.com/x-haose/hs-m3u8/releases/download";

/// 建立连接（含代理与 TLS 握手）与两次读之间的最长等待：超时即失败，不无限挂住构建。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 本目标的预编译库解压到 `out` 下的目录（含 include 与 lib）。目标不在 [`PREBUILT`] 里、下载或校验失败时 panic。
pub(super) fn prebuilt(target: &str, out: &Path) -> PathBuf {
    for var in [
        "HS_M3U8_FFMPEG_CACHE_DIR",
        "HS_M3U8_FFMPEG_MIRROR",
        "CARGO_NET_OFFLINE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let Some(&(_, sha256)) = PREBUILT.iter().find(|(t, _)| *t == target) else {
        let supported: Vec<&str> = PREBUILT.iter().map(|(t, _)| *t).collect();
        panic!(
            "目标 {target} 没有预编译的 FFmpeg（有的目标：{}）。用 hs-m3u8 仓库的 third_party/ffmpeg/build.sh 编译 \
             FFmpeg {VERSION}，再把 FFMPEG_DIR 设为安装目录",
            supported.join("、")
        );
    };
    let archive = cached(target, sha256);
    let dir = out.join("ffmpeg");
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap_or_else(|e| panic!("删除 {} 失败：{e}", dir.display()));
    }
    unpack(&archive, &dir);
    dir
}

/// 缓存里本目标的压缩包，没有或哈希不符时下载。
fn cached(target: &str, sha256: &str) -> PathBuf {
    let cache = cache_dir();
    let file = cache.join(format!("{sha256}.tar.gz"));
    if file.is_file() {
        if digest_of(&file) == sha256 {
            return file;
        }
        fs::remove_file(&file)
            .unwrap_or_else(|e| panic!("删除哈希不符的 {} 失败：{e}", file.display()));
    }
    if env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true") {
        panic!(
            "CARGO_NET_OFFLINE 为 true，而缓存 {} 里没有目标 {target} 的预编译 FFmpeg：先联网构建一次，或设 FFMPEG_DIR",
            cache.display()
        );
    }
    fs::create_dir_all(&cache).unwrap_or_else(|e| panic!("创建 {} 失败：{e}", cache.display()));
    let base = env::var("HS_M3U8_FFMPEG_MIRROR").unwrap_or_else(|_| RELEASES.to_owned());
    let url = format!(
        "{}/ffmpeg-{VERSION}-{REVISION}/{target}.tar.gz",
        base.trim_end_matches('/')
    );
    // 同一缓存目录可能有几个构建同时下载：各写各的临时文件，校验通过后改名换上（同一目录内改名是原子的）
    let part = cache.join(format!("{sha256}.tar.gz.{}.part", std::process::id()));
    let actual = download(&url, &part);
    if actual != sha256 {
        let removed = fs::remove_file(&part);
        panic!(
            "下载的 {url} 的 SHA-256 为 {actual}，应为 {sha256}（{}）：文件被换过或镜像给的不是同一份",
            match removed {
                Ok(()) => "已删除".to_owned(),
                Err(e) => format!("删除 {} 失败：{e}", part.display()),
            }
        );
    }
    fs::rename(&part, &file)
        .unwrap_or_else(|e| panic!("把 {} 改名为 {} 失败：{e}", part.display(), file.display()));
    file
}

/// `HS_M3U8_FFMPEG_CACHE_DIR`，没设时为用户缓存目录下的 `hs-m3u8-ffmpeg`。用户缓存目录按构建所在的系统（与目标无关）：
/// macOS 为 `$HOME/Library/Caches`，Windows 为 `%LOCALAPPDATA%`，其余为 `$XDG_CACHE_HOME`（须为绝对路径）或
/// `$HOME/.cache`。
fn cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os("HS_M3U8_FFMPEG_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    let absolute = |var: &str| {
        env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let user = if cfg!(target_os = "macos") {
        absolute("HOME").map(|home| home.join("Library/Caches"))
    } else if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else {
        absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))
    };
    user.map(|dir| dir.join("hs-m3u8-ffmpeg")).unwrap_or_else(|| {
        panic!("找不到用户缓存目录（HOME 等没设或不是绝对路径）：设 HS_M3U8_FFMPEG_CACHE_DIR 或 FFMPEG_DIR")
    })
}

/// 把 `url` 下载到 `to`，返回内容的 SHA-256。失败时删掉 `to` 并 panic。
fn download(url: &str, to: &Path) -> String {
    let fail = |what: String| -> ! {
        let removed = match fs::remove_file(to) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                format!("；删除 {} 失败：{e}", to.display())
            }
            _ => String::new(),
        };
        panic!(
            "下载预编译的 FFmpeg（{url}）失败：{what}{removed}。可设 HS_M3U8_FFMPEG_MIRROR 换镜像、HTTPS_PROXY 等走代理，\
             或用 hs-m3u8 仓库的 third_party/ffmpeg/build.sh 编译后设 FFMPEG_DIR"
        )
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(ureq::Proxy::try_from_env())
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_per_read(Some(READ_TIMEOUT))
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .unwrap_or_else(|e| fail(e.to_string()));
    let mut body = response.into_body().into_reader();
    let mut file =
        File::create(to).unwrap_or_else(|e| fail(format!("创建 {} 失败：{e}", to.display())));
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => fail(e.to_string()),
        };
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .unwrap_or_else(|e| fail(format!("写 {} 失败：{e}", to.display())));
    }
    file.sync_all()
        .unwrap_or_else(|e| fail(format!("写 {} 失败：{e}", to.display())));
    hex(&hasher.finalize())
}

/// 文件内容的 SHA-256。
fn digest_of(path: &Path) -> String {
    let mut file = File::open(path).unwrap_or_else(|e| panic!("打开 {} 失败：{e}", path.display()));
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => panic!("读 {} 失败：{e}", path.display()),
        }
    }
    hex(&hasher.finalize())
}

/// 小写十六进制。
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 把校验过的压缩包解到 `dir`（gzip 压缩的 tar）。
fn unpack(archive: &Path, dir: &Path) {
    let file =
        File::open(archive).unwrap_or_else(|e| panic!("打开 {} 失败：{e}", archive.display()));
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(dir)
        .unwrap_or_else(|e| panic!("把 {} 解到 {} 失败：{e}", archive.display(), dir.display()));
}
