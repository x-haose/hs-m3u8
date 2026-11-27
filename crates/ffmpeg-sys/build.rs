//! 选定绑定并静态链接 FFmpeg。
//!
//! - 绑定：默认用 `src/bindings/<目标>.rs`（由 CI 开 `bindgen` 特性生成，见 .github/actions/ffmpeg-bindings）；开
//!   `bindgen` 特性时在构建时从 `$FFMPEG_DIR/include` 生成。选中的文件经环境变量 `HS_M3U8_FFMPEG_BINDINGS` 交给 lib.rs。
//! - 版本：`$FFMPEG_DIR` 里三个库的版本（`.pc` 的 `Version` 行）写成 `$OUT_DIR/ffmpeg_dir.rs` 里的编译期断言，由 lib.rs
//!   与绑定里的版本常量比较：不一致时结构体布局可能不同，编译失败。
//! - 链接：`$FFMPEG_DIR/lib` 下由 third_party/ffmpeg/build.sh 编出的 avformat、avcodec、avutil 三个静态库，加上 FFmpeg
//!   安装的 `.pc` 文件 `Libs` 行里的系统库（如 macOS 的 CoreFoundation、Windows 上 `av_get_random_seed` 要的 bcrypt）。
//! - `DOCS_RS` 有值时不链接：docs.rs 构建时没有 FFmpeg，也不能联网。

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
    let bindings = bindings(&target);
    println!(
        "cargo:rustc-env=HS_M3U8_FFMPEG_BINDINGS={}",
        bindings.display()
    );
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo 设了 OUT_DIR"));
    if env::var_os("DOCS_RS").is_some() {
        write(&out.join("ffmpeg_dir.rs"), "");
        return;
    }
    let dir = ffmpeg_dir();
    write(&out.join("ffmpeg_dir.rs"), &version_checks(&dir));
    link(&dir, &target);
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|e| panic!("写 {} 失败：{e}", path.display()));
}

/// 绑定文件的路径（见模块说明）。
#[cfg(not(feature = "bindgen"))]
fn bindings(target: &str) -> PathBuf {
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
#[cfg(feature = "bindgen")]
fn bindings(target: &str) -> PathBuf {
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo 设了 OUT_DIR"));
    let bindings = out.join("bindings.rs");
    generate::write(&ffmpeg_dir().join("include"), target, &bindings);
    write(&out.join("libclang.txt"), &bindgen::clang_version().full);
    bindings
}

/// `FFMPEG_DIR`：third_party/ffmpeg/build.sh 编译安装的 FFmpeg 目录（含 include 与 lib）。
fn ffmpeg_dir() -> PathBuf {
    match env::var_os("FFMPEG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => panic!(
            "没有设 FFMPEG_DIR：用 hs-m3u8 仓库的 third_party/ffmpeg/build.sh 编译 FFmpeg（装到 \
             third_party/ffmpeg/dist），再把 FFMPEG_DIR 设为安装目录"
        ),
    }
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
                "FFMPEG_DIR（{}）里的 lib{name} 是 {version}，与绑定的版本（LIB{upper}_VERSION_*）不同：结构体布局可能对不上。\
                 用与绑定对应的 FFmpeg（hs-m3u8 仓库的 third_party/ffmpeg/build.sh），或开 bindgen 特性按它生成绑定",
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
                "FFMPEG_DIR（{}）里没有编译好的 FFmpeg 静态库 {}：用 hs-m3u8 仓库的 third_party/ffmpeg/build.sh 编译，\
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

/// 用 bindgen 生成绑定。scripts/ffmpeg_bindings.py 开 `bindgen` 特性构建，取生成的文件与 `$OUT_DIR/libclang.txt`（生成用的
/// libclang 的版本）。
#[cfg(feature = "bindgen")]
mod generate {
    use std::env;
    use std::path::Path;
    use std::process::Command;

    use bindgen::callbacks::{IntKind, ParseCallbacks};

    /// 用到的函数：remux 的封装，及其测试里解码校验用的（av_find_best_stream 与 avcodec_、av_frame_ 开头的解码函数）。
    const FUNCTIONS: [&str; 45] = [
        "av_dict_free",
        "av_dict_get",
        "av_dict_set",
        "av_find_best_stream",
        "av_frame_alloc",
        "av_frame_free",
        "av_free",
        "av_freep",
        "av_interleaved_write_frame",
        "av_log_set_level",
        "av_mallocz",
        "av_new_packet",
        "av_packet_alloc",
        "av_packet_free",
        "av_packet_unref",
        "av_pix_fmt_desc_get_id",
        "av_pix_fmt_desc_next",
        "av_probe_input_format2",
        "av_read_frame",
        "av_rescale_q",
        "av_rescale_q_rnd",
        "av_stream_get_parser",
        "av_strerror",
        "av_write_trailer",
        "avcodec_alloc_context3",
        "avcodec_find_decoder",
        "avcodec_free_context",
        "avcodec_get_name",
        "avcodec_open2",
        "avcodec_parameters_copy",
        "avcodec_parameters_to_context",
        "avcodec_receive_frame",
        "avcodec_send_packet",
        "avformat_alloc_context",
        "avformat_alloc_output_context2",
        "avformat_close_input",
        "avformat_find_stream_info",
        "avformat_free_context",
        "avformat_new_stream",
        "avformat_open_input",
        "avformat_write_header",
        "avio_alloc_context",
        "avio_closep",
        "avio_context_free",
        "avio_open2",
    ];

    /// 用到值的枚举（生成为整数常量）；结构体由函数的参数与返回值带出。
    const TYPES: [&str; 3] = ["AVCodecID", "AVMediaType", "AVRounding"];

    /// 只经指针传递、不读写字段的结构体：生成为大小与对齐相同的不透明字节块，不依赖它们的字段布局。
    const OPAQUE: [&str; 3] = ["AVCodec", "AVCodecContext", "AVFrame"];

    /// 用到的宏常量，及三个库的版本（build.rs 与 FFMPEG_DIR 里的比较，见模块说明）。
    const VARS: [&str; 20] = [
        "AVFMT_FLAG_CUSTOM_IO",
        "AVIO_FLAG_WRITE",
        "AVPROBE_PADDING_SIZE",
        "AVSEEK_FORCE",
        "AVSEEK_SIZE",
        "AV_DICT_IGNORE_SUFFIX",
        "AV_DISPOSITION_DEFAULT",
        "AV_DISPOSITION_FORCED",
        "AV_INPUT_BUFFER_PADDING_SIZE",
        "AV_LOG_QUIET",
        "AV_PKT_FLAG_KEY",
        "LIBAVCODEC_VERSION_MAJOR",
        "LIBAVCODEC_VERSION_MICRO",
        "LIBAVCODEC_VERSION_MINOR",
        "LIBAVFORMAT_VERSION_MAJOR",
        "LIBAVFORMAT_VERSION_MICRO",
        "LIBAVFORMAT_VERSION_MINOR",
        "LIBAVUTIL_VERSION_MAJOR",
        "LIBAVUTIL_VERSION_MICRO",
        "LIBAVUTIL_VERSION_MINOR",
    ];

    /// 宏常量都按 C 的 int 生成：它们在 FFmpeg 里与 int 型的参数、字段一起用。
    #[derive(Debug)]
    struct IntMacros;

    impl ParseCallbacks for IntMacros {
        fn int_macro(&self, _name: &str, _value: i64) -> Option<IntKind> {
            Some(IntKind::Int)
        }
    }

    /// 由 `include`（FFmpeg 的头文件目录）为目标 `target` 生成绑定，写到 `out`。
    pub(super) fn write(include: &Path, target: &str, out: &Path) {
        println!("cargo:rerun-if-changed=wrapper.h");
        println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS");
        let mut builder = bindgen::Builder::default()
            .header("wrapper.h")
            .clang_arg(format!("-I{}", include.display()))
            .rust_edition(bindgen::RustEdition::Edition2024)
            .default_enum_style(bindgen::EnumVariation::Consts)
            .prepend_enum_name(false)
            .generate_comments(false)
            .sort_semantically(true)
            .parse_callbacks(Box::new(IntMacros));
        for function in FUNCTIONS {
            builder = builder.allowlist_function(function);
        }
        for ty in TYPES {
            builder = builder.allowlist_type(ty);
        }
        for var in VARS {
            builder = builder.allowlist_var(var);
        }
        for ty in OPAQUE {
            // 不透明的类型 bindgen 不顺着函数签名带出，要单独列进 allowlist
            builder = builder.opaque_type(ty).allowlist_type(ty);
        }
        if target.ends_with("-apple-darwin") && env::var_os("BINDGEN_EXTRA_CLANG_ARGS").is_none() {
            // bindgen 加载的 libclang 不一定带默认的 SDK 路径（如 Homebrew 的 LLVM），那样找不到 stdint.h 等系统头文件
            builder = builder.clang_arg(format!("-isysroot{}", macos_sdk()));
        }
        builder
            .generate()
            .unwrap_or_else(|e| panic!("生成绑定失败：{e}"))
            .write_to_file(out)
            .unwrap_or_else(|e| panic!("写 {} 失败：{e}", out.display()));
    }

    /// 本机 macOS SDK 的路径（`xcrun --show-sdk-path`）。
    fn macos_sdk() -> String {
        let output = Command::new("xcrun")
            .arg("--show-sdk-path")
            .output()
            .unwrap_or_else(|e| panic!("运行 xcrun 失败（要装 Xcode 命令行工具）：{e}"));
        if !output.status.success() {
            panic!(
                "xcrun --show-sdk-path 失败（要装 Xcode 命令行工具）：{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8(output.stdout)
            .expect("xcrun 给出的 SDK 路径是 UTF-8")
            .trim()
            .to_owned()
    }
}
