"""不运行，只交给 mypy 与 basedpyright：存根里枚举属性的类型是各取值类的并集，按取值写全的 `match` 是穷尽的。

漏写一种取值时，mypy 报函数缺少返回语句，basedpyright 报 `match` 不穷尽（reportMatchNotExhaustive）。
"""

from hs_m3u8 import Leftover, LeftoverKind


def advice(leftover: Leftover) -> str:
    """按残留的种类给出处理建议；每种取值一个分支。"""
    match leftover.kind:
        case LeftoverKind.Removable():
            return "可以删除"
        case LeftoverKind.Displaced(target):
            return f"放回 {target}"
        case LeftoverKind.Installed():
            return "完整可用，留着"
        case LeftoverKind.WorkDir():
            return "检查任务目录"
