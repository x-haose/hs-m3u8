//! 按 FFmpeg 自己生成的 pkg-config 文件，补齐静态库依赖的系统库。
//!
//! ffmpeg-sys-next 在 `FFMPEG_DIR` 模式下只链接 avformat / avcodec / avutil 本身，不链接它们依赖的系统库
//! （如 Windows 上 `av_get_random_seed` 需要的 bcrypt）。FFmpeg 静态构建时把这些依赖写在 `.pc` 文件的 `Libs` 行。

use std::env;
use std::fs;
use std::path::PathBuf;

const FFMPEG_LIBS: [&str; 3] = ["avformat", "avcodec", "avutil"];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let dir =
        PathBuf::from(env::var("FFMPEG_DIR").expect("FFMPEG_DIR 未设置，见 .cargo/config.toml"));

    let mut emitted: Vec<String> = Vec::new();
    for lib in FFMPEG_LIBS {
        let pc = dir.join("lib/pkgconfig").join(format!("lib{lib}.pc"));
        println!("cargo:rerun-if-changed={}", pc.display());
        let text =
            fs::read_to_string(&pc).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", pc.display()));

        for line in text.lines() {
            let Some(rest) = line
                .strip_prefix("Libs:")
                .or_else(|| line.strip_prefix("Libs.private:"))
            else {
                continue;
            };
            let mut tokens = rest.split_whitespace();
            while let Some(token) = tokens.next() {
                let link = if token == "-framework" {
                    let name = tokens
                        .next()
                        .unwrap_or_else(|| panic!("{} 中 -framework 后缺少名称", pc.display()));
                    format!("framework={name}")
                } else if let Some(name) = token
                    .strip_prefix("-l")
                    .or_else(|| token.strip_suffix(".lib"))
                {
                    if FFMPEG_LIBS.contains(&name) {
                        continue;
                    }
                    name.to_owned()
                } else if token.starts_with("-L") || token == "-pthread" {
                    // -L 指向 FFmpeg 自身的库目录；-pthread 由 Rust 标准库的线程支持覆盖
                    continue;
                } else {
                    panic!("{} 的链接参数中有无法识别的项 {token:?}", pc.display());
                };
                if !emitted.contains(&link) {
                    println!("cargo:rustc-link-lib={link}");
                    emitted.push(link);
                }
            }
        }
    }
}
