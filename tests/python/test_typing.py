"""存根里枚举属性的类型是各取值类的并集，按取值写全的 `match` 是穷尽的。

漏写一种取值时，mypy 报函数缺少返回语句，basedpyright 报 `match` 不穷尽（reportMatchNotExhaustive）。
"""

from pathlib import Path

from hs_m3u8 import Leftover, LeftoverKind


def advice(leftover: Leftover) -> str:
    match leftover.kind:
        case LeftoverKind.Removable():
            return "可以删除"
        case LeftoverKind.Displaced(target):
            return f"放回 {target}"
        case LeftoverKind.Installed():
            return "完整可用，留着"
        case LeftoverKind.WorkDir():
            return "检查任务目录"


def test_enum_attributes_can_be_matched_exhaustively(tmp_path: Path) -> None:
    target = tmp_path / "a.mp4"
    displaced = Leftover(path=tmp_path / "a.old", kind=LeftoverKind.Displaced(target), cause="原处已被占用")

    assert advice(displaced) == f"放回 {target}"
