"""重新生成 hs-m3u8-ffmpeg-sys 在本机目标上的预生成绑定 crates/ffmpeg-sys/src/bindings/<目标>.rs。

开 `bindgen` 特性构建这个 crate，把构建脚本生成的绑定复制过去。要 libclang，`FFMPEG_DIR` 取 .cargo/config.toml 里的
third_party/ffmpeg/dist（先 make ffmpeg）。FFmpeg 换了版本、build.rs 的清单改了之后运行；CI 在 macOS 上运行后核对与
提交的一致。
"""

import json
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = "hs-m3u8-ffmpeg-sys"
BINDINGS = ROOT / "crates/ffmpeg-sys/src/bindings"


def host() -> str:
    """本机的目标三元组（rustc 的 host）。"""
    output = subprocess.run(["rustc", "-vV"], cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    message = f"rustc -vV 的输出里没有 host：{output!r}"
    raise SystemExit(message)


def generated() -> Path:
    """开 bindgen 特性构建，返回构建脚本生成的绑定文件；构建失败时 cargo 的报错照常输出到 stderr。"""
    build = [
        "cargo",
        "build",
        "--locked",
        "-p",
        PACKAGE,
        "--features",
        "bindgen",
        "--message-format=json-render-diagnostics",
    ]
    output = subprocess.run(build, cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True).stdout
    for line in output.splitlines():
        event = json.loads(line)
        if event["reason"] == "build-script-executed" and f"#{PACKAGE}@" in event["package_id"]:
            return Path(event["out_dir"]) / "bindings.rs"
    message = f"cargo 的输出里没有 {PACKAGE} 构建脚本的运行结果"
    raise SystemExit(message)


def main() -> int:
    """生成并复制到 crates/ffmpeg-sys/src/bindings 下。"""
    target = BINDINGS / f"{host()}.rs"
    shutil.copyfile(generated(), target)
    sys.stdout.write(f"已生成 {target.relative_to(ROOT)}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
