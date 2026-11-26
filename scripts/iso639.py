"""由美国国会图书馆的 ISO 639-2 代码表重新生成 crates/remux/src/iso639.rs 里的 CODES。

用法：python scripts/iso639.py <ISO-639-2_utf-8.txt>，就地替换 iso639.rs 里 `const CODES` 那一段。代码表下载自
https://www.loc.gov/standards/iso639-2/ISO-639-2_utf-8.txt，每行为 `书目代码|术语代码|两字母代码|英文名|法文名`，
术语代码与书目代码相同时为空；保留区间 qaa-qtz 不收。
"""

import re
import sys
from pathlib import Path

TARGET = Path(__file__).resolve().parent.parent / "crates/remux/src/iso639.rs"

# 代码表每行的字段数
FIELDS = 5


def codes(table: Path) -> list[tuple[str, str, str]]:
    """代码表里的 (书目代码, 术语代码, 两字母代码)，按书目代码排序。"""
    rows: list[tuple[str, str, str]] = []
    for line in table.read_text(encoding="utf-8-sig").splitlines():
        fields = line.split("|")
        if len(fields) < FIELDS or "-" in fields[0]:
            continue
        rows.append((fields[0], fields[1] or fields[0], fields[2]))
    return sorted(rows)


def main() -> None:
    """替换 TARGET 里的 CODES。"""
    entries = "".join(f'    ("{b}", "{t}", "{a2}"),\n' for b, t, a2 in codes(Path(sys.argv[1])))
    source = TARGET.read_text(encoding="utf-8")
    block = re.compile(r"(const CODES: &\[\(&str, &str, &str\)\] = &\[\n).*?(\];\n)", re.DOTALL)
    if block.search(source) is None:
        sys.exit(f"{TARGET} 里找不到 CODES")
    TARGET.write_text(block.sub(lambda m: m.group(1) + entries + m.group(2), source), encoding="utf-8")


if __name__ == "__main__":
    main()
