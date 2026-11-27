"""核对预编译 FFmpeg 与 third_party/ffmpeg/build.sh 没有脱节。

third_party/ffmpeg/prebuilt.txt 记着当前预编译库的 Release 标签与编出它的 build.sh（去掉注释行与空行）的 SHA-256。
下面任一不符即失败：build.sh 的内容、build.sh 的 VERSION 与标签里的版本、crates/ffmpeg-sys/build/download.rs 的
VERSION 与 REVISION 与标签。build.sh 改了版本或组件而没发新的一版时，依赖方下载的仍是旧库，这里让 make check 失败。
`--print` 打印当前 build.sh 的这个哈希（发新的一版后写进 prebuilt.txt）。
"""

import argparse
import hashlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BUILD_SH = ROOT / "third_party/ffmpeg/build.sh"
PREBUILT = ROOT / "third_party/ffmpeg/prebuilt.txt"
DOWNLOAD = ROOT / "crates/ffmpeg-sys/build/download.rs"


def script_digest() -> str:
    """build.sh 去掉注释行（含 shebang）与空行后的 SHA-256：只改注释不必发新的一版。"""
    lines = [line for line in BUILD_SH.read_text(encoding="utf-8").splitlines() if line.strip()]
    code = [line for line in lines if not line.lstrip().startswith("#")]
    return hashlib.sha256("".join(f"{line}\n" for line in code).encode()).hexdigest()


def one(pattern: str, text: str, where: Path) -> tuple[str, ...]:
    """`text` 里 `pattern` 唯一一处匹配的各组。"""
    found = list(re.finditer(pattern, text, re.MULTILINE))
    if len(found) != 1:
        message = f"{where.relative_to(ROOT)} 里应有一处匹配 {pattern!r}，找到 {len(found)} 处"
        raise SystemExit(message)
    return found[0].groups()


def main() -> int:
    """核对；`--print` 时只打印 build.sh 的哈希。"""
    parser = argparse.ArgumentParser(description="核对预编译 FFmpeg 与 build.sh 一致")
    parser.add_argument("--print", action="store_true", help="打印当前 build.sh 的哈希")
    if parser.parse_args().print:
        sys.stdout.write(f"{script_digest()}\n")
        return 0
    tag, digest = one(r"^(ffmpeg-\S+) ([0-9a-f]{64})$", PREBUILT.read_text(encoding="utf-8"), PREBUILT)
    download = DOWNLOAD.read_text(encoding="utf-8")
    (version,) = one(r'^const VERSION: &str = "([^"]+)";$', download, DOWNLOAD)
    (revision,) = one(r"^const REVISION: u32 = (\d+);$", download, DOWNLOAD)
    (script_version,) = one(r"^VERSION=(\S+)$", BUILD_SH.read_text(encoding="utf-8"), BUILD_SH)
    problems: list[str] = []
    if tag != f"ffmpeg-{version}-{revision}":
        problems.append(f"download.rs 的版本与修订号是 {version}、{revision}，prebuilt.txt 的标签是 {tag}")
    if script_version != version:
        problems.append(f"build.sh 的 FFmpeg 版本是 {script_version}，预编译库是 {version}")
    if script_digest() != digest:
        problems.append("build.sh 与编出预编译库的那份不同（注释与空行之外）")
    if problems:
        sys.stderr.writelines(f"{problem}\n" for problem in problems)
        sys.stderr.write(
            "改了 build.sh 就要发新一版预编译库：推下一个修订号的 ffmpeg-* 标签，再按 README「开发」一节更新 "
            "download.rs 与 third_party/ffmpeg/prebuilt.txt\n"
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
