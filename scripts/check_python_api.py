"""Python 包对外接口的约定，不满足时列出并以 1 退出。

- 说明齐全：导出的每个函数与类、类的属性与方法、带数据的枚举的每种取值，以及存根里各类的构造函数（它们的说明只在
  存根里，运行时看不到；枚举取值的构造函数由存根生成器按字段生成，参数即取值类的属性，不查）。
- 公开方法的说明在存根与运行时一致：存根手写的方法（存根生成认不出它的参数）说明在两处各写一份。
- 两个以上字段的枚举取值只收关键字参数：同类型的字段（如几个下标）按位置写反时不报错。
- 说明里写的默认值与运行时的一致：默认值由 core 定，Python 的说明里另写了一遍，core 改了默认值时这里报出。
"""

import ast
import inspect
import sys
from datetime import timedelta
from pathlib import Path

import hs_m3u8
from hs_m3u8 import JobRequest, LiveOptions, OutputOptions, RetryPolicy, Source, Target, Timeouts

STUB = Path(hs_m3u8.__file__).parent / "_native" / "__init__.pyi"


def undocumented(owner: object, path: str) -> list[str]:
    """`owner` 与它的公开成员里没有说明的，按 `path.成员` 列出；嵌套的类（枚举的取值）一并检查。"""
    missing = [] if (owner.__doc__ or "").strip() else [path]
    if not isinstance(owner, type):
        return missing
    for name, member in vars(owner).items():
        if name.startswith("_"):
            continue
        if isinstance(member, type):
            missing += undocumented(member, f"{path}.{name}")
        elif not (getattr(member, "__doc__", None) or "").strip():
            missing.append(f"{path}.{name}")
    return missing


def undocumented_constructors(stub: ast.Module) -> list[str]:
    """存根里各类没有说明的 `__new__` 与 `__init__`。"""
    return [
        f"{cls.name}.{method.name}"
        for cls in stub.body
        if isinstance(cls, ast.ClassDef)
        for method in cls.body
        if isinstance(method, ast.FunctionDef)
        and method.name in {"__new__", "__init__"}
        and ast.get_docstring(method) is None
    ]


def mismatched_docs(stub: ast.Module, exported: list[tuple[str, object]]) -> list[str]:
    """存根与运行时的说明不同的公开方法，按 `类.方法` 列出。"""
    classes = dict(exported)
    found: list[str] = []
    for cls in stub.body:
        owner = classes.get(cls.name) if isinstance(cls, ast.ClassDef) else None
        if not isinstance(cls, ast.ClassDef) or not isinstance(owner, type):
            continue
        for method in cls.body:
            if not isinstance(method, ast.FunctionDef | ast.AsyncFunctionDef) or method.name.startswith("_"):
                continue
            if ast.get_docstring(method) != inspect.getdoc(vars(owner).get(method.name)):
                found.append(f"{cls.name}.{method.name}")
    return found


def stale_defaults() -> list[str]:
    """说明里写的默认值与运行时的默认值不符的，按 `类.属性` 列出。"""

    def seconds(value: timedelta) -> str:
        return f"{value.total_seconds():g} 秒"

    timeouts, retry, live = Timeouts(), RetryPolicy(), LiveOptions()
    request = JobRequest(Source("https://example.com/a.m3u8"), OutputOptions(Target.Mp4("a.mp4")))
    expected = [
        ("Timeouts.connect", Timeouts.connect, f"默认 {seconds(timeouts.connect)}"),
        ("Timeouts.read_idle", Timeouts.read_idle, f"默认 {seconds(timeouts.read_idle)}"),
        ("Timeouts.request", Timeouts.request, f"默认 {seconds(timeouts.request)}"),
        (
            "RetryPolicy",
            RetryPolicy,
            f"默认 {retry.attempts} 次、{seconds(retry.base_delay)}起、上限 {seconds(retry.max_delay)}",
        ),
        ("LiveOptions.stall_timeout", LiveOptions.stall_timeout, f"默认 {seconds(live.stall_timeout)}"),
        ("JobRequest.concurrency", JobRequest.concurrency, f"默认 {request.concurrency}"),
    ]
    return [name for name, owner, text in expected if text not in (inspect.getdoc(owner) or "")]


def positional_variants(owner: type, path: str) -> list[str]:
    """`owner` 里两个以上参数能按位置传的枚举取值，按 `path.取值` 列出。"""
    found: list[str] = []
    for name, member in vars(owner).items():
        if name.startswith("_") or not (isinstance(member, type) and issubclass(member, owner)):
            continue
        positional = [
            parameter
            for parameter in inspect.signature(member).parameters.values()
            if parameter.kind is inspect.Parameter.POSITIONAL_OR_KEYWORD
        ]
        if len(positional) > 1:
            found.append(f"{path}.{name}")
    return found


def main() -> int:
    """检查各项约定，把不满足的写到 stderr；都满足时返回 0。"""
    exported = [(name, getattr(hs_m3u8, name)) for name in hs_m3u8.__all__]
    problems = [f"没有说明：{item}" for name, owner in exported for item in undocumented(owner, name)]
    stub = ast.parse(STUB.read_text(encoding="utf-8"))
    problems += [f"存根里的构造函数没有说明：{item}" for item in undocumented_constructors(stub)]
    problems += [f"存根与运行时的说明不同：{item}" for item in mismatched_docs(stub, exported)]
    problems += [f"说明里的默认值与运行时的不符：{item}" for item in stale_defaults()]
    problems += [
        f"两个以上字段能按位置传：{item}"
        for name, owner in exported
        if isinstance(owner, type)
        for item in positional_variants(owner, name)
    ]
    for problem in problems:
        sys.stderr.write(problem + "\n")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
