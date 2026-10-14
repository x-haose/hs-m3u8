"""生成 LICENSES 下随发布包分发的声明：NOTICE.txt 与 THIRD-PARTY-RUST.txt。`--check` 时只核对已有的是否最新。

THIRD-PARTY-RUST.txt 收录 Python 扩展模块在各发布平台（deny.toml 的 `[graph] targets`）上依赖的 Rust crate：按
`cargo tree` 解析特性后 `hs-m3u8-py` 的正常依赖，过程宏与工作区成员不算。各 crate 根目录里的许可证文件原样收录，内容
相同的只收一份；包里没有许可证文件的，取 third_party/crate-licenses/<名字>-<版本>/ 下的，那里也没有即失败。NOTICE.txt
里 FFmpeg 的版本与源码包的 SHA-256 取自 third_party/ffmpeg/build.sh。
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
SUPPLIED = ROOT / "third_party/crate-licenses"
ROOT_CRATE = "hs-m3u8-py"
# crate 根目录里算作许可证文件的名字（不区分大小写）
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)", re.IGNORECASE)
# `cargo tree --format {p}` 的一行：名字、版本，路径依赖另带路径，已列过的带 (*)
TREE_LINE = re.compile(r"^(\S+) v(\S+)(?: \(/[^)]*\))?(?: \(\*\))?$")
RULE = "-" * 80


@dataclass(frozen=True)
class Crate:
    """扩展模块依赖的一个 crate。"""

    name: str
    version: str
    license: str
    repository: str
    directory: Path

    def label(self) -> str:
        """名字与版本。"""
        return f"{self.name} {self.version}"


def cargo(*args: str) -> str:
    """运行 cargo，返回标准输出；cargo 的报错照常输出到 stderr，失败时抛 CalledProcessError。"""
    return subprocess.run(["cargo", *args], cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True).stdout


def release_targets() -> list[str]:
    """deny.toml 里的发布平台；cargo-deny 允许写成目标三元组的字符串或带 `triple` 的表。"""
    config = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    entries: list[str | dict[str, str]] = config["graph"]["targets"]
    return [entry if isinstance(entry, str) else entry["triple"] for entry in entries]


def dependencies(target: str) -> set[tuple[str, str]]:
    """`target` 上扩展模块的依赖（含工作区成员）的名字与版本。"""
    output = cargo(
        "tree",
        "--locked",
        *("-p", ROOT_CRATE, "-e", "normal,no-proc-macro", "--target", target),
        *("--prefix", "none", "--format", "{p}"),
    )
    found: set[tuple[str, str]] = set()
    for line in output.splitlines():
        match = TREE_LINE.match(line)
        if match is None:
            message = f"认不出 cargo tree 的输出：{line!r}"
            raise SystemExit(message)
        found.add((match[1], match[2]))
    return found


def crates(names: set[tuple[str, str]]) -> list[Crate]:
    """`names` 里工作区之外的 crate，按名字与版本排序。"""
    metadata = json.loads(cargo("metadata", "--format-version", "1", "--locked"))
    members = set(metadata["workspace_members"])
    return sorted(
        (
            Crate(
                name=package["name"],
                version=package["version"],
                license=package["license"] or "（未注明）",
                repository=package["repository"] or "（未注明）",
                directory=Path(package["manifest_path"]).parent,
            )
            for package in metadata["packages"]
            if (package["name"], package["version"]) in names and package["id"] not in members
        ),
        key=lambda crate: (crate.name, crate.version),
    )


def license_files(crate: Crate) -> list[Path]:
    """`crate` 的许可证文件，按名字排序：包里有的取包里的，否则取 third_party/crate-licenses 下的。"""

    def found(directory: Path) -> list[Path]:
        return sorted(path for path in directory.iterdir() if path.is_file() and LICENSE_FILE.match(path.name))

    if files := found(crate.directory):
        return files
    supplied = SUPPLIED / f"{crate.name}-{crate.version}"
    if supplied.is_dir() and (files := found(supplied)):
        return files
    message = (
        f"{crate.label()} 的包里没有许可证文件，{supplied.relative_to(ROOT)} 下也没有；"
        "从它发布时的提交（包里 .cargo_vcs_info.json 记的）取许可证文件放到那里"
    )
    raise SystemExit(message)


def read_text(path: Path) -> str:
    """许可证文件的原文；不是 UTF-8 时失败，不替换字符。"""
    try:
        return path.read_bytes().decode("utf-8")
    except UnicodeDecodeError as error:
        message = f"{path} 不是 UTF-8 文本：{error}"
        raise SystemExit(message) from error


def third_party(crates: list[Crate]) -> str:
    """THIRD-PARTY-RUST.txt 的内容。"""
    lines = [
        "hs-m3u8 的 Python 扩展模块在各发布平台上依赖的 Rust crate（合计），及其许可证文件的原文。",
        "由 scripts/licenses.py 生成。",
        "",
        "== 清单 ==",
        "",
    ]
    lines += [f"{crate.label()}  {crate.license}  {crate.repository}" for crate in crates]
    # 内容相同的文件只收一份：按内容归组，组的顺序按第一个用到它的 crate
    groups: dict[str, tuple[str, list[str]]] = {}
    for crate in crates:
        for path in license_files(crate):
            text = read_text(path).replace("\r\n", "\n").rstrip()
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

   换用修改过的 FFmpeg：按 build.sh 里的 configure 参数编译修改过的 FFmpeg 源码并安装到某个目录，再在本项目源码目录
   里以 FFMPEG_DIR=<该目录> maturin build --release 重新构建扩展模块。

2. Rust crate，各自的许可证见 THIRD-PARTY-RUST.txt；Rust 标准库按 MIT 或 Apache-2.0 授权
   （https://github.com/rust-lang/rust）。
"""


def main() -> int:
    """生成或核对声明；`--check` 时不是最新的列到 stderr 并返回 1。"""
    parser = argparse.ArgumentParser(description="生成或核对 LICENSES 下随发布包分发的声明")
    parser.add_argument("--check", action="store_true", help="只核对，不写文件")
    args = parser.parse_args()
    names: set[tuple[str, str]] = set()
    for target in release_targets():
        names |= dependencies(target)
    expected = {"NOTICE.txt": notice(), "THIRD-PARTY-RUST.txt": third_party(crates(names))}
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
