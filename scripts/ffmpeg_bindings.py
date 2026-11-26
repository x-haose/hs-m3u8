"""生成 hs-m3u8-ffmpeg-sys 在本机目标上的绑定，写到 crates/ffmpeg-sys/src/bindings/<目标>.rs。

开 `bindgen` 特性构建这个 crate，把构建脚本生成的绑定复制过去。要 libclang，`FFMPEG_DIR` 取 .cargo/config.toml 里的
third_party/ffmpeg/dist（先 make ffmpeg）。

提交的绑定以 CI 在各目标的固定环境里生成的为准（.github/actions/ffmpeg-bindings）：不同的 libclang 生成的文件可能有
细微差别（如指针的 const），本地运行只用于调试。CI 用 `--check` 运行：生成后与提交的比较，没有提交过或不一致时失败，
并在 GITHUB_OUTPUT 里给出 `target` 与 `upload=true`，由 CI 把新生成的文件作为附件上传，照它提交。
"""

import argparse
import difflib
import json
import os
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
    """生成并复制到 crates/ffmpeg-sys/src/bindings 下；`--check` 时没有提交过或不一致返回 1。"""
    parser = argparse.ArgumentParser(description="生成本机目标的 hs-m3u8-ffmpeg-sys 绑定")
    parser.add_argument("--check", action="store_true", help="与提交的比较（CI 用）")
    check = parser.parse_args().check
    target = host()
    path = BINDINGS / f"{target}.rs"
    committed = path.read_text(encoding="utf-8") if path.is_file() else None
    shutil.copyfile(generated(), path)
    name = path.relative_to(ROOT).as_posix()
    sys.stdout.write(f"已生成 {name}\n")
    if not check:
        return 0
    fresh = path.read_text(encoding="utf-8")
    if committed == fresh:
        return 0
    if committed is None:
        sys.stderr.write(f"没有提交 {name}\n")
    else:
        diff = difflib.unified_diff(
            committed.splitlines(keepends=True), fresh.splitlines(keepends=True), f"a/{name}", f"b/{name}"
        )
        sys.stderr.writelines(diff)
        sys.stderr.write(f"{name} 与新生成的不一致\n")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with Path(output).open("a", encoding="utf-8") as file:
            file.write(f"target={target}\nupload=true\n")
    return 1


if __name__ == "__main__":
    sys.exit(main())
