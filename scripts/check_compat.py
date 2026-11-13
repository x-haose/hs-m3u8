"""对外契约与最近一次发布比较，有不兼容的改动时列出并以 1 退出。

- Python 接口：griffe 比较本包现在与发布时的存根和源码（删掉或改了公开的类、函数、参数）。主版本比发布时新则允许
  不兼容的改动，不比较。
- 任务目录格式：已发布的样本（tests/fixtures/workdir）不得改动或删除，只能为新的格式版本添加，新版本的库须读得了它们
  （由 core 的测试核对）。

发布标签写作 `v<主>.<次>.<修订>`，取当前提交上溯可达的最新一个；还没有发布过时没有可比较的。要完整的提交历史与标签
（CI 的 checkout 要设 fetch-depth: 0）：浅克隆里分不清没发布过与没取到标签，直接失败。
"""

import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RELEASE_TAG = re.compile(r"v(\d+)\.\d+\.\d+")
SAMPLES = "tests/fixtures/workdir"


def git(*args: str) -> str:
    """在仓库根目录运行 git，返回标准输出；失败时抛出。"""
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def latest_release() -> tuple[str, int] | None:
    """当前提交上溯可达的最新发布标签与它的主版本号；没有时为 None。"""
    for tag in git("tag", "--merged", "HEAD", "--list", "v*", "--sort=-v:refname").split():
        if match := RELEASE_TAG.fullmatch(tag):
            return tag, int(match.group(1))
    return None


def current_major() -> int:
    """工作区的主版本号（Cargo.toml 的 workspace.package.version，Python 包的版本由它而来）。"""
    with (ROOT / "Cargo.toml").open("rb") as file:
        version: str = tomllib.load(file)["workspace"]["package"]["version"]
    return int(version.split(".", 1)[0])


def main() -> int:
    """比较并报告；有不兼容的改动或判断不了时返回 1。"""
    if git("rev-parse", "--is-shallow-repository").strip() == "true":
        sys.stderr.write("浅克隆里没有完整的历史与标签，找不出最近的发布\n")
        return 1
    release = latest_release()
    if release is None:
        sys.stdout.write("还没有发布过，没有可比较的\n")
        return 0
    tag, major = release
    failed = False
    changed = git("diff", "--no-renames", "--diff-filter=DMT", "--name-status", tag, "--", SAMPLES)
    if changed:
        sys.stderr.write(f"已发布的任务目录样本与 {tag} 时不同，只能为新的格式版本添加样本：\n{changed}")
        failed = True
    if current_major() > major:
        sys.stdout.write(f"主版本比 {tag} 新，允许 Python 接口不兼容的改动，不比较\n")
    else:
        griffe = subprocess.run(
            ["griffe", "check", "hs_m3u8", "--search", "python", "--against", tag], cwd=ROOT, check=False
        )
        if griffe.returncode != 0:
            sys.stderr.write(f"Python 接口与 {tag} 相比有不兼容的改动（见上）：改回，或升主版本\n")
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
