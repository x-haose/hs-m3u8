"""生成 LICENSES 下随发布包分发的声明：NOTICE.txt 与 THIRD-PARTY-RUST.txt。`--check` 时只核对已有的是否最新。

THIRD-PARTY-RUST.txt 收录 Python 扩展模块在各发布平台（deny.toml 的 `[graph] targets`）上链接的 Rust crate：从
`hs-m3u8-py` 沿正常依赖走到的、工作区之外的 crate，过程宏不进产物、不算。各 crate 根目录里的许可证文件原样收录，内容
相同的只收一份；包里没有许可证文件的列出许可证表达式与源码地址。NOTICE.txt 里 FFmpeg 的版本与源码包的 SHA-256 取自
third_party/ffmpeg/build.sh。
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "LICENSES"
ROOT_CRATE = "hs-m3u8-py"
# crate 根目录里算作许可证文件的名字（不区分大小写）
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)", re.IGNORECASE)
RULE = "-" * 80


@dataclass(frozen=True)
class Crate:
    """链接进扩展模块的一个 crate。"""

    name: str
    version: str
    license: str
    repository: str
    directory: Path

    def label(self) -> str:
        """名字与版本。"""
        return f"{self.name} {self.version}"


def release_targets() -> list[str]:
    """deny.toml 里的发布平台。"""
    config = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    targets: list[str] = config["graph"]["targets"]
    return targets


def linked_crates(target: str) -> list[Crate]:
    """`target` 上扩展模块链接的工作区之外的 crate。"""
    output = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    metadata = json.loads(output)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    root = next(id_ for id_ in members if packages[id_]["name"] == ROOT_CRATE)

    def proc_macro(id_: str) -> bool:
        return any("proc-macro" in target["kind"] for target in packages[id_]["targets"])

    seen: set[str] = set()
    pending = [root]
    while pending:
        id_ = pending.pop()
        if id_ in seen or proc_macro(id_):
            continue
        seen.add(id_)
        pending.extend(
            dep["pkg"] for dep in nodes[id_]["deps"] if any(kind["kind"] is None for kind in dep["dep_kinds"])
        )
    return [
        Crate(
            name=packages[id_]["name"],
            version=packages[id_]["version"],
            license=packages[id_]["license"] or "（未注明）",
            repository=packages[id_]["repository"] or "（未注明）",
            directory=Path(packages[id_]["manifest_path"]).parent,
        )
        for id_ in seen
        if id_ not in members
    ]


def license_files(crate: Crate) -> list[Path]:
    """`crate` 根目录里的许可证文件，按名字排序。"""
    return sorted(path for path in crate.directory.iterdir() if path.is_file() and LICENSE_FILE.match(path.name))


def third_party(crates: list[Crate]) -> str:
    """THIRD-PARTY-RUST.txt 的内容。"""
    lines = [
        "hs-m3u8 的 Python 扩展模块在各发布平台上静态链接的 Rust crate（合计），及其许可证文件的原文。",
        "由 scripts/licenses.py 生成。",
        "",
        "== 清单 ==",
        "",
    ]
    lines += [f"{crate.label()}  {crate.license}  {crate.repository}" for crate in crates]
    without = [crate for crate in crates if not license_files(crate)]
    if without:
        lines += ["", "== 包里没有附许可证文件的 crate（许可证见源码地址） ==", ""]
        lines += [f"{crate.label()}  {crate.license}  {crate.repository}" for crate in without]
    # 内容相同的文件只收一份：按内容归组，组的顺序按第一个用到它的 crate
    groups: dict[str, tuple[str, list[str]]] = {}
    for crate in crates:
        for path in license_files(crate):
            text = path.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n").rstrip()
            digest = hashlib.sha256(text.encode()).hexdigest()
            groups.setdefault(digest, (text, []))[1].append(f"{crate.label()}：{path.name}")
    lines += ["", "== 许可证文件 =="]
    for text, users in groups.values():
        lines += ["", RULE, *users, RULE, "", text]
    return "\n".join(lines) + "\n"


def notice() -> str:
    """NOTICE.txt 的内容。"""
    script = (ROOT / "third_party/ffmpeg/build.sh").read_text(encoding="utf-8")
    version = re.search(r"^VERSION=(\S+)$", script, re.MULTILINE)
    sha256 = re.search(r"^SHA256=(\S+)$", script, re.MULTILINE)
    if version is None or sha256 is None:
        message = "third_party/ffmpeg/build.sh 里找不到 VERSION= 或 SHA256="
        raise SystemExit(message)
    tarball = f"ffmpeg-{version[1]}.tar.xz"
    return f"""hs-m3u8 按 MIT 许可证发布，见 LICENSE。由 scripts/licenses.py 生成。

二进制分发包（Python wheel）里的扩展模块静态链接了以下软件：

1. FFmpeg {version[1]}（https://ffmpeg.org），按 GNU Lesser General Public License（LGPL）2.1 或更高版本授权，全文见
   FFmpeg-LGPL-2.1.txt。FFmpeg 的源码未作修改，只编译了转封装用到的组件，configure 参数见本项目源码里的
   third_party/ffmpeg/build.sh。对应的源码包 {tarball} 随本项目的源码分发包（sdist）一同发布，也可从
   https://ffmpeg.org/releases/{tarball} 取得，SHA-256 为
   {sha256[1]}。

   换用修改过的 FFmpeg：在本项目源码目录里按 build.sh 的参数编译安装 FFmpeg（运行 bash third_party/ffmpeg/build.sh
   <安装目录>，或照它的参数自行编译），再以 FFMPEG_DIR=<安装目录> maturin build --release 重新构建扩展模块。

2. Rust crate，各自的许可证见 THIRD-PARTY-RUST.txt；Rust 标准库按 MIT 或 Apache-2.0 授权
   （https://github.com/rust-lang/rust）。
"""


def main() -> int:
    """生成或核对声明；`--check` 时不是最新的列到 stderr 并返回 1。"""
    parser = argparse.ArgumentParser(description="生成或核对 LICENSES 下随发布包分发的声明")
    parser.add_argument("--check", action="store_true", help="只核对，不写文件")
    args = parser.parse_args()
    found: dict[tuple[str, str], Crate] = {}
    for target in release_targets():
        for crate in linked_crates(target):
            found[(crate.name, crate.version)] = crate
    crates = sorted(found.values(), key=lambda crate: (crate.name, crate.version))
    expected = {"NOTICE.txt": notice(), "THIRD-PARTY-RUST.txt": third_party(crates)}
    stale: list[str] = []
    for name, content in expected.items():
        path = OUT / name
        current = path.read_text(encoding="utf-8") if path.exists() else None
        if current == content:
            continue
        if args.check:
            stale.append(name)
        else:
            path.write_text(content, encoding="utf-8")
    for name in stale:
        sys.stderr.write(f"LICENSES/{name} 不是最新的，运行 make licenses 重新生成\n")
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
