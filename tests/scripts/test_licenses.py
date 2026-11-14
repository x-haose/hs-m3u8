"""scripts/licenses.py 认许可证原文（`is_text_of`）：仓库里的许可证文件作正例，相近的许可证与只给链接的文件作反例。"""

from pathlib import Path

import pytest

from licenses import LICENSE_MARKS, is_text_of

ROOT = Path(__file__).resolve().parent.parent.parent

# 只给名字与链接、没有条款的文件（照 granit-parser 1.3.0 包里的 LICENSE 写）
LINK_ONLY = """Code is licensed under either of:
  - [Apache License, Version 2.0](.licenses/Apache) (http://www.apache.org/licenses/LICENSE-2.0)
  - [MIT License](.licenses/MIT) (http://opensource.org/licenses/MIT)
"""
ISC = """Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies."""
ZERO_BSD = """Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES."""
BSD_2 = """Redistribution and use in source and binary forms, with or without modification, are permitted
provided that the following conditions are met:
1. Redistributions of source code must retain the above copyright notice, this list of conditions and
   the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice."""
BSD_3 = (
    BSD_2
    + """
3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse
   or promote products derived from this software without specific prior written permission."""
)
MIT_0 = """Permission is hereby granted, free of charge, to any person obtaining a copy of this
software and associated documentation files (the "Software"), to deal in the Software without
restriction."""


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


@pytest.mark.parametrize(
    ("license_id", "path"),
    [
        ("MIT", "LICENSE"),
        ("Apache-2.0", "third_party/crate-licenses/pyo3-stub-gen-0.23.1/LICENSE-APACHE"),
        ("LGPL-2.1-or-later", "LICENSES/FFmpeg-LGPL-2.1.txt"),
        ("Unicode-DFS-2016", "third_party/crate-licenses/unicode_names2-1.3.0/LICENSE-UNICODE"),
    ],
)
def test_texts_in_the_repository_are_recognized(license_id: str, path: str) -> None:
    assert is_text_of(license_id, read(path))


@pytest.mark.parametrize(
    ("license_id", "path"),
    [
        ("MIT-0", "LICENSE"),
        ("GPL-2.0-only", "LICENSES/FFmpeg-LGPL-2.1.txt"),
        ("MIT", "third_party/crate-licenses/unicode_names2-1.3.0/LICENSE-UNICODE"),
    ],
)
def test_close_licenses_are_told_apart(license_id: str, path: str) -> None:
    assert not is_text_of(license_id, read(path))


def test_names_and_links_are_not_the_text() -> None:
    assert not is_text_of("Apache-2.0", LINK_ONLY)
    assert not is_text_of("MIT", LINK_ONLY)


def test_permissive_variants_are_told_apart() -> None:
    assert (is_text_of("ISC", ISC), is_text_of("0BSD", ISC)) == (True, False)
    assert (is_text_of("0BSD", ZERO_BSD), is_text_of("ISC", ZERO_BSD)) == (True, False)
    assert (is_text_of("BSD-2-Clause", BSD_2), is_text_of("BSD-3-Clause", BSD_2)) == (True, False)
    assert (is_text_of("BSD-3-Clause", BSD_3), is_text_of("BSD-2-Clause", BSD_3)) == (True, False)
    assert (is_text_of("MIT-0", MIT_0), is_text_of("MIT", MIT_0)) == (True, False)


def test_licenses_written_as_comments_are_recognized() -> None:
    commented = "\n".join(f"// {line}" for line in ISC.splitlines())
    assert is_text_of("ISC", commented)


def test_exceptions_use_the_base_license_and_unknown_ids_never_match() -> None:
    apache = read("third_party/crate-licenses/pyo3-stub-gen-0.23.1/LICENSE-APACHE")
    assert is_text_of("Apache-2.0 WITH LLVM-exception", apache)
    assert "LicenseRef-Unknown" not in LICENSE_MARKS
    assert not is_text_of("LicenseRef-Unknown", read("LICENSE"))
