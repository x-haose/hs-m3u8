"""对外契约与最近一次发布比较，有不兼容的改动时列出并以 1 退出。

- 任务目录格式：发布时已有的样本（tests/fixtures/workdir 下 `job/<版本>/<名字>`、`outputs/<版本>/<平台>`）不得改动、
  删除或往里添加文件；新的样本只能整个加，新版本的库须读得了全部样本（由 core 的测试核对）。
- Python 接口：griffe 找出删掉或改了的公开类、函数、参数（种类、默认值、是否必填），另外比较它不比的：基类（只能
  加）、参数与返回值和属性的类型注解（按顶层 `|` 拆开，只能加，即可以新增取值）、`__match_args__`（只能在末尾加）。
  主版本比发布时新则只列出、不算失败。

发布标签写作 `v<主>.<次>.<修订>`，取当前提交上溯可达、且不指向当前提交的最新一个（给要发布的提交打了标签后，仍与上一
次发布比较）；还没有发布过时没有可比较的。要完整的提交历史与标签（CI 的 checkout 设 fetch-depth: 0）：浅克隆里分不清
没发布过与没取到标签，一个标签也没有（克隆时没取标签）同样分不清，都直接失败。
"""

import ast
import re
import subprocess
import sys
import tomllib
from collections.abc import Iterator
from pathlib import Path

import griffe

ROOT = Path(__file__).resolve().parent.parent
RELEASE_TAG = re.compile(r"v(\d+)\.\d+\.\d+")
SAMPLES = "tests/fixtures/workdir"
# 样本路径里到「一个样本」为止的段数：tests/fixtures/workdir/<job|outputs>/<版本>/<名字或平台>
SAMPLE_DEPTH = 6
PACKAGE = "hs_m3u8"


def git(*args: str) -> str:
    """在仓库根目录运行 git，返回标准输出；失败时抛出，git 的报错照常输出到 stderr。"""
    return subprocess.run(["git", *args], cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True).stdout


def latest_release() -> tuple[str, int] | None:
    """基线：当前提交上溯可达、不指向当前提交的最新发布标签与它的主版本号；没有时为 None。"""
    tags = git("tag", "--merged", "HEAD", "--no-contains", "HEAD", "--list", "v*", "--sort=-v:refname")
    for tag in tags.split():
        if match := RELEASE_TAG.fullmatch(tag):
            return tag, int(match.group(1))
    return None


def current_major() -> int:
    """工作区的主版本号（Cargo.toml 的 workspace.package.version，Python 包的版本由它而来）。"""
    with (ROOT / "Cargo.toml").open("rb") as file:
        version: str = tomllib.load(file)["workspace"]["package"]["version"]
    return int(version.split(".", 1)[0])


def sample_of(path: str) -> str:
    """样本文件 `path` 所属的样本。"""
    return "/".join(path.split("/")[:SAMPLE_DEPTH])


def changed_samples(tag: str) -> list[str]:
    """工作区里与 `tag` 时相比被改动、删除，或在当时已有的样本里新添的文件，每项为「状态 路径」。"""
    released = {sample_of(path) for path in git("ls-tree", "-r", "--name-only", tag, "--", SAMPLES).split("\n") if path}
    problems: list[str] = []
    for line in git("diff", "--no-renames", "--name-status", tag, "--", SAMPLES).splitlines():
        status, path = line.split("\t", 1)
        if status != "A" or sample_of(path) in released:
            problems.append(f"{status} {path}")
    untracked = git("ls-files", "--others", "--exclude-standard", "--", SAMPLES).splitlines()
    problems += [f"? {path}" for path in untracked if sample_of(path) in released]
    return problems


def resolved(obj: griffe.Object | griffe.Alias) -> griffe.Object:
    """别名指向的对象；不是别名时为它本身。"""
    return obj.final_target if isinstance(obj, griffe.Alias) else obj


def union_members(annotation: object) -> set[str]:
    """类型注解按顶层 `|` 拆成的各项；没有注解时为空集。"""
    if annotation is None:
        return set()
    text, depth, start = str(annotation), 0, 0
    members: set[str] = set()
    for index, char in enumerate(text):
        depth += {"[": 1, "(": 1, "]": -1, ")": -1}.get(char, 0)
        if char == "|" and depth == 0:
            members.add(text[start:index].strip())
            start = index + 1
    members.add(text[start:].strip())
    return members


def narrowed(path: str, what: str, old: object, new: object) -> Iterator[str]:
    """类型注解 `old` 里有 `new` 没有的项时报出。"""
    if missing := union_members(old) - union_members(new):
        yield f"{path}：{what}的类型注解去掉或改了 {' | '.join(sorted(missing))}（现为 {new}）"


def is_public(name: str) -> bool:
    """比较的成员：公开的，与构造函数和 `__match_args__`。"""
    return not name.startswith("_") or name in {"__new__", "__init__", "__match_args__"}


def function_breakages(old: griffe.Function, new: griffe.Function) -> Iterator[str]:
    """函数的返回值与两边都有的参数的类型注解不能去掉或改掉某一项。"""
    yield from narrowed(new.path, "返回值", old.returns, new.returns)
    for parameter in old.parameters:
        if parameter.name in new.parameters:
            current = new.parameters[parameter.name].annotation
            yield from narrowed(new.path, f"参数 {parameter.name}", parameter.annotation, current)


def attribute_breakages(old: griffe.Attribute, new: griffe.Attribute) -> Iterator[str]:
    """属性的类型注解不能去掉或改掉某一项；`__match_args__` 只能在末尾加。"""
    if old.name != "__match_args__":
        yield from narrowed(new.path, "属性", old.annotation, new.annotation)
        return
    before, after = ast.literal_eval(str(old.value)), ast.literal_eval(str(new.value))
    if tuple(after[: len(before)]) != tuple(before):
        yield f"{new.path}：由 {before} 改为 {after}，只能在末尾加"


def beyond_griffe(old: griffe.Object, new: griffe.Object) -> Iterator[str]:
    """Griffe 不比的不兼容改动：两边都有的对象的基类（只能加）、类型注解与 `__match_args__`，逐层比较公开的成员。"""
    if isinstance(old, griffe.Class) and isinstance(new, griffe.Class):
        missing = {str(b) for b in old.bases} - {str(b) for b in new.bases}
        yield from (f"{new.path}：去掉了基类 {base}" for base in sorted(missing))
    elif isinstance(old, griffe.Function) and isinstance(new, griffe.Function):
        yield from function_breakages(old, new)
    elif isinstance(old, griffe.Attribute) and isinstance(new, griffe.Attribute):
        yield from attribute_breakages(old, new)
    for name, member in old.members.items():
        if is_public(name) and name in new.members:
            yield from beyond_griffe(resolved(member), resolved(new.members[name]))


def api_breakages(tag: str) -> list[str]:
    """Python 接口与 `tag` 时相比的不兼容改动。按存根与源码静态分析，不导入扩展模块。"""
    old = griffe.load_git(
        PACKAGE, ref=tag, repo=ROOT, search_paths=["python"], allow_inspection=False, resolve_aliases=True
    )
    new = griffe.load(PACKAGE, search_paths=[ROOT / "python"], allow_inspection=False, resolve_aliases=True)
    if not isinstance(old, griffe.Module) or not isinstance(new, griffe.Module):
        msg = f"{PACKAGE} 应是模块"
        raise TypeError(msg)
    found = [breakage.explain() for breakage in griffe.find_breaking_changes(old, new)]
    for name, member in old.members.items():
        if not name.startswith("_") and name in new.members:
            found += beyond_griffe(resolved(member), resolved(new.members[name]))
    return found


def main() -> int:
    """比较并报告；有不兼容的改动或判断不了时返回 1。"""
    if git("rev-parse", "--is-shallow-repository").strip() == "true":
        sys.stderr.write("浅克隆里没有完整的历史与标签，找不出最近的发布\n")
        return 1
    if not git("tag", "--list").strip():
        sys.stderr.write("仓库里一个标签也没有（克隆时没取标签？），找不出最近的发布\n")
        return 1
    release = latest_release()
    if release is None:
        sys.stdout.write("还没有发布过，没有可比较的\n")
        return 0
    tag, major = release
    failed = False
    if changed := changed_samples(tag):
        sys.stderr.write(f"已发布的任务目录样本与 {tag} 时不同，只能整个加新的样本：\n" + "\n".join(changed) + "\n")
        failed = True
    if breakages := api_breakages(tag):
        listing = "\n".join(breakages) + "\n"
        if current_major() > major:
            sys.stdout.write(f"主版本比 {tag} 新，Python 接口的以下改动不算失败：\n{listing}")
        else:
            sys.stderr.write(f"Python 接口与 {tag} 相比有不兼容的改动，改回或升主版本：\n{listing}")
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
