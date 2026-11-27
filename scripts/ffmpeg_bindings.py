"""生成 hs-m3u8-ffmpeg-sys 在本机目标上的绑定，写到 crates/ffmpeg-sys/src/bindings/<目标>.rs。

开 `bindgen` 特性构建这个 crate，取构建脚本生成的绑定。要 libclang，`FFMPEG_DIR` 取 .cargo/config.toml 里的
third_party/ffmpeg/dist（先 make ffmpeg）。

仓库里的绑定以 CI 在各目标上生成的为准（.github/actions/ffmpeg-bindings）：不同的 libclang 生成的文件可能有细微差别
（如指针的 const），本地运行只用于调试。生成用的 libclang 的版本打印在输出里。CI 用 `--check` 运行：生成到
target/ffmpeg-bindings/<目标>.rs，不改仓库里的文件，与仓库里的比较；仓库里没有或不一致时失败，并在 GITHUB_OUTPUT 里
给出 `target` 与 `path`（新生成的文件），由 CI 作为附件上传，照它提交。
"""

import argparse
import difflib
import io
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = "hs-m3u8-ffmpeg-sys"
BINDINGS = ROOT / "crates/ffmpeg-sys/src/bindings"
CHECK_DIR = ROOT / "target/ffmpeg-bindings"


def host() -> str:
    """本机的目标三元组（rustc 的 host）。"""
    output = subprocess.run(["rustc", "-vV"], cwd=ROOT, stdout=subprocess.PIPE, encoding="utf-8", check=True).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    message = f"rustc -vV 的输出里没有 host：{output!r}"
    raise SystemExit(message)


def generated() -> Path:
    """开 bindgen 特性构建，返回构建脚本的输出目录（含 bindings.rs 与 libclang.txt）。

    构建失败时 cargo 的报错照常输出到 stderr。cargo 的 JSON 输出是 UTF-8（路径里的非 ASCII 字符不转义）。
    """
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
    output = subprocess.run(build, cwd=ROOT, stdout=subprocess.PIPE, encoding="utf-8", check=True).stdout
    for line in output.splitlines():
        event = json.loads(line)
        if event["reason"] == "build-script-executed" and f"#{PACKAGE}@" in event["package_id"]:
            return Path(event["out_dir"])
    message = f"cargo 的输出里没有 {PACKAGE} 构建脚本的运行结果"
    raise SystemExit(message)


def main() -> int:
    """生成并复制到仓库里；`--check` 时只生成到 target/ffmpeg-bindings 下，仓库里没有或不一致返回 1。"""
    # Windows 上输出接到管道时默认用系统区域编码（如 cp1252），写不出中文和绑定里的字符
    for stream in (sys.stdout, sys.stderr):
        if not isinstance(stream, io.TextIOWrapper):
            message = f"标准输出或标准错误被换成了 {type(stream).__name__}，改不了编码"
            raise SystemExit(message)
        stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description="生成本机目标的 hs-m3u8-ffmpeg-sys 绑定")
    parser.add_argument(
        "--check", action="store_true", help="不改仓库里的文件，生成到 target/ffmpeg-bindings 下与之比较（CI 用）"
    )
    check = parser.parse_args().check
    target = host()
    repo = BINDINGS / f"{target}.rs"
    out_dir = generated()
    libclang = (out_dir / "libclang.txt").read_text(encoding="utf-8")
    path = CHECK_DIR / f"{target}.rs" if check else repo
    path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(out_dir / "bindings.rs", path)
    name = repo.relative_to(ROOT).as_posix()
    sys.stdout.write(f"已生成 {path.relative_to(ROOT).as_posix()}（{libclang}）\n")
    if not check:
        return 0
    fresh = path.read_text(encoding="utf-8")
    committed = repo.read_text(encoding="utf-8") if repo.is_file() else None
    if committed == fresh:
        return 0
    if committed is None:
        sys.stderr.write(f"仓库里没有 {name}\n")
    else:
        diff = difflib.unified_diff(
            committed.splitlines(keepends=True), fresh.splitlines(keepends=True), f"a/{name}", f"b/{name}"
        )
        sys.stderr.writelines(diff)
        sys.stderr.write(f"{name} 与新生成的不一致\n")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with Path(output).open("a", encoding="utf-8") as file:
            file.write(f"target={target}\npath={path}\n")
    return 1


if __name__ == "__main__":
    sys.exit(main())
