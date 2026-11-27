//! 用 bindgen 生成绑定。scripts/ffmpeg_bindings.py 开 `bindgen` 特性构建，取生成的文件与 `$OUT_DIR/libclang.txt`（生成用的
//! libclang 的版本）。

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
