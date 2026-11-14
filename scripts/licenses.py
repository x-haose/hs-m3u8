"""生成随发布包分发的声明与 pyproject.toml 的许可证表达式；`--check` 时只核对是否最新。

声明为 LICENSES 下的 NOTICE.txt 与 THIRD-PARTY-RUST.txt，许可证表达式为 pyproject.toml 的 `license`。

THIRD-PARTY-RUST.txt 收录 Python 扩展模块在各发布平台（deny.toml 的 `[graph] targets`）上依赖的 Rust crate：按
`cargo tree` 解析特性后 `hs-m3u8-py` 的正常依赖，过程宏与工作区成员不算。各 crate 根目录里的许可证文件原样收录，内容
相同的只收一份。收录的文件要对得上 crate 声明的许可证表达式：每个子句（可任选其一的几种许可证）至少有一种的原文在
里面（按 LICENSE_MARKS 里各许可证条款的特征认）；包里的对不上时，加上 third_party/crate-licenses/<名字>-<版本>/ 下的
再认，仍对不上即失败。补的文件在 THIRD-PARTY-RUST.txt 里标出。NOTICE.txt 里 FFmpeg 的版本与源码包的 SHA-256 取自
third_party/ffmpeg/build.sh。许可证表达式为本项目（MIT）、FFmpeg（LGPL）与各 crate 的许可证表达式之积，化成合取范式后
按吸收律化简（`A AND (A OR B)` 即 `A`）。
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
from typing import NamedTuple, NoReturn

ROOT = Path(__file__).resolve().parent.parent
SUPPLIED = ROOT / "third_party/crate-licenses"
OUT = ROOT / "LICENSES"
ROOT_CRATE = "hs-m3u8-py"
# crate 根目录里算作许可证文件的名字（不区分大小写）
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)", re.IGNORECASE)
# SPDX 的许可证标识符（含 LicenseRef-…）
SPDX_ID = re.compile(r"^[A-Za-z0-9.+-]+$")
# `cargo tree --format {p}` 的一行：名字、版本，路径依赖另带路径，已列过的带 (*)
TREE_LINE = re.compile(r"^(\S+) v(\S+)(?: \([^)]*\))?(?: \(\*\))?$")
RULE = "-" * 80
# 本项目与静态链接的 FFmpeg 的许可证
OWN_LICENSES = ["MIT", "LGPL-2.1-or-later"]
PYPROJECT_LICENSE = re.compile(r'^license = "[^"]*"$', re.MULTILINE)
# 许可证文件一行开头的注释记号
COMMENT_LEAD = re.compile(r"^\s*(?://+|#+|\*+)")


class Marks(NamedTuple):
    """一种许可证原文的特征：小写、空白合成一个空格后，`present` 里的句子都要出现，`absent` 里的都不能出现。"""

    present: tuple[str, ...]
    absent: tuple[str, ...] = ()


# 各许可证原文的特征，取自它的条款（不是标题，只给链接或名字的文件对不上）；相近的许可证靠 `absent` 分开：MIT 与
# MIT-0、ISC 与 0BSD、BSD-2-Clause 与 BSD-3-Clause、GPL 与 LGPL。依赖用到新的许可证时加上，并在
# tests/scripts/test_licenses.py 里加正例与反例
LICENSE_MARKS = {
    "0BSD": Marks(
        ("permission to use, copy, modify, and/or distribute this software for any purpose with or without fee",),
        ("provided that the above copyright notice",),
    ),
    "Apache-2.0": Marks(
        ("apache license", "version 2.0", "terms and conditions for use, reproduction, and distribution")
    ),
    "BSD-2-Clause": Marks(
        ("redistribution and use in source and binary forms", "redistributions of source code must retain"),
        ("neither the name",),
    ),
    "BSD-3-Clause": Marks(
        (
            "redistribution and use in source and binary forms",
            "redistributions of source code must retain",
            "neither the name",
        )
    ),
    "BSL-1.0": Marks(("boost software license", "to any person or organization obtaining a copy of the software")),
    "CC0-1.0": Marks(("cc0 1.0 universal", "statement of purpose")),
    "GPL-2.0-only": Marks(
        ("gnu general public license", "version 2, june 1991"),
        ("lesser general public license", "library general public license"),
    ),
    "ISC": Marks(
        (
            "permission to use, copy, modify, and/or distribute this software for any purpose with or without fee",
            "provided that the above copyright notice and this permission notice appear in all copies",
        )
    ),
    "LGPL-2.1-or-later": Marks(("gnu lesser general public license", "version 2.1, february 1999")),
    "MIT": Marks(
        (
            "permission is hereby granted, free of charge, to any person obtaining a copy of this software",
            "the above copyright notice and this permission notice shall be included",
        )
    ),
    "MIT-0": Marks(
        ("permission is hereby granted, free of charge, to any person obtaining a copy of this software",),
        ("the above copyright notice and this permission notice shall be included",),
    ),
    "MPL-2.0": Marks(("mozilla public license", "covered software", "larger work")),
    "Unicode-3.0": Marks(
        (
            "unicode license v3",
            "permission is hereby granted, free of charge, to any person obtaining a copy of data files",
        )
    ),
    "Unicode-DFS-2016": Marks(
        (
            "unicode, inc. license agreement - data files and software",
            "permission is hereby granted, free of charge, to any person obtaining a copy of the unicode data files",
        )
    ),
    "Unlicense": Marks(("this is free and unencumbered software released into the public domain",)),
    "WTFPL": Marks(("do what the fuck you want to public license", "you just do what the fuck you want to")),
    "Zlib": Marks(
        (
            "this software is provided 'as-is', without any express or implied warranty",
            "altered source versions must be plainly marked",
        )
    ),
}


@dataclass(frozen=True)
class Crate:
    """产物依赖的一个 crate。"""

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


def dependencies(root: str, target: str) -> set[tuple[str, str]]:
    """`target` 上 `root` 的依赖（含工作区成员）的名字与版本。"""
    output = cargo(
        "tree",
        "--locked",
        *("-p", root, "-e", "normal,no-proc-macro", "--target", target),
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
    """`names` 里工作区之外的 crate，按名字与版本排序；有 crate 没写 `license`（只写了 `license-file`）时失败。"""
    metadata = json.loads(cargo("metadata", "--format-version", "1", "--locked"))
    members = set(metadata["workspace_members"])
    found: list[Crate] = []
    for package in metadata["packages"]:
        if (package["name"], package["version"]) not in names or package["id"] in members:
            continue
        if not package["license"]:
            message = f"{package['name']} {package['version']} 没写 license（SPDX 表达式），要人工核对它的许可证"
            raise SystemExit(message)
        found.append(
            Crate(
                name=package["name"],
                version=package["version"],
                license=package["license"],
                repository=package["repository"] or "（未注明）",
                directory=Path(package["manifest_path"]).parent,
            )
        )
    return sorted(found, key=lambda crate: (crate.name, crate.version))


def is_text_of(license_id: str, text: str) -> bool:
    """`text` 是不是 `license_id` 的原文。

    按 LICENSE_MARKS 认，`WITH` 例外不另认，行首的注释记号不算；没有这种许可证的特征时为 False。
    """
    marks = LICENSE_MARKS.get(license_id.split(" WITH ", maxsplit=1)[0])
    if marks is None:
        return False
    # 写成源码注释的许可证（每行以 //、#、* 起头）去掉行首的注释记号再比
    flat = " ".join(" ".join(COMMENT_LEAD.sub("", line) for line in text.lower().splitlines()).split())
    return all(phrase in flat for phrase in marks.present) and not any(phrase in flat for phrase in marks.absent)


def uncovered(crate: Crate, files: list[Path]) -> list[frozenset[str]]:
    """`crate` 的许可证表达式里，`files` 里没有任一种许可证原文的子句（见 `is_text_of`）。"""
    texts = [read_text(path) for path in files]
    return [
        clause
        for clause in Clauses(crate.license).result
        if not any(is_text_of(license_id, text) for license_id in clause for text in texts)
    ]


def license_files(crate: Crate) -> list[Path]:
    """`crate` 的许可证文件，包里的在前、各按名字排序。

    包里的对得上它声明的许可证（见 `uncovered`）时只取包里的，否则加上 third_party/crate-licenses 下的；仍对不上时失败。
    """

    def found(directory: Path) -> list[Path]:
        return sorted(path for path in directory.iterdir() if path.is_file() and LICENSE_FILE.match(path.name))

    files = found(crate.directory)
    if not uncovered(crate, files):
        return files
    supplied = SUPPLIED / f"{crate.name}-{crate.version}"
    if supplied.is_dir():
        files += found(supplied)
    missing = uncovered(crate, files)
    if not missing:
        return files
    unknown = sorted(
        {lid.split(" WITH ", maxsplit=1)[0] for clause in missing for lid in clause} - LICENSE_MARKS.keys()
    )
    names = "；".join(" 或 ".join(sorted(clause)) for clause in missing)
    hint = f"（LICENSE_MARKS 里还没有 {'、'.join(unknown)} 的特征）" if unknown else ""
    message = (
        f"{crate.label()} 的许可证文件里没有 {names} 的原文{hint}；从它发布时的提交（包里 .cargo_vcs_info.json "
        f"记的）取许可证文件放到 {supplied.relative_to(ROOT)}，源码里也没有时取 SPDX 的标准文本，出处写进那里的 README"
    )
    raise SystemExit(message)


def read_text(path: Path) -> str:
    """许可证文件的原文；不是 UTF-8 时失败，不替换字符。"""
    try:
        return path.read_bytes().decode("utf-8")
    except UnicodeDecodeError as error:
        message = f"{path} 不是 UTF-8 文本：{error}"
        raise SystemExit(message) from error


def third_party(subject: str, crates: list[Crate]) -> str:
    """THIRD-PARTY-RUST.txt 的内容；`subject` 为产物的叫法。"""
    lines = [
        f"{subject}在各发布平台上依赖的 Rust crate（合计），及其许可证文件（包里没有原文的由本项目补，已标出）。",
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
            supplied = (
                f"（本项目补的，出处见源码的 {SUPPLIED.relative_to(ROOT)}/README.md）"
                if path.is_relative_to(SUPPLIED)
                else ""
            )
            groups.setdefault(digest, (text, []))[1].append(f"{crate.label()}：{path.name}{supplied}")
    lines += ["", "== 许可证文件 =="]
    for text, users in groups.values():
        lines += ["", RULE, *users, RULE, "", text]
    return "\n".join(lines) + "\n"


class Clauses:
    """许可证表达式化成合取范式的各子句，每个子句为可任选其一的许可证；旧写法里的 `/` 即 OR。

    只认 OR 都在 AND 之内的写法（如 `(A OR B) AND C`），按 SPDX 的优先级 AND 先于 OR；别的写法失败。
    """

    def __init__(self, expression: str) -> None:
        """解析 `expression`；认不出时失败。"""
        self.expression = expression
        self.words: list[str] = re.findall(r"\(|\)|[^\s()]+", expression.replace("/", " OR "))
        self.position = 0
        self.result = self.either()
        if self.position != len(self.words):
            self.fail()

    def fail(self) -> NoReturn:
        """认不出这个表达式。"""
        message = f"认不出的许可证表达式：{self.expression}"
        raise SystemExit(message)

    def peek(self) -> str | None:
        """下一个词；读完了为 None。"""
        return self.words[self.position] if self.position < len(self.words) else None

    def take(self) -> str:
        """取下一个词；读完了即失败。"""
        if self.position >= len(self.words):
            self.fail()
        self.position += 1
        return self.words[self.position - 1]

    def either(self) -> set[frozenset[str]]:
        """以 OR 连接的部分；两边都须是单个子句。"""
        left = self.both()
        while self.peek() == "OR":
            self.take()
            right = self.both()
            if len(left) != 1 or len(right) != 1:
                self.fail()
            left = {next(iter(left)) | next(iter(right))}
        return left

    def both(self) -> set[frozenset[str]]:
        """以 AND 连接的部分。"""
        left = self.single()
        while self.peek() == "AND":
            self.take()
            left |= self.single()
        return left

    def single(self) -> set[frozenset[str]]:
        """一个许可证（可带 WITH 例外），或括号里的表达式。"""
        word = self.take()
        if word == "(":
            inner = self.either()
            if self.take() != ")":
                self.fail()
            return inner
        if word in {")", "AND", "OR", "WITH"} or not SPDX_ID.match(word):
            self.fail()
        if self.peek() == "WITH":
            self.take()
            exception = self.take()
            if not SPDX_ID.match(exception):
                self.fail()
            word = f"{word} WITH {exception}"
        return {frozenset([word])}


def license_expression(crates: list[Crate]) -> str:
    """pyproject.toml 的许可证表达式：本项目、FFmpeg 与各 crate 的许可证之积，按吸收律化简。"""
    found = {frozenset([own]) for own in OWN_LICENSES}
    for crate in crates:
        found |= Clauses(crate.license).result
    kept = [clause for clause in found if not any(other < clause for other in found)]
    order = {own: index for index, own in enumerate(OWN_LICENSES)}

    def text(clause: frozenset[str]) -> str:
        names = sorted(clause)
        return names[0] if len(names) == 1 else "(" + " OR ".join(names) + ")"

    kept.sort(key=lambda clause: (len(clause), order.get(text(clause), len(order)), text(clause)))
    return " AND ".join(text(clause) for clause in kept)


def ffmpeg_release() -> tuple[str, str]:
    """静态链接的 FFmpeg 的版本与源码包的 SHA-256，取自 third_party/ffmpeg/build.sh。"""
    script = (ROOT / "third_party/ffmpeg/build.sh").read_text(encoding="utf-8")
    version = re.search(r"^VERSION=(\S+)$", script, re.MULTILINE)
    sha256 = re.search(r"^SHA256=(\S+)$", script, re.MULTILINE)
    if version is None or sha256 is None:
        message = "third_party/ffmpeg/build.sh 里找不到 VERSION= 或 SHA256="
        raise SystemExit(message)
    return version[1], sha256[1]


def notice() -> str:
    """NOTICE.txt 的内容。"""
    version, sha256 = ffmpeg_release()
    tarball = f"ffmpeg-{version}.tar.xz"
    return f"""hs-m3u8 按 MIT 许可证发布，见 LICENSE。由 scripts/licenses.py 生成。

二进制分发包（Python wheel）里的扩展模块静态链接了以下软件：

1. FFmpeg {version}（https://ffmpeg.org），按 GNU Lesser General Public License（LGPL）2.1 或更高版本授权，全文见
   FFmpeg-LGPL-2.1.txt。FFmpeg 的源码未作修改，只编译了转封装用到的组件，configure 参数见本项目源码里的
   third_party/ffmpeg/build.sh。对应的源码包 {tarball} 随本项目的源码分发包（sdist）一同发布，也可从
   https://ffmpeg.org/releases/{tarball} 取得，SHA-256 为
   {sha256}。

   换用修改过的 FFmpeg：按 build.sh 里的 configure 参数编译修改过的 FFmpeg 源码并安装到某个目录，再在本项目源码目录
   里以 FFMPEG_DIR=<该目录> maturin build --release 重新构建扩展模块。

2. Rust crate，各自的许可证见 THIRD-PARTY-RUST.txt；Rust 标准库按 MIT 或 Apache-2.0 授权
   （https://github.com/rust-lang/rust）。
"""


def found_crates() -> list[Crate]:
    """扩展模块在各发布平台上依赖的 crate（合计）。"""
    names: set[tuple[str, str]] = set()
    for target in release_targets():
        names |= dependencies(ROOT_CRATE, target)
    return crates(names)


def main() -> int:
    """生成或核对声明与许可证表达式；`--check` 时不是最新的列到 stderr 并返回 1。"""
    parser = argparse.ArgumentParser(description="生成或核对随 Python wheel 分发的声明与 pyproject.toml 的许可证表达式")
    parser.add_argument("--check", action="store_true", help="只核对，不写文件")
    args = parser.parse_args()
    found = found_crates()
    pyproject = ROOT / "pyproject.toml"
    project = pyproject.read_text(encoding="utf-8")
    if len(PYPROJECT_LICENSE.findall(project)) != 1:
        message = 'pyproject.toml 里要有恰好一行 license = "..."'
        raise SystemExit(message)
    expected = {
        OUT / "NOTICE.txt": notice(),
        OUT / "THIRD-PARTY-RUST.txt": third_party("hs-m3u8 的 Python 扩展模块", found),
        pyproject: PYPROJECT_LICENSE.sub(f'license = "{license_expression(found)}"', project),
    }
    stale: list[Path] = []
    for path, content in expected.items():
        # 按字节比对与写出，换行不做转换
        current = path.read_bytes().decode("utf-8") if path.exists() else None
        if current == content:
            continue
        if args.check:
            stale.append(path)
        else:
            path.write_bytes(content.encode("utf-8"))
    for path in stale:
        sys.stderr.write(f"{path.relative_to(ROOT)} 不是最新的，运行 make licenses 重新生成\n")
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
