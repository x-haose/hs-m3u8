//! 选定绑定并静态链接 FFmpeg。
//!
//! - FFmpeg：`FFMPEG_DIR`（third_party/ffmpeg/build.sh 编译安装的目录，含 include 与 lib）；没设时下载本目标的预编译库
//!   （见 [`download`]）。下面「FFmpeg 目录」指两者之一。
//! - 绑定：默认用 `src/bindings/<目标>.rs`（由 CI 开 `bindgen` 特性生成，见 .github/actions/ffmpeg-bindings）；开
//!   `bindgen` 特性时在构建时从 FFmpeg 目录的 include 生成。选中的文件经环境变量 `HS_M3U8_FFMPEG_BINDINGS` 交给 lib.rs。
//! - 版本：FFmpeg 目录里三个库的版本（`.pc` 的 `Version` 行）写成 `$OUT_DIR/ffmpeg_dir.rs` 里的编译期断言，由 lib.rs
//!   与绑定里的版本常量比较：不一致时结构体布局可能不同，编译失败。
//! - 链接：FFmpeg 目录 lib 下的 avformat、avcodec、avutil 三个静态库，加上 `.pc` 文件 `Libs` 行里的系统库（如 macOS 的
//!   CoreFoundation、Windows 上 `av_get_random_seed` 要的 bcrypt）。
//! - `DOCS_RS` 有值时用预生成的绑定、不链接也不下载：docs.rs 构建时没有 FFmpeg，也不能联网。

mod download;
#[cfg(feature = "bindgen")]
mod generate;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// 链接的 FFmpeg 库，按链接顺序（依赖方在前）。
const LIBS: [&str; 3] = ["avformat", "avcodec", "avutil"];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-changed=src/bindings");
    let target = env::var("TARGET").expect("cargo 给构建脚本设了 TARGET");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo 设了 OUT_DIR"));
    if env::var_os("DOCS_RS").is_some() {
        use_bindings(&committed_bindings(&target));
        write(&out.join("ffmpeg_dir.rs"), "");
        return;
    }
    let dir = match env::var_os("FFMPEG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => download::prebuilt(&target, &out),
    };
    use_bindings(&bindings(&target, &dir, &out));
    write(&out.join("ffmpeg_dir.rs"), &version_checks(&dir));
    link(&dir, &target);
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|e| panic!("写 {} 失败：{e}", path.display()));
}

/// 让 lib.rs 引入绑定文件 `path`。
fn use_bindings(path: &Path) {
    println!("cargo:rustc-env=HS_M3U8_FFMPEG_BINDINGS={}", path.display());
}

/// 本目标预生成的绑定文件。
fn committed_bindings(target: &str) -> PathBuf {
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo 设了 CARGO_MANIFEST_DIR"));
    let path = manifest.join("src/bindings").join(format!("{target}.rs"));
    if !path.is_file() {
        panic!(
            "没有目标 {target} 的预生成绑定（{}）。开 bindgen 特性在构建时生成，要 libclang 与 FFMPEG_DIR；依赖方与 \
             Python 包怎样打开它见 hs-m3u8 仓库 README 的「从源码构建」",
            path.display()
        );
    }
    path
}

/// 绑定文件的路径（见模块说明）。
#[cfg(not(feature = "bindgen"))]
fn bindings(target: &str, _dir: &Path, _out: &Path) -> PathBuf {
    committed_bindings(target)
}

/// 由 FFmpeg 目录 `dir` 的头文件生成的绑定（见模块说明），连同生成用的 libclang 的版本写在 `out` 下。
#[cfg(feature = "bindgen")]
fn bindings(target: &str, dir: &Path, out: &Path) -> PathBuf {
    let bindings = out.join("bindings.rs");
    generate::write(&dir.join("include"), target, &bindings);
    write(&out.join("libclang.txt"), &bindgen::clang_version().full);
    bindings
}

/// `dir` 下三个库的 `.pc` 文件里的版本（`主.次.修订`），写成与绑定里 `LIB<名>_VERSION_MAJOR` 等常量比较的编译期断言。
fn version_checks(dir: &Path) -> String {
    let mut checks = String::new();
    for name in LIBS {
        let pc = dir.join("lib/pkgconfig").join(format!("lib{name}.pc"));
        println!("cargo:rerun-if-changed={}", pc.display());
        let text =
            fs::read_to_string(&pc).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", pc.display()));
        let version = text
            .lines()
            .find_map(|line| line.strip_prefix("Version:"))
            .unwrap_or_else(|| panic!("{} 里没有 Version 行", pc.display()))
            .trim();
        let parts: Vec<u32> = version
            .split('.')
            .map(|part| {
                part.parse().unwrap_or_else(|_| {
                    panic!("{} 的版本 {version:?} 不是 主.次.修订", pc.display())
                })
            })
            .collect();
        let [major, minor, micro] = parts[..] else {
            panic!("{} 的版本 {version:?} 不是 主.次.修订", pc.display());
        };
        let upper = name.to_uppercase();
        checks.push_str(&format!(
            "const _: () = assert!(\n    \
             LIB{upper}_VERSION_MAJOR == {major} && LIB{upper}_VERSION_MINOR == {minor} && LIB{upper}_VERSION_MICRO == {micro},\n    \
             {:?}\n);\n",
            format!(
                "FFmpeg 目录 {} 里的 lib{name} 是 {version}，与绑定的版本（LIB{upper}_VERSION_*）不同：结构体布局可能对\
                 不上。用与绑定对应的 FFmpeg（hs-m3u8 仓库的 third_party/ffmpeg/build.sh），或开 bindgen 特性按它生成绑定",
                dir.display()
            )
        ));
    }
    checks
}

/// 链接 `dir` 下的静态 FFmpeg 及其依赖的系统库。
fn link(dir: &Path, target: &str) {
    let lib = dir.join("lib");
    // 静态库会被打包进本 crate 的 rlib：FFmpeg 重新编译后（如增减组件）构建脚本要重跑，否则链接的仍是旧库
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rerun-if-changed={}", dir.join("include").display());
    let msvc = target.ends_with("-msvc");
    for name in LIBS {
        let file = lib.join(if msvc {
            format!("{name}.lib")
        } else {
            format!("lib{name}.a")
        });
        if !file.is_file() {
            panic!(
                "FFmpeg 目录 {} 里没有编译好的 FFmpeg 静态库 {}：用 hs-m3u8 仓库的 third_party/ffmpeg/build.sh 编译，\
                 或把 FFMPEG_DIR 设为按它的参数编译安装的 FFmpeg 目录",
                dir.display(),
                file.display()
            );
        }
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in LIBS {
        println!("cargo:rustc-link-lib=static={name}");
    }
    for link in system_libs(&lib.join("pkgconfig")) {
        println!("cargo:rustc-link-lib={link}");
    }
}

/// 各静态库的 `.pc` 文件 `Libs` 与 `Libs.private` 行里的系统库，按出现的顺序去重，写成 `rustc-link-lib` 的值
/// （`bcrypt`、`framework=CoreFoundation`）。FFmpeg 自身的库、libm 与 `-pthread`（Rust 标准库已链接）、`-L`（指向 FFmpeg
/// 自己的库目录）不算。
fn system_libs(pkgconfig: &Path) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    for name in LIBS {
        let pc = pkgconfig.join(format!("lib{name}.pc"));
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
                    // libm 由 Rust 标准库在 Unix 上链接，再给一次 macOS 的链接器会警告重复的库
                    if LIBS.contains(&name) || name == "m" {
                        continue;
                    }
                    name.to_owned()
                } else if token.starts_with("-L") || token == "-pthread" {
                    continue;
                } else {
                    panic!("{} 的链接参数中有无法识别的项 {token:?}", pc.display());
                };
                if !links.contains(&link) {
                    links.push(link);
                }
            }
        }
    }
    links
}
