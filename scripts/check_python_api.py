"""Python 包对外接口的约定，不满足时列出并以 1 退出。

- 说明齐全：导出的每个类、它的属性与方法、带数据的枚举的每种取值，以及存根里各类的构造函数（它们的说明只在存根
  里，运行时看不到；枚举取值的构造函数由存根生成器按字段生成，参数即取值类的属性，不查）。
- 两个以上字段的枚举取值只收关键字参数：同类型的字段（如几个下标）按位置写反时不报错。
"""

import ast
import inspect
import sys
from pathlib import Path

import hs_m3u8

STUB = Path(hs_m3u8.__file__).parent / "_native" / "__init__.pyi"


def undocumented(owner: type, path: str) -> list[str]:
    """`owner` 与它的公开成员里没有说明的，按 `path.成员` 列出；嵌套的类（枚举的取值）一并检查。"""
    missing = [] if (owner.__doc__ or "").strip() else [path]
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
    problems += [
        f"两个以上字段能按位置传：{item}" for name, owner in exported for item in positional_variants(owner, name)
    ]
    for problem in problems:
        sys.stderr.write(problem + "\n")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
