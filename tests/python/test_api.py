"""对外接口的说明是否齐全：导出的每个类、它的属性与方法、带数据的枚举的每种取值都要有说明。"""

import hs_m3u8


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


def test_every_public_name_is_documented() -> None:
    missing = [item for name in hs_m3u8.__all__ for item in undocumented(getattr(hs_m3u8, name), name)]
    assert missing == []
